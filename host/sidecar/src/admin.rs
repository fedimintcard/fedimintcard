//! First setup and merchant controls. The admin listener can open before a federation has been
//! joined. Once the wallet is ready it shares the POS service's handle and RocksDB client.
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, Weak};

use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderValue, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use crate::glue::CardWallet;
use crate::state::StateStore;
use crate::till::{Reply, ServeOptions, TillHandle};
use crate::wallet::{Wallet, state_path};

struct JoinRequest {
    invite: String,
    answer: oneshot::Sender<Reply>,
}

struct Control {
    dir: PathBuf,
    registry: String,
    pos_url: Option<String>,
    live: RwLock<Weak<TillHandle>>,
    joins: mpsc::Sender<JoinRequest>,
    stop: watch::Sender<bool>,
    claim_lock: Mutex<()>,
    payout_lock: Mutex<()>,
}

#[derive(Clone)]
pub struct AdminState(Arc<Control>);

impl AdminState {
    fn new(dir: PathBuf, registry: String, pos_url: Option<String>, joins: mpsc::Sender<JoinRequest>) -> Self {
        Self(Arc::new(Control {
            dir,
            registry,
            pos_url,
            live: RwLock::new(Weak::new()),
            joins,
            stop: watch::channel(false).0,
            claim_lock: Mutex::new(()),
            payout_lock: Mutex::new(()),
        }))
    }

    pub(crate) fn attach(&self, handle: &TillHandle) -> Arc<TillHandle> {
        let active = Arc::new(handle.clone());
        *self.0.live.write().expect("live handle") = Arc::downgrade(&active);
        active
    }

    pub(crate) async fn stopping(&self) {
        let mut rx = self.0.stop.subscribe();
        let _ = rx.wait_for(|stop| *stop).await;
    }

    pub async fn till(&self) -> Result<TillHandle, Reply> {
        if *self.0.stop.borrow() {
            return Err(Reply::fail(503, "stopping", "The till is stopping."));
        }
        self.0
            .live
            .read()
            .expect("live handle")
            .upgrade()
            .map(|h| (*h).clone())
            .ok_or_else(|| Reply::fail(503, "setup_required", "The till is not ready to take payments."))
    }

    pub fn dir(&self) -> &Path {
        &self.0.dir
    }

    pub fn payout_lock(&self) -> &Mutex<()> {
        &self.0.payout_lock
    }
}

pub async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("installing the SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

fn check_secret(s: String) -> Result<String> {
    ensure!(
        (16..=512).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_graphic()),
        "provide a dedicated admin password of 16 to 512 visible ASCII characters"
    );
    Ok(s)
}

fn secret(file: Option<&Path>) -> Result<String> {
    let s = match file {
        Some(p) => std::fs::read_to_string(p).context("reading the admin password file")?.trim_end_matches(['\r', '\n']).to_owned(),
        None => std::env::var("TILL_ADMIN_PASSWORD")
            .context("configure TILL_ADMIN_PASSWORD_FILE or TILL_ADMIN_PASSWORD before starting the admin service")?,
    };
    check_secret(s)
}

async fn auth(State(password): State<Arc<String>>, req: Request, next: Next) -> Response {
    let token = req.headers().get(header::AUTHORIZATION).and_then(|h| h.to_str().ok()).and_then(|s| s.strip_prefix("Bearer "));
    if !token.is_some_and(|t| crate::pos::token_eq(t.as_bytes(), password.as_bytes())) {
        return Reply::fail(401, "unauthorized", "Enter the admin password.").into_response();
    }
    next.run(req).await
}

async fn response_headers(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    headers.insert("content-security-policy", HeaderValue::from_static("default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; font-src 'self'; img-src 'self' data: blob:; connect-src 'self'; form-action 'none'; base-uri 'none'; frame-ancestors 'none'"));
    response
}

