//! Till claims (docs/TILL-CLAIM.md): `till claim` writes the claim privately, `till serve` with no
//! token polls it and goes on ops once an operator approves, and until then it says it is not
//! connected and refuses card jobs. The registry is a small fake speaking the claim routes; the
//! real ones are tested in registry/test.
//!
//! The federation fixture is copied from `till_service.rs`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::Request;
use axum::routing::post;
use axum::{Json, Router};
use fedimint_card_host_card::{Card, SoftCard};
use fedimint_card_sidecar::claim;
use fedimint_card_sidecar::glue::CardWallet;
use fedimint_card_sidecar::ops::{FederationReport, OpsConfig};
use fedimint_card_sidecar::sales::SalesStore;
use fedimint_card_sidecar::state::StateStore;
use fedimint_card_sidecar::till::{self, Conn, OpsState, claim_tick};
use fedimint_card_sidecar::wallet::Wallet;
use fedimint_dummy_client::DummyClientInit;
use fedimint_dummy_server::DummyInit;
use fedimint_mint_client::MintClientInit;
use fedimint_mint_server::MintInit;
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;
use serde_json::{Value, json};
use tower::ServiceExt as _;

// ------------------------------------------------------------------ a fake registry

#[derive(Default)]
struct Fake {
    /// How long a new claim lives, in ms; negative makes it born expired.
    ttl_ms: i64,
    approved: bool,
    /// The token was answered: the claim is gone.
    taken: bool,
    polls: u32,
    federations: u32,
    last_name: Option<String>,
}

const SECRET: &str = "5ec7e75ec7e75ec7e75ec7e75ec7e75ec7e75ec7e75ec7e75ec7e75ec7e75ec7";
const CLAIM_ID: &str = "c1a1c1a1c1a1c1a1c1a1c1a1c1a1c1a1";
const TOKEN: &str = "till_00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

type Shared = Arc<Mutex<Fake>>;

async fn start_claim(State(s): State<Shared>, Json(b): Json<Value>) -> Json<Value> {
    let mut g = s.lock().unwrap();
    g.last_name = b["name"].as_str().map(str::to_owned);
    let expires = claim::now_ms() as i64 + g.ttl_ms;
    Json(json!({"ok": true, "claim_id": CLAIM_ID, "code": "K7QM-3XRP", "poll_secret": SECRET, "expires_at": expires}))
}

async fn poll(State(s): State<Shared>, Path(id): Path<String>, Json(b): Json<Value>) -> (axum::http::StatusCode, Json<Value>) {
    let mut g = s.lock().unwrap();
    g.polls += 1;
    if id != CLAIM_ID || b["poll_secret"] != SECRET || g.taken {
        return (axum::http::StatusCode::NOT_FOUND, Json(json!({"ok": false, "reason": "no claim"})));
    }
    if !g.approved {
        return (axum::http::StatusCode::OK, Json(json!({"ok": true, "status": "pending"})));
    }
    g.taken = true;
    (axum::http::StatusCode::OK, Json(json!({"ok": true, "status": "approved", "till_id": 7, "name": "t", "token": TOKEN})))
}

async fn federations(State(s): State<Shared>, headers: axum::http::HeaderMap) -> Json<Value> {
    assert_eq!(headers["authorization"], format!("Bearer {TOKEN}"));
    s.lock().unwrap().federations += 1;
    Json(json!({"ok": true}))
}

/// The fake's URL and its state.
async fn fake_registry(ttl_ms: i64) -> (String, Shared) {
    let state: Shared = Arc::new(Mutex::new(Fake { ttl_ms, ..Fake::default() }));
    let app = Router::new()
        .route("/till-claims", post(start_claim))
        .route("/till-claims/{id}/poll", post(poll))
        .route("/federations", post(federations))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, state)
}

fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn report() -> FederationReport {
    FederationReport { federation_id: "f".repeat(64), name: Some("Test".into()), tbs_pks: Default::default() }
}

// ------------------------------------------------------------------ the claim file and the poll

#[tokio::test]
async fn claim_writes_a_private_file_and_refuses_a_connected_till_without_replace() -> anyhow::Result<()> {
    let (url, fake) = fake_registry(600_000).await;
    let dir = tempfile::tempdir()?;
    let c = claim::start(dir.path(), &format!("{url}/"), "  Front counter ", false).await?;
    assert_eq!(fake.lock().unwrap().last_name.as_deref(), Some("Front counter"));
    assert_eq!(c.code, "K7QM-3XRP");
    assert_eq!(c.qr_text()?, format!("fedimintcard-till-claim:{url}?c=K7QM-3XRP"));
    let path = dir.path().join(claim::CLAIM_FILE);
    assert_eq!(mode(&path), 0o600);
    let on_disk: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(on_disk["claim_id"], CLAIM_ID);
    assert_eq!(on_disk["poll_secret"], SECRET);
    assert_eq!(on_disk["ops_url"], url.as_str());
    // No temporary files left beside it.
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);

    assert!(claim::start(dir.path(), &url, "", false).await.is_err(), "an empty name");
    claim::write_ops(dir.path(), &OpsConfig { url: url.clone(), token: TOKEN.into() })?;
    let err = claim::start(dir.path(), &url, "again", false).await.unwrap_err();
    assert!(err.to_string().contains("--replace"), "{err:#}");
    claim::start(dir.path(), &url, "again", true).await?;
    Ok(())
}

