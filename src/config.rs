//! Chargement et validation de la configuration TOML.

use anyhow::{bail, Context, Result};
use ipnet::IpNet;
use serde::Deserialize;
use std::{net::SocketAddr, path::{Path, PathBuf}};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Adresse d'écoute, ex. "0.0.0.0:8443".
    pub listen: SocketAddr,
    /// Nom lisible du serveur, annoncé à l'agent (ex. "pve-opens").
    pub server_name: String,
    /// Valeurs acceptées pour l'en-tête Host (protection DNS rebinding).
    /// Liste vide = contrôle désactivé.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Réseaux autorisés à se connecter. Liste vide = toutes les IP.
    #[serde(default)]
    pub allowed_ips: Vec<IpNet>,
    /// Journal d'audit JSON lines.
    pub audit_log: PathBuf,
    /// Autorise le HTTP en clair quand [tls] est absent (derrière un reverse proxy local).
    #[serde(default)]
    pub allow_plain_http: bool,
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub tokens: Vec<TokenConfig>,
    pub exec: ExecConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert: PathBuf,
    pub key: PathBuf,
    /// Intervalle de rechargement du certificat (renouvellement Let's Encrypt sans redémarrage).
    #[serde(default = "default_reload")]
    pub reload_interval_secs: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenConfig {
    /// Nom du client (apparaît dans l'audit).
    pub name: String,
    /// SHA-256 hexadécimal du jeton (généré par `mcp-shell gen-token`).
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecConfig {
    #[serde(default = "default_shell")]
    pub shell: PathBuf,
    #[serde(default = "default_workdir")]
    pub workdir: PathBuf,
    #[serde(default = "default_path")]
    pub path: String,
    #[serde(default = "default_timeout")]
    pub default_timeout_secs: u64,
    #[serde(default = "default_max_timeout")]
    pub max_timeout_secs: u64,
    #[serde(default = "default_max_output")]
    pub max_output_bytes: usize,
    /// Désactive totalement la liste blanche. Dangereux : à réserver aux tests.
    #[serde(default)]
    pub allow_all: bool,
    /// Autorise les redirections > < >> (écriture/lecture de fichiers par le shell).
    #[serde(default)]
    pub allow_redirections: bool,
    /// Regex (correspondance complète, ancrage automatique) des commandes autorisées.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Regex (recherche partielle) des motifs toujours refusés, prioritaires.
    #[serde(default)]
    pub deny: Vec<String>,
}

fn default_reload() -> u64 { 3600 }
fn default_shell() -> PathBuf { "/bin/sh".into() }
fn default_workdir() -> PathBuf { "/".into() }
fn default_path() -> String { "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into() }
fn default_timeout() -> u64 { 60 }
fn default_max_timeout() -> u64 { 600 }
fn default_max_output() -> usize { 64 * 1024 }

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("lecture de {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("analyse de {}", path.display()))?;
        cfg.validate()?;
        check_permissions(path);
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.tls.is_none() && !self.allow_plain_http {
            bail!("pas de section [tls] : ajoutez-la, ou mettez allow_plain_http = true \
                   (uniquement derrière un reverse proxy qui écoute en local)");
        }
        if self.tokens.is_empty() {
            bail!("aucun jeton [[tokens]] défini : personne ne pourrait se connecter");
        }
        for t in &self.tokens {
            let ok = t.sha256.len() == 64 && t.sha256.chars().all(|c| c.is_ascii_hexdigit());
            if !ok {
                bail!("jeton '{}' : sha256 doit faire 64 caractères hexadécimaux", t.name);
            }
            if t.sha256.chars().all(|c| c == '0') {
                bail!("jeton '{}' : l'empreinte vaut encore 000…0 (valeur d'exemple). \
                       Lancez `mcp-shell gen-token` et collez la valeur sha256 affichée", t.name);
            }
        }
        if !self.exec.allow_all && self.exec.allow.is_empty() {
            bail!("[exec].allow est vide : aucune commande ne serait autorisée \
                   (ou allow_all = true, déconseillé)");
        }
        if self.exec.default_timeout_secs == 0 || self.exec.default_timeout_secs > self.exec.max_timeout_secs {
            bail!("[exec] : il faut 0 < default_timeout_secs <= max_timeout_secs");
        }
        if !self.exec.shell.is_absolute() || !self.exec.workdir.is_absolute() {
            bail!("[exec] : shell et workdir doivent être des chemins absolus");
        }
        Ok(())
    }
}

/// Avertit si la config (qui contient les empreintes de jetons et la politique) est lisible par tous.
fn check_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.permissions().mode() & 0o002 != 0 {
            tracing::warn!("{} est modifiable par tous : n'importe quel utilisateur pourrait élargir la liste blanche", path.display());
        }
    }
}
