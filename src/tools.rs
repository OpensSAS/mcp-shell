//! Outils MCP exposés : `run_command` et `get_policy`.

use crate::{audit::{self, Audit, AuditEvent}, auth::Client, config::ExecConfig, exec, policy::Policy};
use axum::http::request::Parts;
use rmcp::{
    handler::server::{router::tool::ToolRouter, tool::Extension, wrapper::{Json, Parameters}},
    model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router, ErrorData, ServerHandler,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{path::PathBuf, sync::Arc, time::Duration};

pub struct Shared {
    pub server_name: String,
    pub hostname: String,
    pub exec: ExecConfig,
    pub policy: Policy,
    pub audit: Audit,
}

#[derive(Clone)]
pub struct ShellServer {
    shared: Arc<Shared>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunParams {
    /// Commande à exécuter (interprétée par le shell configuré). Doit respecter la liste blanche.
    pub command: String,
    /// Répertoire de travail absolu (optionnel, défaut : celui de la configuration).
    pub workdir: Option<String>,
    /// Timeout en secondes (optionnel, plafonné par la configuration).
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PolicyInfo {
    pub server_name: String,
    pub hostname: String,
    /// Regex des commandes autorisées (correspondance complète, segment par segment).
    pub allow: Vec<String>,
    /// Regex des motifs toujours refusés.
    pub deny: Vec<String>,
    pub allow_all: bool,
    pub allow_redirections: bool,
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    pub max_output_bytes: usize,
    pub rules: String,
}

impl ShellServer {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self { shared, tool_router: Self::tool_router() }
    }
}

fn client_of(parts: &Parts) -> (String, String) {
    parts
        .extensions
        .get::<Client>()
        .map(|c| (c.name.clone(), c.ip.clone()))
        .unwrap_or_else(|| ("inconnu".into(), "?".into()))
}

#[tool_router]
impl ShellServer {
    #[tool(
        name = "run_command",
        annotations(title = "Exécuter une commande", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false),
        description = "Exécute une commande shell sur ce serveur et renvoie stdout, stderr et le code de retour. \
                       Seules les commandes de la liste blanche sont acceptées : appelez get_policy pour la connaître. \
                       Chaque segment séparé par | ; && || doit être autorisé ; $(...), backticks et redirections sont refusés."
    )]
    async fn run_command(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<RunParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let s = &self.shared;
        let (client, ip) = client_of(&parts);
        let workdir = p.workdir.as_deref().map(PathBuf::from).unwrap_or_else(|| s.exec.workdir.clone());
        let wd = workdir.display().to_string();

        let refusal = if !workdir.is_absolute() {
            Some("workdir doit être un chemin absolu".to_string())
        } else {
            s.policy.check(&p.command).err()
        };
        if let Some(reason) = refusal {
            s.audit
                .write(AuditEvent {
                    ts: audit::now(), client: &client, ip: &ip, tool: "run_command",
                    command: Some(&p.command), workdir: Some(&wd), outcome: "denied",
                    reason: Some(&reason), exit_code: None, timed_out: None, duration_ms: None,
                })
                .await;
            tracing::warn!(client = %client, command = %p.command, %reason, "commande refusée");
            return Ok(CallToolResult::structured_error(json!({ "denied": true, "reason": reason })));
        }

        let timeout = p
            .timeout_secs
            .unwrap_or(s.exec.default_timeout_secs)
            .clamp(1, s.exec.max_timeout_secs);

        match exec::run(&s.exec, &p.command, &workdir, Duration::from_secs(timeout)).await {
            Ok(out) => {
                s.audit
                    .write(AuditEvent {
                        ts: audit::now(), client: &client, ip: &ip, tool: "run_command",
                        command: Some(&p.command), workdir: Some(&wd), outcome: "executed",
                        reason: None, exit_code: out.exit_code, timed_out: Some(out.timed_out),
                        duration_ms: Some(out.duration_ms),
                    })
                    .await;
                tracing::info!(client = %client, command = %p.command, exit = ?out.exit_code, "commande exécutée");
                let value = serde_json::to_value(&out).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
                Ok(CallToolResult::structured(value))
            }
            Err(e) => {
                let reason = format!("lancement impossible : {e}");
                s.audit
                    .write(AuditEvent {
                        ts: audit::now(), client: &client, ip: &ip, tool: "run_command",
                        command: Some(&p.command), workdir: Some(&wd), outcome: "error",
                        reason: Some(&reason), exit_code: None, timed_out: None, duration_ms: None,
                    })
                    .await;
                Ok(CallToolResult::structured_error(json!({ "error": reason })))
            }
        }
    }

    #[tool(
        name = "get_policy",
        annotations(title = "Politique d'exécution", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false),
        description = "Renvoie le nom du serveur et la politique d'exécution : commandes autorisées (regex), \
                       motifs interdits, limites de temps et de taille de sortie. À appeler avant run_command."
    )]
    async fn get_policy(&self) -> Json<PolicyInfo> {
        let s = &self.shared;
        Json(PolicyInfo {
            server_name: s.server_name.clone(),
            hostname: s.hostname.clone(),
            allow: s.policy.allow_src.clone(),
            deny: s.policy.deny_src.clone(),
            allow_all: s.exec.allow_all,
            allow_redirections: s.exec.allow_redirections,
            default_timeout_secs: s.exec.default_timeout_secs,
            max_timeout_secs: s.exec.max_timeout_secs,
            max_output_bytes: s.exec.max_output_bytes,
            rules: "Chaque segment (séparé par | ; && || & ou retour ligne) doit correspondre entièrement \
                    à une regex de 'allow'. Les motifs 'deny' sont testés sur la commande entière et priment. \
                    $(...), backticks, <(...) et >(...) sont toujours refusés."
                .into(),
        })
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ShellServer {
    fn get_info(&self) -> ServerConfig {
        let s = &self.shared;
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mcp-shell", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "Serveur '{}' (hôte {}). Accès shell restreint par liste blanche : appelez get_policy \
                 pour connaître les commandes autorisées, puis run_command. Toute commande est journalisée.",
                s.server_name, s.hostname
            ))
    }
}