#[tokio::test]
async fn the_poller_waits_then_keeps_the_token_and_goes_on_ops() -> anyhow::Result<()> {
    let (url, fake) = fake_registry(600_000).await;
    let dir = tempfile::tempdir()?;
    let c = claim::start(dir.path(), &url, "t", false).await?;
    let conn = Conn::new(OpsState::of_claim(Some(&c), None, claim::now_ms()), None);
    let mut dead = Default::default();
    let r = report();

    claim_tick(&conn, dir.path(), Some(&r), &mut dead).await;
    assert!(matches!(&conn.lock().unwrap().state, OpsState::Waiting { code, .. } if code == "K7QM-3XRP"));
    assert!(conn.lock().unwrap().ops.is_none());

    fake.lock().unwrap().approved = true;
    claim_tick(&conn, dir.path(), Some(&r), &mut dead).await;
    {
        let g = conn.lock().unwrap();
        assert_eq!(g.state, OpsState::Connected { url: url.clone() });
        assert_eq!(g.ops.as_ref().unwrap().client.url(), url);
    }
    assert_eq!(fake.lock().unwrap().federations, 1, "the federation is reported with the new token");
    assert!(!dir.path().join(claim::CLAIM_FILE).exists(), "the claim file is gone");
    let ops_file = dir.path().join(claim::OPS_FILE);
    assert_eq!(mode(&ops_file), 0o600);
    let kept = claim::read_ops(dir.path())?.unwrap();
    assert_eq!((kept.url.as_str(), kept.token.as_str()), (url.as_str(), TOKEN));

    // Connected: nothing more is asked of the registry.
    let polls = fake.lock().unwrap().polls;
    claim_tick(&conn, dir.path(), Some(&r), &mut dead).await;
    assert_eq!(fake.lock().unwrap().polls, polls);
    assert!(matches!(conn.lock().unwrap().state, OpsState::Connected { .. }));
    Ok(())
}

#[tokio::test]
async fn a_claim_approved_while_the_till_was_off_is_collected_after_its_file_expires() -> anyhow::Result<()> {
    // The operator approved within the claim's 10 minutes; the till only polls after them. The
    // registry keeps an approved claim for days, so the file's expires_at must not stop the poll.
    let (url, fake) = fake_registry(-1).await;
    let dir = tempfile::tempdir()?;
    let c = claim::start(dir.path(), &url, "t", false).await?;
    assert_eq!(OpsState::of_claim(Some(&c), None, claim::now_ms()), OpsState::ClaimExpired { url: url.clone(), code: c.code.clone() });
    fake.lock().unwrap().approved = true;
    let conn = Conn::new(OpsState::of_claim(Some(&c), None, claim::now_ms()), None);
    let r = report();
    claim_tick(&conn, dir.path(), Some(&r), &mut Default::default()).await;
    assert_eq!(conn.lock().unwrap().state, OpsState::Connected { url: url.clone() });
    assert_eq!(claim::read_ops(dir.path())?.unwrap().token, TOKEN);
    Ok(())
}

#[tokio::test]
async fn an_expired_claim_the_registry_forgot_is_asked_once() -> anyhow::Result<()> {
    let (url, fake) = fake_registry(-1).await;
    let dir = tempfile::tempdir()?;
    claim::start(dir.path(), &url, "t", false).await?;
    fake.lock().unwrap().taken = true;
    let conn = Conn::new(OpsState::NotConnected { url: None }, None);
    let mut dead = Default::default();
    for _ in 0..3 {
        claim_tick(&conn, dir.path(), None, &mut dead).await;
    }
    assert!(matches!(conn.lock().unwrap().state, OpsState::ClaimExpired { .. }));
    assert_eq!(fake.lock().unwrap().polls, 1);

    // Live by its file, but the registry no longer knows it (404): expired, and asked only once.
    let (url, fake) = fake_registry(600_000).await;
    let dir = tempfile::tempdir()?;
    claim::start(dir.path(), &url, "t", false).await?;
    fake.lock().unwrap().taken = true;
    let conn = Conn::new(OpsState::NotConnected { url: None }, None);
    let mut dead = Default::default();
    for _ in 0..3 {
        claim_tick(&conn, dir.path(), None, &mut dead).await;
    }
    assert!(matches!(conn.lock().unwrap().state, OpsState::ClaimExpired { .. }));
    assert_eq!(fake.lock().unwrap().polls, 1);
    Ok(())
}

