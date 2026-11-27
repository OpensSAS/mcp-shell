//! Politique d'exécution : liste blanche, liste noire, opérateurs shell.
//!
//! Règles, dans l'ordre :
//! 1. la liste noire (`deny`) est testée sur la commande entière : un motif trouvé = refus ;
//! 2. hors `allow_all`, les substitutions `$(...)`, `` `...` ``, `<(...)`, `>(...)` sont refusées ;
//! 3. hors `allow_redirections`, les redirections `>` et `<` sont refusées ;
//! 4. la mise en arrière-plan (`&` seul) est refusée : un processus détaché
//!    survivrait à la requête et échapperait au timeout ;
//! 5. la commande est découpée sur `;` `&&` `||` `|` `|&` et retour ligne :
//!    CHAQUE segment doit correspondre entièrement à un motif de `allow`.
//!
//! Le découpage est volontairement naïf (il ignore les guillemets) : il ne peut que
//! rendre la vérification plus stricte, jamais plus permissive.

use crate::config::ExecConfig;
use anyhow::{Context, Result};
use regex::Regex;

pub struct Policy {
    allow_all: bool,
    allow_redirections: bool,
    allow: Vec<Regex>,
    deny: Vec<Regex>,
    pub allow_src: Vec<String>,
    pub deny_src: Vec<String>,
}

impl Policy {
    pub fn new(cfg: &ExecConfig) -> Result<Self> {
        let allow = cfg
            .allow
            .iter()
            .map(|p| {
                Regex::new(&format!("^(?:{p})$"))
                    .with_context(|| format!("regex allow invalide : {p}"))
            })
            .collect::<Result<Vec<_>>>()?;
        let deny = cfg
            .deny
            .iter()
            .map(|p| Regex::new(p).with_context(|| format!("regex deny invalide : {p}")))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            allow_all: cfg.allow_all,
            allow_redirections: cfg.allow_redirections,
            allow,
            deny,
            allow_src: cfg.allow.clone(),
            deny_src: cfg.deny.clone(),
        })
    }

    /// Ok(()) si la commande est autorisée, sinon la raison du refus.
    pub fn check(&self, command: &str) -> std::result::Result<(), String> {
        let command = command.trim();
        if command.is_empty() {
            return Err("commande vide".into());
        }
        if command.contains('\0') {
            return Err("caractère NUL interdit".into());
        }
        for (re, src) in self.deny.iter().zip(&self.deny_src) {
            if re.is_match(command) {
                return Err(format!("motif interdit (deny) : {src}"));
            }
        }
        if self.allow_all {
            return Ok(());
        }
        for sub in ["$(", "`", "<(", ">("] {
            if command.contains(sub) {
                return Err(format!("substitution interdite : {sub}"));
            }
        }
        if !self.allow_redirections && (command.contains('>') || command.contains('<')) {
            return Err("redirections > < interdites (allow_redirections = false)".into());
        }
        for segment in split_segments(command)? {
            let seg = segment.trim();
            if seg.is_empty() {
                continue;
            }
            if !self.allow.iter().any(|re| re.is_match(seg)) {
                return Err(format!(
                    "segment non autorisé par la liste blanche : `{seg}`"
                ));
            }
        }
        Ok(())
    }
}

/// Découpe sur `;` `&&` `||` `|` `|&` et retour ligne.
///
/// Un `&` qui fait partie d'une redirection (`2>&1`, `<&`, `&>`, `&>>`) n'est pas un
/// séparateur. Tout autre `&` seul est une mise en arrière-plan : refusée.
fn split_segments(cmd: &str) -> std::result::Result<Vec<String>, String> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let prev = if i > 0 { Some(chars[i - 1]) } else { None };
        match c {
            ';' | '\n' | '\r' => out.push(std::mem::take(&mut cur)),
            '|' => {
                out.push(std::mem::take(&mut cur));
                // `||` (ou logique) et `|&` (tube stdout + stderr)
                if matches!(next, Some('|') | Some('&')) {
                    i += 1;
                }
            }
            '&' if next == Some('&') => {
                out.push(std::mem::take(&mut cur));
                i += 1;
            }
            '&' if matches!(prev, Some('>') | Some('<')) || next == Some('>') => cur.push(c),
            '&' => return Err("mise en arrière-plan `&` interdite".into()),
            _ => cur.push(c),
        }
        i += 1;
    }
    out.push(cur);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allow: &[&str], deny: &[&str], redirections: bool) -> Policy {
        let cfg = ExecConfig {
            allow_redirections: redirections,
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            ..ExecConfig::for_tests()
        };
        Policy::new(&cfg).unwrap()
    }

    #[test]
    fn allowlist_full_match() {
        let p = policy(&[r"pvesm status", r"df -h( \S+)?"], &[], false);
        assert!(p.check("pvesm status").is_ok());
        assert!(p.check("df -h /var").is_ok());
        assert!(p.check("pvesm status; rm -rf /").is_err());
        assert!(p.check("pvesm statusX").is_err());
    }

    #[test]
    fn pipes_each_segment_checked() {
        let p = policy(&[r"pct list", r"grep -i \w+"], &[], false);
        assert!(p.check("pct list | grep -i running").is_ok());
        assert!(p.check("pct list | sh").is_err());
        assert!(p.check("pct list && reboot").is_err());
    }

    #[test]
    fn substitutions_and_redirections() {
        let p = policy(&[r"echo .*"], &[], false);
        assert!(p.check("echo $(id)").is_err());
        assert!(p.check("echo `id`").is_err());
        assert!(p.check("echo x > /etc/passwd").is_err());
        let p = policy(&[r"journalctl .*"], &[], true);
        assert!(p.check("journalctl -u pveproxy 2>&1").is_ok());
    }

    #[test]
    fn deny_wins() {
        let p = policy(&[r".*"], &[r"\brm\b"], false);
        assert!(p.check("rm -rf /").is_err());
        assert!(p.check("ls").is_ok());
    }

    #[test]
    fn background_refused() {
        let p = policy(&[r"sleep \d+", r"hostname"], &[], false);
        assert!(p.check("sleep 3600 &").is_err());
        assert!(p.check("sleep 3600 & hostname").is_err());
        assert!(p.check("sleep 1 && hostname").is_ok());
        assert!(p.check("sleep 1 || hostname").is_ok());
    }

    #[test]
    fn redirection_ampersands_are_not_separators() {
        let p = policy(&[r"journalctl -u \w+( 2>&1)?", r"grep \w+"], &[], true);
        assert!(p.check("journalctl -u pveproxy 2>&1 | grep error").is_ok());
        assert!(p.check("journalctl -u pveproxy |& grep error").is_ok());
        assert_eq!(
            split_segments("a 2>&1 |& b && c || d; e").unwrap(),
            vec!["a 2>&1 ", " b ", " c ", " d", " e"]
        );
        assert!(split_segments("a &> f").is_ok());
        assert!(split_segments("a & b").is_err());
    }

    #[test]
    fn newlines_split() {
        let p = policy(&[r"hostname"], &[], false);
        assert!(p.check("hostname\nhostname").is_ok());
        assert!(p.check("hostname\nreboot").is_err());
        assert!(p.check("hostname\r\nreboot").is_err());
    }
}
