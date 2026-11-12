//! Pairing a POS phone with the till, and the bearer token that guards the till API off loopback
//! (`docs/POS-LINK.md`, "Pairing and auth"); and what a paired phone asks of the till to charge
//! offline (`docs/POS-OFFLINE.md`): its offline kit and the sync of the charges it took.
//!
//! The token is 32 random bytes, hex, in `<data_dir>/till-pos-token` (mode 0600). It is compared
//! in constant time and never logged.

use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::pool::{MAX_INGEST, TillKeys};
use crate::till::{Reply, TillHandle, error_reply};

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
///
/// Two callers at once (a pairing while the till starts) agree on one token: the new token is
/// written whole to a private temporary file and linked into place only if no token is there yet.
pub fn ensure_token(data_dir: &Path) -> Result<(String, bool)> {
    if let Some(t) = read_token(data_dir)? {
        return Ok((t, false));
    }
    std::fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let path = token_path(data_dir);
    let token = new_token();
    let tmp = write_private_temp(data_dir, &token)?;
    // An empty file counts as no token (`read_token`): replace it rather than keep it.
    let placed = match std::fs::hard_link(&tmp, &path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match read_token(data_dir)? {
            Some(_) => false,
            None => {
                std::fs::rename(&tmp, &path).with_context(|| format!("creating {}", path.display()))?;
                return Ok((token, true));
            }
        },
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("creating {}", path.display()));
        }
    };
    let _ = std::fs::remove_file(&tmp);
    if placed {
        Ok((token, true))
    } else {
        let t = read_token(data_dir)?.context("the POS token vanished while it was being made")?;
        Ok((t, false))
    }
}

/// Replace the POS token with a new one. Every paired phone must pair again, and a running till
/// keeps the old token until it restarts.
pub fn rotate_token(data_dir: &Path) -> Result<String> {
    std::fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let token = new_token();
    let tmp = write_private_temp(data_dir, &token)?;
    let path = token_path(data_dir);
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(token)
}

fn new_token() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

