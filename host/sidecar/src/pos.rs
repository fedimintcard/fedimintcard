//! Pairing a POS phone with the till, and the bearer token that guards the till API off loopback
//! (`docs/POS-LINK.md`, "Pairing and auth").
//!
//! The token is 32 random bytes, hex, in `<data_dir>/till-pos-token` (mode 0600). It is compared
//! in constant time and never logged.

use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

pub const TOKEN_FILE: &str = "till-pos-token";
pub const PAIR_SCHEME: &str = "fedimintcard-till:";

pub fn token_path(data_dir: &Path) -> PathBuf {
    data_dir.join(TOKEN_FILE)
}

/// The POS token, if one has been made. An empty file counts as none.
pub fn read_token(data_dir: &Path) -> Result<Option<String>> {
    let path = token_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let t = s.trim().to_owned();
            Ok((!t.is_empty()).then_some(t))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The POS token, made now (0600) if there is none. The flag says whether it was made.
pub fn ensure_token(data_dir: &Path) -> Result<(String, bool)> {
    if let Some(t) = read_token(data_dir)? {
        return Ok((t, false));
    }
    std::fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let path = token_path(data_dir);
    let token = hex::encode(rand::random::<[u8; 32]>());
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        // An empty file left by someone else keeps its mode through `open`; set it explicitly.
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(format!("{token}\n").as_bytes())?;
    f.sync_all()?;
    Ok((token, true))
}

/// The address the phone should use: the first non-loopback IPv4 address, taken as the one the
/// default route leaves from (a UDP `connect` sends nothing).
pub fn lan_ipv4() -> Result<std::net::Ipv4Addr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0")?;
    s.connect("192.0.2.1:9")?;
    match s.local_addr()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Ok(v4),
        other => bail!("no LAN IPv4 address (got {other}); pass --url"),
    }
}

/// `fedimintcard-till:<url>?t=<token>`
pub fn pair_text(url: &str, token: &str) -> String {
    format!("{PAIR_SCHEME}{}?t={token}", url.trim_end_matches('/'))
}

/// Constant-time equality (the length is not secret).
pub fn token_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, axum::Json(json!({"ok": false, "error": "unauthorized"}))).into_response()
}

/// Is this request allowed? Loopback peers always are; others need the bearer when a token is
/// configured. A request with no peer address (none recorded) is not treated as loopback.
pub fn authorized(token: Option<&str>, peer: Option<SocketAddr>, authorization: Option<&str>) -> bool {
    let Some(token) = token else { return true };
    if peer.is_some_and(|p| p.ip().is_loopback() || p.ip().to_canonical().is_loopback()) {
        return true;
    }
    let Some(given) = authorization.and_then(|h| h.strip_prefix("Bearer ")) else { return false };
    token_eq(given.trim().as_bytes(), token.as_bytes())
}

/// Middleware over `/v1/*`: [`authorized`] with the handle's token and the peer's address.
pub async fn auth(State(token): State<Option<std::sync::Arc<str>>>, req: Request, next: Next) -> Response {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    let header = req.headers().get(axum::http::header::AUTHORIZATION).and_then(|h| h.to_str().ok());
    if authorized(token.as_deref(), peer, header) {
        next.run(req).await
    } else {
        unauthorized()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_rules() {
        let lan: SocketAddr = "192.168.1.20:5000".parse().unwrap();
        let lo: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let lo6: SocketAddr = "[::ffff:127.0.0.1]:5000".parse().unwrap();
        assert!(authorized(None, Some(lan), None));
        assert!(authorized(Some("ab"), Some(lo), None));
        assert!(authorized(Some("ab"), Some(lo6), None));
        assert!(!authorized(Some("ab"), Some(lan), None));
        assert!(!authorized(Some("ab"), None, None));
        assert!(!authorized(Some("ab"), Some(lan), Some("Bearer ac")));
        assert!(!authorized(Some("ab"), Some(lan), Some("ab")));
        assert!(authorized(Some("ab"), Some(lan), Some("Bearer ab")));
        assert_eq!(pair_text("http://10.0.0.2:9877/", "ff"), "fedimintcard-till:http://10.0.0.2:9877?t=ff");
    }
}