#[tokio::test]
async fn a_local_till_stays_local_until_a_claim_file_appears() -> anyhow::Result<()> {
    let (url, _fake) = fake_registry(600_000).await;
    let dir = tempfile::tempdir()?;
    let conn = Conn::new(OpsState::Local, None);
    let mut dead = Default::default();
    claim_tick(&conn, dir.path(), None, &mut dead).await;
    assert_eq!(conn.lock().unwrap().state, OpsState::Local);
    claim::start(dir.path(), &url, "t", false).await?;
    claim_tick(&conn, dir.path(), None, &mut dead).await;
    assert!(matches!(conn.lock().unwrap().state, OpsState::Waiting { .. }));
    Ok(())
}

#[tokio::test]
async fn no_claim_file_is_not_connected() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let conn = Conn::new(OpsState::NotConnected { url: Some("https://r.example".into()) }, None);
    claim_tick(&conn, dir.path(), None, &mut Default::default()).await;
    assert_eq!(conn.lock().unwrap().state, OpsState::NotConnected { url: Some("https://r.example".into()) });
    Ok(())
}

// ------------------------------------------------------------------ the till service

struct Fx {
    _fed: FederationTest,
    cw: CardWallet,
    dir: tempfile::TempDir,
}

async fn fx() -> anyhow::Result<Fx> {
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit);
    let fed = tokio::time::timeout(Duration::from_secs(60), fixtures.new_fed_builder(0).build()).await?;
    let client = fed.new_client().await;
    let wallet = Wallet::from_client(client).await?;
    let dir = tempfile::tempdir()?;
    let store = StateStore::open(dir.path().join("card_state.json"))?;
    let cw = CardWallet::new(wallet, store, Box::new(SoftCard::new()));
    Ok(Fx { _fed: fed, cw, dir })
}

