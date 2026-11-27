//! Authentification par jeton Bearer (empreintes SHA-256) et filtrage par IP.

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use ipnet::IpNet;
use sha2::{Digest, Sha256};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;

/// Identité du client, injectée dans les extensions de la requête HTTP.
#[derive(Clone, Debug)]
pub struct Client {
    pub name: String,
    pub ip: String,
}

pub struct AuthState {
    /// (nom, empreinte sha256 brute)
    tokens: Vec<(String, [u8; 32])>,
    allowed_ips: Vec<IpNet>,
}

impl AuthState {
    pub fn new(
        tokens: &[crate::config::TokenConfig],
        allowed_ips: Vec<IpNet>,
    ) -> anyhow::Result<Self> {
        let tokens = tokens
            .iter()
            .map(|t| {
                let mut h = [0u8; 32];
                hex::decode_to_slice(&t.sha256, &mut h)?;
                Ok((t.name.clone(), h))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            tokens,
            allowed_ips,
        })
    }

    fn identify(&self, presented: &str) -> Option<&str> {
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        // On parcourt toute la liste pour un temps indépendant du jeton trouvé.
        let mut found = None;
        for (name, h) in &self.tokens {
            if bool::from(digest.ct_eq(h)) {
                found = Some(name.as_str());
            }
        }
        found
    }
}

pub async fn middleware(
    State(state): State<Arc<AuthState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut req: Request,
    next: Next,
) -> Response {
    let ip = peer.ip();
    if !state.allowed_ips.is_empty() && !state.allowed_ips.iter().any(|n| n.contains(&ip)) {
        tracing::warn!(%ip, "connexion refusée : IP hors liste");
        return (StatusCode::FORBIDDEN, "forbidden\n").into_response();
    }

    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token);

    let Some(name) = presented.and_then(|t| state.identify(t)) else {
        tracing::warn!(%ip, "authentification refusée");
        // Ralentit les essais en force brute.
        tokio::time::sleep(Duration::from_millis(500)).await;
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized\n",
        )
            .into_response();
    };

    req.extensions_mut().insert(Client {
        name: name.to_string(),
        ip: ip.to_string(),
    });
    next.run(req).await
}

/// Génère un jeton aléatoire (32 octets, hex) et son empreinte SHA-256.
pub fn generate_token() -> anyhow::Result<(String, String)> {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| anyhow::anyhow!("getrandom : {e}"))?;
    let token = hex::encode(raw);
    let hash = hex::encode(Sha256::digest(token.as_bytes()));
    Ok((token, hash))
}

/// Extrait le jeton d'un en-tête `Authorization: Bearer <jeton>`
/// (schéma insensible à la casse, RFC 9110 §11.1).
fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.trim().split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TokenConfig;

    #[test]
    fn bearer_parsing() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer  abc "), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("Bearerabc"), None);
    }

    #[test]
    fn generated_token_is_identified() {
        let (token, hash) = generate_token().unwrap();
        let cfg = [TokenConfig {
            name: "client".into(),
            sha256: hash,
        }];
        let state = AuthState::new(&cfg, Vec::new()).unwrap();
        assert_eq!(state.identify(&token), Some("client"));
        assert_eq!(state.identify("mauvais"), None);
    }
}
