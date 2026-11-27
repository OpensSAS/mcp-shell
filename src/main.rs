mod audit;
mod auth;
mod config;
mod exec;
mod policy;
mod tools;

use anyhow::{Context, Result};
use axum::{middleware, routing::get, Router};
use axum_server::{tls_rustls::RustlsConfig, Handle};
use clap::{Parser, Subcommand};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

#[derive(Parser)]
#[command(
    name = "mcp-shell",
    version,
    about = "Serveur MCP d'exécution shell sous liste blanche (HTTP streamable, TLS, jetons)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Démarre le serveur.
    Serve {
        #[arg(short, long, default_value = "/etc/mcp-shell/config.toml")]
        config: PathBuf,
    },
    /// Génère un jeton et son empreinte SHA-256 à coller dans la configuration.
    GenToken,
    /// Valide la configuration, et teste éventuellement une commande contre la politique.
    Check {
        #[arg(short, long, default_value = "/etc/mcp-shell/config.toml")]
        config: PathBuf,
        /// Commande à tester (sans l'exécuter).
        #[arg(long)]
        command: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mcp_shell=info,warn".into()),
        )
        .with_target(false)
        .init();

    match Cli::parse().cmd {
        Cmd::GenToken => {
            let (token, hash) = auth::generate_token()?;
            println!("Jeton (à donner au client, ne sera plus affiché) :\n  {token}\n");
            println!("À ajouter dans la configuration :\n[[tokens]]\nname = \"paperclip\"\nsha256 = \"{hash}\"");
            Ok(())
        }
        Cmd::Check { config, command } => {
            let cfg = config::Config::load(&config)?;
            let policy = policy::Policy::new(&cfg.exec)?;
            println!("Configuration valide : {}", config.display());
            if let Some(c) = command {
                match policy.check(&c) {
                    Ok(()) => println!("AUTORISÉE : {c}"),
                    Err(r) => {
                        println!("REFUSÉE : {c}\n  raison : {r}");
                        std::process::exit(2);
                    }
                }
            }
            Ok(())
        }
        Cmd::Serve { config } => serve(config).await,
    }
}

async fn serve(config_path: PathBuf) -> Result<()> {
    let cfg = config::Config::load(&config_path)?;
    let policy = policy::Policy::new(&cfg.exec)?;
    let auth_state = Arc::new(auth::AuthState::new(&cfg.tokens, cfg.allowed_ips.clone())?);

    if cfg.exec.allow_all {
        tracing::warn!("allow_all = true : TOUTE commande est acceptée (hors liste deny)");
    }
    if unsafe { libc::geteuid() } == 0 {
        tracing::warn!("le service tourne en root : utilisez un utilisateur dédié et sudoers");
    }

    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "inconnu".into());

    let shared = Arc::new(tools::Shared {
        server_name: cfg.server_name.clone(),
        hostname,
        exec: cfg.exec,
        policy,
        audit: audit::Audit::new(cfg.audit_log.clone()),
    });

    let mut http_cfg = StreamableHttpServerConfig::default();
    http_cfg = if cfg.allowed_hosts.is_empty() {
        http_cfg.disable_allowed_hosts()
    } else {
        http_cfg.with_allowed_hosts(cfg.allowed_hosts.clone())
    };

    let svc_shared = shared.clone();
    let mcp = StreamableHttpService::new(
        move || Ok(tools::ShellServer::new(svc_shared.clone())),
        LocalSessionManager::default().into(),
        http_cfg,
    );

    let app = Router::new()
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(auth_state, auth::middleware))
        .route("/health", get(|| async { "ok\n" }));
    let make = app.into_make_service_with_connect_info::<SocketAddr>();

    let handle = Handle::new();
    tokio::spawn(shutdown_on_signal(handle.clone()));

    match &cfg.tls {
        Some(tls) => {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let rustls_cfg = RustlsConfig::from_pem_file(&tls.cert, &tls.key)
                .await
                .with_context(|| {
                    format!(
                        "chargement TLS ({} / {})",
                        tls.cert.display(),
                        tls.key.display()
                    )
                })?;
            tokio::spawn(reload_tls(
                rustls_cfg.clone(),
                tls.cert.clone(),
                tls.key.clone(),
                Duration::from_secs(tls.reload_interval_secs.max(60)),
            ));
            tracing::info!("écoute HTTPS sur {} (endpoint /mcp)", cfg.listen);
            axum_server::bind_rustls(cfg.listen, rustls_cfg)
                .handle(handle)
                .serve(make)
                .await?;
        }
        None => {
            tracing::warn!(
                "écoute en HTTP clair sur {} : à réserver à un reverse proxy local",
                cfg.listen
            );
            axum_server::bind(cfg.listen)
                .handle(handle)
                .serve(make)
                .await?;
        }
    }
    Ok(())
}

async fn reload_tls(cfg: RustlsConfig, cert: PathBuf, key: PathBuf, every: Duration) {
    let mtime = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut last = (mtime(&cert), mtime(&key));
    loop {
        tokio::time::sleep(every).await;
        let now = (mtime(&cert), mtime(&key));
        if now != last {
            match cfg.reload_from_pem_file(&cert, &key).await {
                Ok(()) => {
                    tracing::info!("certificat TLS rechargé");
                    last = now;
                }
                Err(e) => tracing::error!("rechargement TLS impossible : {e}"),
            }
        }
    }
}

async fn shutdown_on_signal(handle: Handle<SocketAddr>) {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("signal SIGTERM");
    tokio::select! {
        _ = term.recv() => {},
        _ = tokio::signal::ctrl_c() => {},
    }
    tracing::info!("arrêt demandé");
    handle.graceful_shutdown(Some(Duration::from_secs(10)));
}
