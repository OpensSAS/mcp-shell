//! Journal d'audit en JSON lines (une ligne par appel d'outil).

use serde::Serialize;
use std::path::PathBuf;
use tokio::{fs::OpenOptions, io::AsyncWriteExt, sync::Mutex};

pub struct Audit {
    path: PathBuf,
    lock: Mutex<()>,
}

#[derive(Serialize)]
pub struct AuditEvent<'a> {
    pub ts: String,
    pub client: &'a str,
    pub ip: &'a str,
    pub tool: &'a str,
    pub command: Option<&'a str>,
    pub workdir: Option<&'a str>,
    /// "executed", "denied" ou "error"
    pub outcome: &'a str,
    pub reason: Option<&'a str>,
    pub exit_code: Option<i32>,
    pub timed_out: Option<bool>,
    pub duration_ms: Option<u128>,
}

impl Audit {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lock: Mutex::new(()) }
    }

    pub async fn write(&self, ev: AuditEvent<'_>) {
        let line = match serde_json::to_string(&ev) {
            Ok(l) => l + "\n",
            Err(e) => {
                tracing::error!("audit : sérialisation impossible : {e}");
                return;
            }
        };
        let _guard = self.lock.lock().await;
        let res = async {
            let mut f = OpenOptions::new().create(true).append(true).open(&self.path).await?;
            f.write_all(line.as_bytes()).await
        }
        .await;
        if let Err(e) = res {
            // L'échec d'audit est journalisé mais ne bloque pas le service.
            tracing::error!("audit : écriture dans {} impossible : {e}", self.path.display());
        }
    }
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