fn router(state: AdminState, password: String) -> Router {
    let controls = Router::new()
        .route("/admin/status", get(status))
        .route("/admin/sales", get(sales))
        .route("/admin/join", post(join))
        .route("/admin/claim", post(claim))
        .route("/admin/pair", post(pair))
        .merge(crate::payout::router())
        .route_layer(middleware::from_fn_with_state(Arc::new(password), auth))
        .layer(DefaultBodyLimit::max(32 * 1024))
        .with_state(state);
    Router::new()
        .merge(controls)
        .route("/", get(|| async { Html(include_str!("admin.html")) }))
        .route(
            "/assets/figtree.ttf",
            get(|| async { ([(header::CONTENT_TYPE, "font/ttf")], include_bytes!("assets/figtree.ttf").as_slice()) }),
        )
        .route("/assets/Figtree-OFL.txt", get(|| async { include_str!("assets/Figtree-OFL.txt") }))
        .route("/healthz", get(|| async { Json(json!({"ok":true})) }))
        .layer(middleware::from_fn(response_headers))
}

async fn status(State(s): State<AdminState>) -> Reply {
    let mut reply = match s.till().await {
        Ok(h) => {
            let mut reply = crate::till::status(State(h)).await;
            reply.body["joined"] = json!(true);
            reply
        }
        Err(_) => Reply::ok(json!({"joined": false, "starting": s.0.joins.is_closed()})),
    };
    reply.body["pos_url"] = json!(s.0.pos_url);
    reply
}

/// Use deployment-provided routing information. A browser's Host header identifies
/// the admin page, which can be on an entirely different network from the POS API.
fn pairing_origin(explicit: Option<&str>, published: Option<&str>, listen: SocketAddr) -> Result<Option<String>> {
    if let Some(url) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return crate::pos::check_till_url(url).map(Some);
    }
    let addr = match published.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.parse::<SocketAddr>().context("invalid published POS socket address")?,
        None => listen,
    };
    let ip = addr.ip().to_canonical();
    if addr.port() == 0 || ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return Ok(None);
    }
    match ip {
        std::net::IpAddr::V4(ip) if ip.is_broadcast() => Ok(None),
        std::net::IpAddr::V6(ip) if ip.is_unicast_link_local() => Ok(None),
        _ => Ok(Some(format!("http://{addr}"))),
    }
}

async fn sales(State(s): State<AdminState>) -> Result<Reply, Reply> {
    Ok(crate::till::sales_list(State(s.till().await?)).await)
}

#[derive(Deserialize)]
struct Invite {
    invite: String,
}

async fn join(State(s): State<AdminState>, Json(req): Json<Invite>) -> Reply {
    let (answer, reply) = oneshot::channel();
    let request = JoinRequest { invite: req.invite.trim().to_owned(), answer };
    if s.0.joins.try_send(request).is_err() {
        return Reply::fail(409, "join_unavailable", "The till is already joining or joined.");
    }
    reply.await.unwrap_or_else(|_| Reply::fail(409, "join_unavailable", "The till is already joining or joined."))
}

#[derive(Deserialize)]
struct Claim {
    name: String,
}

async fn claim(State(s): State<AdminState>, Json(req): Json<Claim>) -> Result<Reply, Reply> {
    let _handle = s.till().await?;
    let _guard = s.0.claim_lock.lock().await;
    crate::claim::check_name(&req.name).map_err(|e| Reply::fail(400, "invalid_name", e))?;
    let c = crate::claim::start(s.dir(), &s.0.registry, &req.name, false)
        .await
        .map_err(|_| Reply::fail(409, "claim_failed", "Check the registry connection and whether this till is already connected."))?;
    let text = c.qr_text().map_err(|_| Reply::fail(500, "claim_qr", "The claim QR could not be made."))?;
    Ok(Reply::ok(json!({"text":text, "code":c.code, "expires_at_ms":c.expires_at, "qr_svg":qr_svg(&text)?})))
}

#[derive(Deserialize)]
struct Pair {
    url: Option<String>,
}

async fn pair(State(s): State<AdminState>, Json(req): Json<Pair>) -> Result<Reply, Reply> {
    let _handle = s.till().await?;
    let address = req
        .url
        .as_deref()
        .or(s.0.pos_url.as_deref())
        .ok_or_else(|| Reply::fail(400, "pos_address_required", "This installation has not supplied a phone address."))?;
    let url = crate::pos::check_till_url(address)
        .map_err(|_| Reply::fail(400, "invalid_pos_url", "Enter the HTTP or HTTPS address that the POS phone uses to reach the till."))?;
    let token = crate::pos::read_token(s.dir())
        .ok()
        .flatten()
        .ok_or_else(|| Reply::fail(503, "pos_unavailable", "The POS token is unavailable."))?;
    let text = crate::pos::pair_text(&url, &token);
    Ok(Reply::ok(json!({"text":text,"qr_svg":qr_svg(&text)?})))
}

