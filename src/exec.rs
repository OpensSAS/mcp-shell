//! Exécution d'une commande shell avec timeout, sortie plafonnée et
//! arrêt du groupe de processus complet en cas de dépassement.

use crate::config::ExecConfig;
use serde::Serialize;
use std::{path::Path, process::Stdio, time::{Duration, Instant}};
use tokio::{io::{AsyncRead, AsyncReadExt}, process::Command};

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExecOutput {
    /// Code de retour (absent si tué par un signal ou par le timeout).
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// true si stdout ou stderr a été tronqué à max_output_bytes.
    pub truncated: bool,
    pub timed_out: bool,
    pub duration_ms: u128,
}

pub async fn run(cfg: &ExecConfig, command: &str, workdir: &Path, timeout: Duration) -> std::io::Result<ExecOutput> {
    let start = Instant::now();
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
    let pid = child.id();
    let limit = cfg.max_output_bytes;
    let out_task = tokio::spawn(read_capped(child.stdout.take(), limit));
    let err_task = tokio::spawn(read_capped(child.stderr.take(), limit));

    let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(res) => (Some(res?), false),
        Err(_) => {
            if let Some(pid) = pid {
                // Tue tout le groupe : le shell et ses enfants.
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
            let _ = child.wait().await;
            (None, true)
        }
    };

    let (stdout, t1) = out_task.await.unwrap_or_default();
    let (stderr, t2) = err_task.await.unwrap_or_default();

    Ok(ExecOutput {
        exit_code: status.and_then(|s| s.code()),
        stdout,
        stderr,
        truncated: t1 || t2,
        timed_out,
        duration_ms: start.elapsed().as_millis(),
    })
}

/// Lit tout le flux (pour ne jamais bloquer le processus) mais ne garde que `limit` octets.
async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>, limit: usize) -> (String, bool) {
    let Some(mut reader) = reader else { return (String::new(), false) };
    let mut kept = Vec::with_capacity(limit.min(8192));
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
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
    (String::from_utf8_lossy(&kept).into_owned(), truncated)
}