/// `token` in a new 0600 file next to the token file, synced. Returns its path.
fn write_private_temp(data_dir: &Path, token: &str) -> Result<PathBuf> {
    let tmp = data_dir.join(format!(".{TOKEN_FILE}.{}", hex::encode(rand::random::<[u8; 8]>())));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    #[cfg(unix)]
    {
        // The umask can only remove bits, but set the mode explicitly all the same.
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(format!("{token}\n").as_bytes())?;
    f.sync_all()?;
    Ok(tmp)
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

/// The till URL a pairing may carry: `http://` or `https://`, a host, an optional port, and
/// nothing else (no user, path, query or fragment), as both POS apps parse it. `https://` is for
/// a till behind a proxy that terminates TLS; the phone then opens the card link with `wss://`.
/// Returns the URL without a trailing slash.
pub fn check_till_url(url: &str) -> Result<String> {
    let u = reqwest::Url::parse(url.trim()).with_context(|| format!("{url:?} is not a URL"))?;
    if u.scheme() != "http" && u.scheme() != "https" {
        bail!("the till URL must start with http:// or https://, not {}://", u.scheme());
    }
    if u.host_str().is_none_or(str::is_empty) {
        bail!("the till URL {url:?} has no host");
    }
    if !u.username().is_empty() || u.password().is_some() {
        bail!("the till URL must not carry a user name or password");
    }
    if u.path() != "/" || u.query().is_some() || u.fragment().is_some() {
        bail!("the till URL must be only scheme, host and port (no path, query or fragment)");
    }
    Ok(u.as_str().trim_end_matches('/').to_owned())
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

/// How `/v1/*` is guarded: the POS token, and whether loopback peers are exempt from it.
#[derive(Clone, Default)]
pub struct PosAuth {
    pub token: Option<std::sync::Arc<str>>,
    /// With a token, require it from loopback peers too. Set behind a proxy (StartOS), where
    /// every request may arrive from a local address whoever sent it.
    pub everywhere: bool,
}

/// Is this request allowed? Without a token, yes. With one, loopback peers are (unless
/// `everywhere`); others need the bearer. A request with no peer address (none recorded) is not
/// treated as loopback.
pub fn authorized(token: Option<&str>, everywhere: bool, peer: Option<SocketAddr>, authorization: Option<&str>) -> bool {
    let Some(token) = token else { return true };
    if !everywhere && peer.is_some_and(|p| p.ip().is_loopback() || p.ip().to_canonical().is_loopback()) {
        return true;
    }
    let Some(given) = authorization.and_then(|h| h.strip_prefix("Bearer ")) else { return false };
    token_eq(given.trim().as_bytes(), token.as_bytes())
}

/// Middleware over `/v1/*`: [`authorized`] with the handle's token and the peer's address.
pub async fn auth(State(a): State<PosAuth>, req: Request, next: Next) -> Response {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    let header = req.headers().get(axum::http::header::AUTHORIZATION).and_then(|h| h.to_str().ok());
    if authorized(a.token.as_deref(), a.everywhere, peer, header) { next.run(req).await } else { unauthorized() }
}

// ------------------------------------------------------------------ phones charging offline

/// Largest `POST /v1/offline/charges` body: 50 charges of a few kilobytes each.
pub const MAX_CHARGES_BODY: usize = 1 << 20;

/// A phone's id (its store's `phone-id`): 16 bytes, hex.
fn phone_of(id: &str) -> Option<String> {
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_ascii_lowercase())
}

#[derive(Deserialize)]
pub struct KitQuery {
    phone: Option<String>,
}

/// `GET /v1/offline/kit?phone=<32 hex>`: the phone's offline kit (docs/TILL-API.md), never cached.
pub async fn offline_kit(State(h): State<TillHandle>, Query(q): Query<KitQuery>) -> Response {
    let Some(phone) = q.phone.as_deref().and_then(phone_of) else {
        return Reply::fail(400, "bad_request", "phone must be the app's phone id, 32 hex characters").into_response();
    };
    let Some(off) = h.offline_now() else {
        return Reply::fail(503, "offline_not_ready", "this till takes no offline charges").into_response();
    };
    let keys = TillKeys::of(h.wallet());
    let header = h.header();
    let r = tokio::task::spawn_blocking(move || {
        let cert_key = crate::claim::read_cert_key(&off.dir)?;
        crate::pool::issue_kit(&off.pool, &keys, &off.cfg, &off.kits, cert_key, header, &phone, crate::claim::now_ms())
    })
    .await;
    match r {
        Ok(Ok(kit)) => ([(CACHE_CONTROL, "no-store")], Reply::ok(json!({"ok": true, "kit": kit}))).into_response(),
        Ok(Err(e)) => error_reply(&e).into_response(),
        Err(e) => Reply::fail(500, "internal", format!("issuing the kit: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct ChargesReq {
    phone_id: String,
    charges: Vec<Value>,
}

/// `POST /v1/offline/charges {phone_id, charges}`: what a phone took offline, into the till's
/// queue (docs/TILL-API.md). Each charge is answered `queued`, `already` or `refused`.
pub async fn offline_charges(State(h): State<TillHandle>, body: Bytes) -> Reply {
    let req: ChargesReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return Reply::fail(400, "bad_request", format!("body must be JSON like {{\"phone_id\":\"…\",\"charges\":[…]}}: {e}")),
    };
    let Some(phone) = phone_of(&req.phone_id) else {
        return Reply::fail(400, "bad_request", "phone_id must be the app's phone id, 32 hex characters");
    };
    if req.charges.len() > MAX_INGEST {
        return Reply::fail(400, "bad_request", format!("at most {MAX_INGEST} charges at a time"));
    }
    let Some(off) = h.offline_now() else {
        return Reply::fail(503, "offline_not_ready", "this till takes no offline charges");
    };
    let keys = TillKeys::of(h.wallet());
    let sales = h.sales();
    let r = tokio::task::spawn_blocking(move || {
        crate::pool::ingest(&off.queue, &off.pool, &keys, &sales, &phone, req.charges, crate::claim::now_ms())
    })
    .await;
    match r {
        Ok(results) => Reply::ok(json!({"ok": true, "results": results})),
        Err(e) => Reply::fail(500, "internal", format!("taking the charges: {e}")),
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
        assert!(authorized(None, false, Some(lan), None));
        assert!(authorized(Some("ab"), false, Some(lo), None));
        assert!(authorized(Some("ab"), false, Some(lo6), None));
        assert!(!authorized(Some("ab"), false, Some(lan), None));
        assert!(!authorized(Some("ab"), false, None, None));
        assert!(!authorized(Some("ab"), false, Some(lan), Some("Bearer ac")));
        assert!(!authorized(Some("ab"), false, Some(lan), Some("ab")));
        assert!(authorized(Some("ab"), false, Some(lan), Some("Bearer ab")));
        assert_eq!(pair_text("http://10.0.0.2:9877/", "ff"), "fedimintcard-till:http://10.0.0.2:9877?t=ff");
    }

    #[test]
    fn everywhere_takes_away_the_loopback_exemption() {
        let lo: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let lo6: SocketAddr = "[::1]:5000".parse().unwrap();
        let lan: SocketAddr = "10.0.3.1:5000".parse().unwrap();
        for peer in [Some(lo), Some(lo6), Some(lan), None] {
            assert!(!authorized(Some("ab"), true, peer, None), "{peer:?}");
            assert!(!authorized(Some("ab"), true, peer, Some("Bearer ac")), "{peer:?}");
            assert!(authorized(Some("ab"), true, peer, Some("Bearer ab")), "{peer:?}");
        }
    }

    #[test]
    fn till_urls() {
        assert_eq!(check_till_url("https://till.example.com").unwrap(), "https://till.example.com");
        assert_eq!(check_till_url(" https://till.example.com/ ").unwrap(), "https://till.example.com");
        assert_eq!(check_till_url("https://till.example.com:8443").unwrap(), "https://till.example.com:8443");
        assert_eq!(check_till_url("http://192.168.1.20:9877").unwrap(), "http://192.168.1.20:9877");
        // The default port is dropped, as the phones would see it anyway.
        assert_eq!(check_till_url("https://till.example.com:443").unwrap(), "https://till.example.com");
        for bad in [
            "",
            "till.example.com",
            "ftp://till.example.com",
            "wss://till.example.com",
            "https://u:p@till.example.com",
            "https://till.example.com/v1",
            "https://till.example.com?t=ab",
            "https://till.example.com#x",
        ] {
            assert!(check_till_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rotating_replaces_the_token_privately() {
        let dir = tempfile::tempdir().unwrap();
        let (first, made) = ensure_token(dir.path()).unwrap();
        assert!(made);
        let second = rotate_token(dir.path()).unwrap();
        assert_ne!(first, second);
        assert_eq!(read_token(dir.path()).unwrap(), Some(second.clone()));
        assert_eq!(ensure_token(dir.path()).unwrap(), (second, false));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(token_path(dir.path())).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // Only the token file is left behind: no temporary files.
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from(TOKEN_FILE)]);
    }

    #[test]
    fn an_empty_token_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(token_path(dir.path()), "").unwrap();
        let (t, made) = ensure_token(dir.path()).unwrap();
        assert!(made);
        assert_eq!(read_token(dir.path()).unwrap(), Some(t));
    }

    #[test]
    fn concurrent_pairings_agree_on_one_token() {
        let dir = tempfile::tempdir().unwrap();
        let tokens: Vec<String> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..8).map(|_| s.spawn(|| ensure_token(dir.path()).unwrap().0)).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(tokens.iter().all(|t| t == &tokens[0]), "{tokens:?}");
        assert_eq!(read_token(dir.path()).unwrap(), Some(tokens[0].clone()));
    }
}