/// No user text is interpolated into the SVG; the payload only selects dark modules.
pub(crate) fn qr_svg(text: &str) -> Result<String, Reply> {
    use std::fmt::Write;
    let code =
        qrcode::QrCode::new(text.as_bytes()).map_err(|_| Reply::fail(422, "qr_too_large", "The text is too long for one QR code."))?;
    let width = code.width();
    let size = width + 8;
    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {size} {size}" shape-rendering="crispEdges"><rect width="100%" height="100%" fill="white"/><path d=""#
    );
    for y in 0..width {
        for x in 0..width {
            if code[(x, y)] == qrcode::Color::Dark {
                write!(&mut svg, "M{} {}h1v1h-1z", x + 4, y + 4).expect("write to string");
            }
        }
    }
    svg.push_str(r#""/></svg>"#);
    Ok(svg)
}

#[allow(clippy::too_many_arguments)]
pub async fn serve(
    dir: PathBuf,
    admin_listen: SocketAddr,
    pos_listen: SocketAddr,
    relay: String,
    registry: Option<String>,
    ops_token: Option<String>,
    opts: ServeOptions,
    password_file: Option<&Path>,
    pos_public_url: Option<&str>,
) -> Result<()> {
    let password = secret(password_file)?;
    crate::state::create_private_dir(&dir)?;
    let lock = std::fs::File::options().read(true).write(true).create(true).truncate(false).open(dir.join("admin.lock"))?;
    lock.try_lock().context("another admin process owns this data directory")?;
    for token in [crate::pos::read_token(&dir)?, crate::pos::read_pay_token(&dir)?].into_iter().flatten() {
        ensure!(
            !crate::pos::token_eq(password.as_bytes(), token.as_bytes()),
            "set a dedicated admin password different from the paired tokens"
        );
    }
    let listener = tokio::net::TcpListener::bind(admin_listen).await.context("opening admin listener")?;
    let registry = registry.unwrap_or_else(|| "https://registry.fedimintcard.org".to_owned());
    let (tx, mut rx) = mpsc::channel::<JoinRequest>(1);
    let pos_url = pairing_origin(pos_public_url, std::env::var("TILL_POS_LAN_ADDR").ok().as_deref(), pos_listen).unwrap_or_else(|_| {
        tracing::warn!("Invalid POS pairing address configuration. A merchant can still enter the phone address in Pairing.");
        None
    });
    let state = AdminState::new(dir.clone(), registry.clone(), pos_url, tx);
    let app = router(state.clone(), password);
    let admin_state = state.clone();
    let admin = async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { admin_state.stopping().await })
            .await
            .context("serving merchant controls")
    };
    let pos = async {
        let wallet = if Wallet::joined(&dir).await? {
            Wallet::open(&dir).await?
        } else {
            loop {
                let request = tokio::select! {
                    req = rx.recv() => req.context("admin listener closed")?,
                    _ = state.stopping() => return Ok(()),
                };
                // The service owns this attempt even if the browser navigates away.
                let joined = tokio::select! {
                    result = Wallet::join(&dir, &request.invite) => result,
                    _ = state.stopping() => return Ok(()),
                };
                match joined {
                    Ok(w) => {
                        rx.close();
                        let _ = request.answer.send(Reply::ok(json!({"joined":true})));
                        break w;
                    }
                    Err(_) if Wallet::joined(&dir).await? => {
                        let w = Wallet::open(&dir).await?;
                        rx.close();
                        let _ = request.answer.send(Reply::ok(json!({"joined":true})));
                        break w;
                    }
                    Err(_) => {
                        let _ = request.answer.send(Reply::fail(400, "join_failed", "Check the federation invite and its connectivity."));
                    }
                }
            }
        };
        drop(rx);
        let store = StateStore::open(state_path(&dir))?;
        let cw = CardWallet::new(wallet, store, Box::new(crate::till::NoCard));
        crate::till::serve_managed(cw, pos_listen, relay, Some(registry), ops_token, true, opts, Some(state.clone())).await
    };
    // The sweep loop is scoped to this service and shares the same authenticated admin state.
    let services = async {
        tokio::try_join!(admin, pos)?;
        Ok(())
    };
    tokio::pin!(services);
    tokio::select! {
        result = &mut services => result,
        _ = shutdown() => {state.0.stop.send_replace(true); services.await},
        _ = crate::payout::sweeper(state.clone()) => bail!("payout worker exited unexpectedly"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;
    use tower::ServiceExt as _;

    #[test]
    fn pairing_uses_the_published_pos_origin() -> Result<()> {
        let listener = "0.0.0.0:16410".parse()?;
        assert_eq!(
            pairing_origin(Some("https://pos.example/"), Some("192.168.1.2:16411"), listener)?.as_deref(),
            Some("https://pos.example")
        );
        assert_eq!(pairing_origin(None, Some("192.168.1.2:16411"), listener)?.as_deref(), Some("http://192.168.1.2:16411"));
        assert_eq!(pairing_origin(Some(""), None, "[fd00::2]:16411".parse()?)?.as_deref(), Some("http://[fd00::2]:16411"));
        assert!(pairing_origin(None, None, listener)?.is_none());
        assert!(pairing_origin(None, Some("127.0.0.1:16411"), listener)?.is_none());
        assert!(pairing_origin(Some("https://u:pw@pos.example/"), None, listener).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn prejoin_health_is_public_but_setup_sales_and_payout_require_the_admin_credential() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (tx, _rx) = mpsc::channel(1);
        let state = AdminState::new(dir.path().to_owned(), "http://127.0.0.1".into(), None, tx);
        let admin_password = "test-merchant-only-credential";
        let app = router(state, admin_password.to_owned());
        for (method, path, credential, status) in [
            ("GET", "/healthz", None, StatusCode::OK),
            ("GET", "/admin/status", None, StatusCode::UNAUTHORIZED),
            ("GET", "/admin/status", Some("paired-pos-token"), StatusCode::UNAUTHORIZED),
            ("POST", "/admin/join", Some("paired-pay-token"), StatusCode::UNAUTHORIZED),
            ("POST", "/admin/claim", None, StatusCode::UNAUTHORIZED),
            ("POST", "/admin/pair", None, StatusCode::UNAUTHORIZED),
            ("GET", "/admin/sales", None, StatusCode::UNAUTHORIZED),
            ("GET", "/admin/payout", None, StatusCode::UNAUTHORIZED),
            ("GET", "/admin/status", Some(admin_password), StatusCode::OK),
            ("GET", "/admin/sales", Some(admin_password), StatusCode::SERVICE_UNAVAILABLE),
            ("GET", "/v1/status", Some(admin_password), StatusCode::NOT_FOUND),
        ] {
            let mut req = Request::builder().method(method).uri(path);
            if let Some(secret) = credential {
                req = req.header(header::AUTHORIZATION, format!("Bearer {secret}"));
            }
            let res = app.clone().oneshot(req.body(Body::empty())?).await?;
            assert_eq!(res.status(), status, "{method} {path}");
            assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
        }
        Ok(())
    }

    #[test]
    fn password_file_keeps_the_actual_credential_and_allows_the_final_newline() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("admin-password");
        std::fs::write(&path, "a-private-password123\r\n")?;
        assert_eq!(secret(Some(&path))?, "a-private-password123");
        std::fs::write(&path, "a-private password123\n")?;
        assert!(secret(Some(&path)).is_err());
        std::fs::write(&path, "\n")?;
        assert!(secret(Some(&path)).is_err());
        assert!(secret(Some(&dir.path().join("missing"))).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn incomplete_restores_cannot_create_a_replacement_wallet() -> Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(!Wallet::joined(dir.path()).await?);
        std::fs::write(dir.path().join(crate::wallet::INVITE_FILE), "existing federation")?;
        assert!(Wallet::joined(dir.path()).await.is_err());
        std::fs::create_dir(dir.path().join(crate::wallet::DB_DIR))?;
        assert!(Wallet::joined(dir.path()).await.is_err());
        assert!(!dir.path().join(crate::wallet::MNEMONIC_FILE).exists());
        Ok(())
    }
}