async fn call(app: Router, method: &str, uri: &str, body: &str) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .extension(axum::extract::ConnectInfo("127.0.0.1:9".parse::<SocketAddr>().unwrap()))
        .body(Body::from(body.to_owned()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Not connected: status says so, card jobs are refused before any card is opened, and once the
/// claim is approved the same running till is on ops (a charge then wants the holder's PIN).
#[tokio::test(flavor = "multi_thread")]
async fn a_till_without_a_token_refuses_card_jobs_until_approved_then_attaches_live() -> anyhow::Result<()> {
    let (url, fake) = fake_registry(600_000).await;
    let f = fx().await?;
    let dir = f.dir.path().to_owned();
    let sales = SalesStore::open(dir.join("sales.json"))?;
    let conn = Conn::new(OpsState::NotConnected { url: Some(url.clone()) }, None);
    let opened = Arc::new(Mutex::new(0u32));
    let counter = opened.clone();
    let (handle, _actor) = till::new_with_conn(
        f.cw,
        "http://127.0.0.1:1".into(),
        sales,
        Box::new(move || {
            *counter.lock().unwrap() += 1;
            Ok(Box::new(SoftCard::new()) as Box<dyn Card>)
        }),
        conn.clone(),
    )
    .await;
    let app = till::router(handle);

    let (code, st) = call(app.clone(), "GET", "/v1/status", "").await;
    assert_eq!(code, 200);
    assert_eq!((&st["connected"], &st["ops_state"], &st["ops_url"]), (&json!(false), &json!("not_connected"), &json!(url)));
    for (method, path, body) in [
        ("POST", "/v1/charge", r#"{"price_msat":20000,"client_ref":"x","pin":"1234"}"#),
        ("POST", "/v1/card/balance", ""),
        ("POST", "/v1/cards/issue", r#"{"amount_msat":[1000]}"#),
    ] {
        let (code, body) = call(app.clone(), method, path, body).await;
        assert_eq!((code, body["error"].as_str()), (503, Some("ops_not_connected")), "{path}: {body}");
    }

    // A claim: waiting, and still refused, with the code in both.
    claim::start(&dir, &url, "t", false).await?;
    let mut dead = Default::default();
    claim_tick(&conn, &dir, Some(&report()), &mut dead).await;
    let (_, st) = call(app.clone(), "GET", "/v1/status", "").await;
    assert_eq!((&st["ops_state"], &st["claim_code"]), (&json!("waiting_for_approval"), &json!("K7QM-3XRP")));
    assert!(st["claim_expires_at_ms"].as_u64().unwrap() > claim::now_ms());
    let (code, body) = call(app.clone(), "POST", "/v1/charge", r#"{"price_msat":20000,"pin":"1234"}"#).await;
    assert_eq!(code, 503);
    assert!(body["detail"].as_str().unwrap().contains("K7QM-3XRP"), "{body}");
    assert_eq!(*opened.lock().unwrap(), 0, "no card was opened");

    fake.lock().unwrap().approved = true;
    claim_tick(&conn, &dir, Some(&report()), &mut dead).await;
    let (_, st) = call(app.clone(), "GET", "/v1/status", "").await;
    assert_eq!((&st["connected"], &st["ops_state"]), (&json!(true), &json!("connected")));
    assert_eq!(st["claim_code"], Value::Null);
    let (code, body) = call(app, "POST", "/v1/charge", r#"{"price_msat":20000,"client_ref":"np"}"#).await;
    assert_eq!((code, body["error"].as_str()), (400, Some("pin_required")), "on ops now: {body}");
    Ok(())
}

/// The real `serve_with` as the StartOS package runs it (an ops URL, no token): it polls the
/// claim on its own and is on ops after approval, with no restart.
#[tokio::test(flavor = "multi_thread")]
async fn serve_polls_the_claim_and_picks_up_the_token() -> anyhow::Result<()> {
    let (url, fake) = fake_registry(600_000).await;
    let f = fx().await?;
    let dir = f.dir.path().to_owned();
    claim::start(&dir, &url, "StartOS till", false).await?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let listen: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let status_url = format!("http://127.0.0.1:{port}/v1/status");
    let local = tokio::task::LocalSet::new();
    let ops_url = url.clone();
    local
        .run_until(async move {
            let server = tokio::task::spawn_local(till::serve_with(f.cw, listen, "http://127.0.0.1:1".into(), Some(ops_url), None, false));
            let client = reqwest::Client::new();
            let status = || async {
                for _ in 0..200 {
                    if let Ok(r) = client.get(&status_url).send().await {
                        return r.json::<Value>().await.unwrap();
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                panic!("the till never answered");
            };
            assert_eq!(status().await["ops_state"], "waiting_for_approval");
            fake.lock().unwrap().approved = true;
            let mut seen = Value::Null;
            for _ in 0..40 {
                seen = status().await;
                if seen["connected"] == true {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            assert_eq!(seen["ops_state"], "connected", "{seen}");
            assert!(claim::read_ops(&dir).unwrap().is_some());
            assert!(claim::read_claim(&dir).unwrap().is_none());
            server.abort();
            drop(f._fed);
        })
        .await;
    Ok(())
}

/// A till serving with no registry and no claim picks up a claim made afterwards (`till claim`
/// while it runs) and is on ops after approval, with no restart.
#[tokio::test(flavor = "multi_thread")]
async fn serve_picks_up_a_claim_made_after_it_started() -> anyhow::Result<()> {
    let (url, fake) = fake_registry(600_000).await;
    let f = fx().await?;
    let dir = f.dir.path().to_owned();
    let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let listen: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let status_url = format!("http://127.0.0.1:{port}/v1/status");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let server = tokio::task::spawn_local(till::serve_with(f.cw, listen, "http://127.0.0.1:1".into(), None, None, false));
            let client = reqwest::Client::new();
            let status = || async {
                for _ in 0..200 {
                    if let Ok(r) = client.get(&status_url).send().await {
                        return r.json::<Value>().await.unwrap();
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                panic!("the till never answered");
            };
            assert_eq!(status().await["ops_state"], "local");
            claim::start(&dir, &url, "late claim", false).await.unwrap();
            fake.lock().unwrap().approved = true;
            let mut seen = Value::Null;
            for _ in 0..40 {
                seen = status().await;
                if seen["connected"] == true {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            assert_eq!(seen["ops_state"], "connected", "{seen}");
            server.abort();
            drop(f._fed);
        })
        .await;
    Ok(())
}

/// A till that never had a registry stays as it was: local card state, no polling.
#[tokio::test(flavor = "multi_thread")]
async fn a_till_with_no_registry_settings_stays_local() -> anyhow::Result<()> {
    let f = fx().await?;
    let sales = SalesStore::open(f.dir.path().join("sales.json"))?;
    let (handle, _actor) = till::new(f.cw, "http://127.0.0.1:1".into(), sales, Box::new(|| anyhow::bail!("no card"))).await;
    let (_, st) = call(till::router(handle), "GET", "/v1/status", "").await;
    assert_eq!((&st["connected"], &st["ops_state"]), (&json!(false), &json!("local")));
    Ok(())
}
