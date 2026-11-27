//! Exécution d'une commande shell avec timeout, sortie plafonnée et
//! arrêt du groupe de processus complet.
//!
//! Le timeout est une échéance unique qui couvre toute l'exécution : l'attente du
//! shell ET la lecture de stdout/stderr. Un processus qui garderait les tubes ouverts
//! (lancé en arrière-plan, détaché par `setsid`…) ne peut donc pas bloquer la requête
//! au-delà du timeout.

use crate::config::ExecConfig;
use serde::Serialize;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::Instant,
};

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExecOutput {
    /// Code de retour du shell (absent s'il a été tué par un signal ou par le timeout).
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// true si stdout ou stderr a été tronqué à max_output_bytes.
    pub truncated: bool,
    /// true si l'échéance a été atteinte (commande trop longue, ou sortie encore
    /// ouverte par un processus détaché).
    pub timed_out: bool,
    pub duration_ms: u128,
}

pub async fn run(
    cfg: &ExecConfig,
    command: &str,
    workdir: &Path,
    timeout: Duration,
) -> std::io::Result<ExecOutput> {
    let start = Instant::now();
    let deadline = start + timeout;
    let mut cmd = Command::new(&cfg.shell);
    cmd.arg("-c")
        .arg(command)
        .current_dir(workdir)
        .env_clear()
        .env("PATH", &cfg.path)
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    }

    let mut child = cmd.spawn()?;
    let pgid = child.id().map(|p| p as libc::pid_t);
    let limit = cfg.max_output_bytes;
    let out_task = tokio::spawn(read_capped(child.stdout.take(), limit, deadline));
    let err_task = tokio::spawn(read_capped(child.stderr.take(), limit, deadline));

    let waited = tokio::time::timeout_at(deadline, child.wait()).await;

    // Que le shell ait fini ou non, rien de son groupe ne doit survivre à la requête.
    // Si le shell vient d'être récupéré, le numéro de groupe reste réservé par le noyau
    // tant qu'un membre du groupe existe ; s'il n'en reste aucun, kill renvoie ESRCH.
    if let Some(pgid) = pgid {
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }

    let status = match waited {
        Ok(res) => Some(res?),
        Err(_) => {
            let _ = child.wait().await;
            None
        }
    };

    let (stdout, trunc_out, late_out) = out_task.await.unwrap_or_default();
    let (stderr, trunc_err, late_err) = err_task.await.unwrap_or_default();

    Ok(ExecOutput {
        exit_code: status.and_then(|s| s.code()),
        stdout,
        stderr,
        truncated: trunc_out || trunc_err,
        timed_out: status.is_none() || late_out || late_err,
        duration_ms: start.elapsed().as_millis(),
    })
}

/// Lit le flux jusqu'à sa fin ou jusqu'à l'échéance, en ne gardant que `limit` octets
/// (le reste est lu et jeté pour ne jamais bloquer l'écrivain).
/// Renvoie (texte, tronqué, échéance atteinte).
async fn read_capped<R: AsyncRead + Unpin>(
    reader: Option<R>,
    limit: usize,
    deadline: Instant,
) -> (String, bool, bool) {
    let Some(mut reader) = reader else {
        return (String::new(), false, false);
    };
    let mut kept = Vec::with_capacity(limit.min(8192));
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    let mut late = false;
    loop {
        match tokio::time::timeout_at(deadline, reader.read(&mut buf)).await {
            Err(_) => {
                late = true;
                break;
            }
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(n)) => {
                let room = limit.saturating_sub(kept.len());
                if room > 0 {
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
                if n > room {
                    truncated = true;
                }
            }
        }
    }
    (String::from_utf8_lossy(&kept).into_owned(), truncated, late)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn sh(command: &str, timeout_secs: u64) -> ExecOutput {
        let cfg = ExecConfig::for_tests();
        run(
            &cfg,
            command,
            Path::new("/"),
            Duration::from_secs(timeout_secs),
        )
        .await
        .unwrap()
    }

    fn process_alive(marker: &str) -> bool {
        std::process::Command::new("pgrep")
            .args(["-f", marker])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let out = sh("echo out; echo err >&2; exit 3", 5).await;
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.stderr, "err\n");
        assert_eq!(out.exit_code, Some(3));
        assert!(!out.timed_out && !out.truncated);
    }

    #[tokio::test]
    async fn environment_is_cleared() {
        // Seules les variables fixées par run() (et celles que le shell pose lui-même)
        // doivent être visibles : rien n'est hérité du service.
        let out = sh("env", 5).await;
        let allowed = [
            "PATH", "LANG", "LC_ALL", "TERM", "HOME", "PWD", "OLDPWD", "SHLVL", "_",
        ];
        for line in out.stdout.lines() {
            let key = line.split('=').next().unwrap_or_default();
            assert!(allowed.contains(&key), "variable héritée : {line}");
        }
        assert!(out.stdout.contains("LC_ALL=C.UTF-8"));
    }

    #[tokio::test]
    async fn timeout_kills_command() {
        let out = sh("sleep 30", 1).await;
        assert!(out.timed_out);
        assert_eq!(out.exit_code, None);
        assert!(out.duration_ms < 5_000);
    }

    #[tokio::test]
    async fn background_job_does_not_outlive_request() {
        // Régression 0.1.1 : `sleep N &` gardait stdout ouvert et bloquait la requête
        // N secondes, sans timed_out.
        let out = sh("sleep 31 & echo fini", 10).await;
        assert_eq!(out.stdout, "fini\n");
        assert_eq!(out.exit_code, Some(0));
        assert!(!out.timed_out);
        assert!(out.duration_ms < 5_000, "durée {} ms", out.duration_ms);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !process_alive("sleep 31"),
            "le processus d'arrière-plan a survécu"
        );
    }

    #[tokio::test]
    async fn detached_process_cannot_exceed_deadline() {
        // setsid sort du groupe de processus : il survit au kill du groupe, mais la
        // requête rend la main à l'échéance. Le `sleep 0.3` laisse à setsid le temps de
        // se détacher avant que le shell ne se termine.
        let out = sh("setsid sleep 6.123 & sleep 0.3; echo lance", 1).await;
        let _ = std::process::Command::new("pkill")
            .args(["-f", "sleep 6.123"])
            .status();
        assert_eq!(out.stdout, "lance\n");
        assert_eq!(out.exit_code, Some(0));
        assert!(out.timed_out);
        assert!(out.duration_ms < 4_000, "durée {} ms", out.duration_ms);
    }

    #[tokio::test]
    async fn output_is_capped() {
        let out = sh("head -c 5000 /dev/zero | tr '\\0' x", 5).await;
        assert_eq!(out.stdout.len(), 1024);
        assert!(out.truncated);
        assert_eq!(out.exit_code, Some(0));
    }
}
