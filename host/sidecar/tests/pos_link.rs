//! The POS link (docs/POS-LINK.md): the bearer token off loopback, pairing, and charges and
//! balances whose card is a phone's WebSocket link. The phone here is a test WebSocket client
//! answering APDUs from a `SoftCard`.
//!
//! The federation fixture is copied from `till_service.rs`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use fedimint_card_host_card::{Card, SoftCard, open_pin_session};
use fedimint_card_sidecar::glue::CardWallet;
use fedimint_card_sidecar::pos;
use fedimint_card_sidecar::sales::SalesStore;
use fedimint_card_sidecar::state::StateStore;
use fedimint_card_sidecar::till::{self, TillHandle};
use fedimint_card_sidecar::wallet::Wallet;
use fedimint_client::ClientHandleArc;
use fedimint_client::transaction::TransactionBuilder;
use fedimint_core::Amount;
use fedimint_core::core::OperationId;
use fedimint_core::secp256k1::XOnlyPublicKey;
use fedimint_core::secp256k1::schnorr::Signature;
use fedimint_dummy_client::{DummyClientInit, DummyClientModule};
use fedimint_dummy_server::DummyInit;
use fedimint_mint_client::MintClientInit;
use fedimint_mint_server::MintInit;
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt as _;

struct Fx {
    _fed: FederationTest,
    cw: CardWallet,
    card: Arc<Mutex<SoftCard>>,
    dir: tempfile::TempDir,
}

/// The test's card, shared between the wallet (for setup) and the phone (for the link).
struct Shared(Arc<Mutex<SoftCard>>);
impl Card for Shared {
    fn gen_key(&mut self, s: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.lock().unwrap().gen_key(s)
    }
    fn gen_key_overwrite(&mut self, s: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.lock().unwrap().gen_key_overwrite(s)
    }
    fn get_pub(&mut self, s: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.lock().unwrap().get_pub(s)
    }
    fn sign(&mut self, s: u8, m: &[u8; 32]) -> anyhow::Result<Signature> {
        self.0.lock().unwrap().sign(s, m)
    }
    fn apdu(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.0.lock().unwrap().apdu(apdu)
    }
}

async fn fund(client: &ClientHandleArc, amount: Amount) -> anyhow::Result<()> {
    let dummy = client.get_first_module::<DummyClientModule>()?;
    let op = OperationId::new_random();
    let range = client
        .finalize_and_submit_transaction(op, "fund", |_| (), TransactionBuilder::new().with_inputs(dummy.create_input(amount)))
        .await?;
    client.await_primary_bitcoin_module_outputs(op, range.into_iter().collect()).await?;
    Ok(())
}

/// A funded wallet and a keyed soft card holding `notes`.
async fn fx(notes: &[u64]) -> anyhow::Result<Fx> {
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit);
    let fed = tokio::time::timeout(Duration::from_secs(60), fixtures.new_fed_builder(0).build()).await?;
    let client = fed.new_client().await;
    fund(&client, Amount::from_sats(10_000)).await?;
    let wallet = Wallet::from_client(client).await?;
    let dir = tempfile::tempdir()?;
    let store = StateStore::open(dir.path().join("card_state.json"))?;
    let card = Arc::new(Mutex::new(SoftCard::new()));
    let mut cw = CardWallet::new(wallet, store, Box::new(Shared(card.clone())));
    cw.card_setup(false)?;
    open_pin_session(cw.card.as_mut())?;
    if !notes.is_empty() {
        cw.topup(notes).await?;
    }
    Ok(Fx { _fed: fed, cw, card, dir })
}

/// The applet's APDU answers (`data || SW`), from a soft card.
fn answer(card: &Mutex<SoftCard>, apdu: &[u8]) -> Vec<u8> {
    fedimint_card_host_card::softkit::answer(&mut card.lock().unwrap(), apdu)
}

/// How the test phone ends: answer everything, or give up at the first APDU with this INS.
#[derive(Clone, Copy)]
enum Phone {
    Answer,
    CloseAt(u8),
    LostAt(u8),
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Open a link as the POS app does and return its id plus the task that answers it. The task
/// returns the INS bytes it saw, and ends when the till closes the link.
async fn open_link(addr: SocketAddr, card: Arc<Mutex<SoftCard>>, how: Phone) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
    let (mut ws, _): (Ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/card/link")).await.unwrap();
    let first = ws.next().await.unwrap().unwrap();
    let ready: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
    assert_eq!(ready["type"], "ready");
    let id = ready["link"].as_str().unwrap().to_owned();
    assert_eq!(id.len(), 16);
    let task = tokio::spawn(async move {
        let mut seen = vec![];
        while let Some(Ok(msg)) = ws.next().await {
            let Message::Text(t) = msg else { continue };
            let v: Value = serde_json::from_str(t.as_str()).unwrap();
            assert_eq!(v["type"], "apdu");
            let n = v["id"].as_u64().unwrap();
            let apdu = hex::decode(v["apdu"].as_str().unwrap()).unwrap();
            seen.push(apdu[1]);
            match how {
                Phone::CloseAt(ins) if apdu[1] == ins => {
                    let _ = ws.close(None).await;
                    break;
                }
                Phone::LostAt(ins) if apdu[1] == ins => {
                    ws.send(Message::Text(json!({"type": "lost", "id": n}).to_string().into())).await.unwrap();
                    continue;
                }
                _ => {}
            }
            let r = answer(&card, &apdu);
            let frame = json!({"type": "rapdu", "id": n, "rapdu": hex::encode(r)}).to_string();
            if ws.send(Message::Text(frame.into())).await.is_err() {
                break;
            }
        }
        seen
    });
    (id, task)
}

/// The till on a real loopback socket; its actor is returned for the test to poll.
async fn start(f: Fx, token: Option<String>) -> (SocketAddr, TillHandle, impl std::future::Future<Output = ()>, Fx2) {
    let sales = SalesStore::open(f.dir.path().join("sales.json")).unwrap();
    // No relay: a job without a link would fail, so a passing job proves the link was used.
    let (handle, actor) = till::new(f.cw, "http://127.0.0.1:1".into(), sales, Box::new(|| anyhow::bail!("no relay in this test"))).await;
    let handle = handle.with_pos_token(token);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = till::router(handle.clone()).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, handle, actor, Fx2 { _fed: f._fed, card: f.card, _dir: f.dir })
}

struct Fx2 {
    _fed: FederationTest,
    card: Arc<Mutex<SoftCard>>,
    _dir: tempfile::TempDir,
}

async fn post(addr: SocketAddr, path: &str, link: Option<&str>, body: Value) -> (u16, Value) {
    let mut req = reqwest::Client::new().post(format!("http://{addr}{path}")).json(&body).timeout(Duration::from_secs(120));
    if let Some(l) = link {
        req = req.header("X-Card-Link", l);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// Run `test` next to the actor (which is not `Send`) on this task.
async fn with_actor<T>(actor: impl std::future::Future<Output = ()>, test: impl std::future::Future<Output = T> + Send + 'static) -> T
where
    T: Send + 'static,
{
    let t = tokio::spawn(test);
    let mut actor = Box::pin(actor);
    tokio::select! {
        r = t => r.unwrap(),
        _ = &mut actor => panic!("actor ended early"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_charge_and_a_balance_over_a_link() -> anyhow::Result<()> {
    let f = fx(&[1 << 16, 1 << 14]).await?;
    let (addr, handle, actor, fx2) = start(f, None).await;
    let card = fx2.card.clone();
    let links = handle.links();
    let (charge, seen, bal) = with_actor(actor, async move {
        let (id, phone) = open_link(addr, card.clone(), Phone::Answer).await;
        let charge = post(addr, "/v1/charge", Some(&id), json!({"price_msat": 20_000, "client_ref": "l1"})).await;
        // The till closes the link once the job has answered.
        let seen = tokio::time::timeout(Duration::from_secs(5), phone).await.expect("link closed").unwrap();
        let gone = post(addr, "/v1/card/balance", Some(&id), json!({})).await;
        assert_eq!((gone.0, gone.1["error"].as_str()), (409, Some("card_link_gone")), "{:?}", gone.1);
        let (id2, phone2) = open_link(addr, card, Phone::Answer).await;
        let bal = post(addr, "/v1/card/balance", Some(&id2), json!({})).await;
        tokio::time::timeout(Duration::from_secs(5), phone2).await.expect("link closed").unwrap();
        (charge, seen, bal)
    })
    .await;
    assert_eq!(charge.0, 200, "{:?}", charge.1);
    assert_eq!(charge.1["price_msat"], 20_000);
    // Probe, SELECT, PIN, the card check and the signatures all went over the link.
    for ins in [0xCA, 0xA4, 0x40, 0x11, 0x12] {
        assert!(seen.contains(&ins), "INS {ins:02X} not seen: {seen:02X?}");
    }
    assert_eq!(bal.0, 200, "{:?}", bal.1);
    let paid =
        charge.1["price_msat"].as_u64().unwrap() + charge.1["overpaid_msat"].as_u64().unwrap() + charge.1["fees_msat"].as_u64().unwrap();
    assert_eq!(bal.1["live_msat"].as_u64().unwrap(), (1 << 16) + (1 << 14) - paid, "{:?}", bal.1);
    assert!(links.is_empty(), "every link is gone after its job");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_that_drops_mid_charge_is_no_card_and_moves_nothing() -> anyhow::Result<()> {
    let f = fx(&[1 << 16]).await?;
    let before = f.cw.store.state.live_total_msat();
    let (addr, _handle, actor, fx2) = start(f, None).await;
    let card = fx2.card.clone();
    let (closed, lost, retry, bal) = with_actor(actor, async move {
        // The phone goes away when the till asks for the first signature.
        let (id, phone) = open_link(addr, card.clone(), Phone::CloseAt(0x12)).await;
        let closed = post(addr, "/v1/charge", Some(&id), json!({"price_msat": 20_000, "client_ref": "m1"})).await;
        phone.await.unwrap();
        // The phone loses the card at the first signature.
        let (id, phone) = open_link(addr, card.clone(), Phone::LostAt(0x12)).await;
        let lost = post(addr, "/v1/charge", Some(&id), json!({"price_msat": 20_000, "client_ref": "m1"})).await;
        phone.await.unwrap();
        let (id, phone) = open_link(addr, card.clone(), Phone::Answer).await;
        let bal = post(addr, "/v1/card/balance", Some(&id), json!({})).await;
        phone.await.unwrap();
        // The same ref is a fresh attempt: nothing of the failed ones counts.
        let (id, phone) = open_link(addr, card, Phone::Answer).await;
        let retry = post(addr, "/v1/charge", Some(&id), json!({"price_msat": 20_000, "client_ref": "m1"})).await;
        phone.await.unwrap();
        (closed, lost, retry, bal)
    })
    .await;
    assert_eq!((closed.0, closed.1["error"].as_str()), (409, Some("no_card")), "{:?}", closed.1);
    assert_eq!((lost.0, lost.1["error"].as_str()), (409, Some("no_card")), "{:?}", lost.1);
    assert!(lost.1["detail"].as_str().unwrap().contains("moved away"), "{:?}", lost.1);
    assert_eq!((bal.0, bal.1["live_msat"].as_u64()), (200, Some(before)), "{:?}", bal.1);
    assert_eq!(retry.0, 200, "{:?}", retry.1);
    assert_eq!(retry.1["sale_id"], 1, "the failed attempts recorded no sale");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_link_is_refused_before_anything_runs() -> anyhow::Result<()> {
    let f = fx(&[]).await?;
    let (addr, _handle, actor, _fx2) = start(f, None).await;
    let (c, b) = with_actor(actor, async move {
        let c = post(addr, "/v1/charge", Some("00112233445566778"), json!({"price_msat": 20_000})).await;
        let b = post(addr, "/v1/card/balance", Some("nope"), json!({})).await;
        (c, b)
    })
    .await;
    assert_eq!((c.0, c.1["error"].as_str()), (409, Some("card_link_gone")), "{:?}", c.1);
    assert_eq!((b.0, b.1["error"].as_str()), (409, Some("card_link_gone")), "{:?}", b.1);
    Ok(())
}

async fn call_from(app: axum::Router, peer: &str, bearer: Option<&str>, uri: &str) -> (u16, Value) {
    let mut req = Request::builder().method("GET").uri(uri);
    if let Some(b) = bearer {
        req = req.header("authorization", format!("Bearer {b}"));
    }
    let mut req = req.body(Body::empty()).unwrap();
    req.extensions_mut().insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bearer_is_required_off_loopback_only() -> anyhow::Result<()> {
    let f = fx(&[]).await?;
    let dir = f.dir.path().to_owned();
    let sales = SalesStore::open(dir.join("sales.json"))?;
    let (handle, _actor) = till::new(f.cw, "http://127.0.0.1:1".into(), sales, Box::new(|| anyhow::bail!("unused"))).await;
    let token = "ab".repeat(32);
    let app = till::router(handle.clone().with_pos_token(Some(token.clone())));
    let lan = "192.168.1.50:40000";
    assert_eq!(call_from(app.clone(), "127.0.0.1:5000", None, "/v1/health").await.0, 200);
    let denied = call_from(app.clone(), lan, None, "/v1/health").await;
    assert_eq!(denied, (401, json!({"ok": false, "error": "unauthorized"})));
    assert_eq!(call_from(app.clone(), lan, Some(&"cd".repeat(32)), "/v1/health").await.0, 401);
    assert_eq!(call_from(app.clone(), lan, Some(&token), "/v1/health").await.0, 200);
    // The link needs it too (401 before the upgrade is even looked at).
    assert_eq!(call_from(app.clone(), lan, None, "/v1/card/link").await.0, 401);
    // Without a token nothing is asked (a till without one only listens on loopback).
    assert_eq!(call_from(till::router(handle), lan, None, "/v1/health").await.0, 200);
    Ok(())
}

/// A token with the pay scope (`till pair --scope pay`): the pay routes, health and the card link,
/// from every peer; nothing else, loopback included. The POS token still opens everything.
#[tokio::test(flavor = "multi_thread")]
async fn the_pay_token_opens_the_pay_routes_and_nothing_else() -> anyhow::Result<()> {
    let f = fx(&[]).await?;
    let dir = f.dir.path().to_owned();
    let sales = SalesStore::open(dir.join("sales.json"))?;
    let (handle, _actor) = till::new(f.cw, "http://127.0.0.1:1".into(), sales, Box::new(|| anyhow::bail!("unused"))).await;
    let (token, pay) = ("ab".repeat(32), "cd".repeat(32));
    // Set before or after the POS token, the pay token is kept.
    for app in [
        till::router(handle.clone().with_pay_token(Some(pay.clone())).with_pos_token(Some(token.clone()))),
        till::router(handle.clone().with_pos_token_everywhere(token.clone()).with_pay_token(Some(pay.clone()))),
    ] {
        for peer in ["127.0.0.1:5000", "192.168.1.50:40000"] {
            assert_eq!(call_from(app.clone(), peer, Some(&pay), "/v1/health").await.0, 200, "{peer}");
            // A till without pays answers its pay routes 503, past the auth.
            for path in ["/v1/pays", "/v1/pay/1"] {
                let (s, v) = call_from(app.clone(), peer, Some(&pay), path).await;
                assert_eq!((s, v["error"].as_str()), (503, Some("lightning_unavailable")), "{peer} {path}");
            }
            // The card link: past the auth (refused only as a request that is no upgrade).
            assert_ne!(call_from(app.clone(), peer, Some(&pay), "/v1/card/link").await.0, 403, "{peer}");
            assert_ne!(call_from(app.clone(), peer, Some(&pay), "/v1/card/link").await.0, 401, "{peer}");
            for path in ["/v1/status", "/v1/sales", "/v1/topups/waiting", "/v1/offline/kit?phone=00"] {
                let (s, v) = call_from(app.clone(), peer, Some(&pay), path).await;
                assert_eq!((s, v["error"].as_str()), (403, Some("forbidden")), "{peer} {path}");
            }
            assert_eq!(call_from(app.clone(), peer, Some(&token), "/v1/sales").await.0, 200, "{peer}");
        }
    }
    Ok(())
}

#[test]
fn pairing_makes_one_private_token() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    assert_eq!(pos::read_token(dir.path())?, None);
    let (t, made) = pos::ensure_token(dir.path())?;
    assert!(made);
    assert_eq!(t.len(), 64);
    assert!(t.bytes().all(|b| b.is_ascii_hexdigit()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(pos::token_path(dir.path()))?.permissions().mode() & 0o777, 0o600);
    }
    assert_eq!(pos::ensure_token(dir.path())?, (t.clone(), false), "pairing again keeps the token");
    assert_eq!(pos::read_token(dir.path())?, Some(t.clone()));
    assert_eq!(pos::pair_text("http://10.0.0.5:9877", &t), format!("fedimintcard-till:http://10.0.0.5:9877?t={t}"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_refuses_the_lan_without_a_token() -> anyhow::Result<()> {
    let f = fx(&[]).await?;
    let err = till::serve(f.cw, "0.0.0.0:0".parse()?, "http://127.0.0.1:1".into(), None, false).await.unwrap_err();
    assert!(err.to_string().contains("till pair"), "{err:#}");
    Ok(())
}

/// `till serve --require-pos-token` (the StartOS package): behind a proxy every request may come
/// from a local address, so loopback is no exemption.
#[tokio::test(flavor = "multi_thread")]
async fn require_pos_token_guards_loopback_too() -> anyhow::Result<()> {
    let f = fx(&[]).await?;
    let dir = f.dir.path().to_owned();
    let sales = SalesStore::open(dir.join("sales.json"))?;
    let (handle, _actor) = till::new(f.cw, "http://127.0.0.1:1".into(), sales, Box::new(|| anyhow::bail!("unused"))).await;
    let token = "ab".repeat(32);
    let app = till::router(handle.with_pos_token_everywhere(token.clone()));
    for peer in ["127.0.0.1:5000", "[::1]:5000", "10.0.3.1:40000", "192.168.1.50:40000"] {
        for path in ["/v1/health", "/v1/status", "/v1/sales", "/v1/card/link"] {
            let denied = call_from(app.clone(), peer, None, path).await;
            assert_eq!(denied, (401, json!({"ok": false, "error": "unauthorized"})), "{peer} {path}");
            assert_eq!(call_from(app.clone(), peer, Some(&"cd".repeat(32)), path).await.0, 401, "{peer} {path}");
        }
        assert_eq!(call_from(app.clone(), peer, Some(&token), "/v1/health").await.0, 200, "{peer}");
    }
    Ok(())
}

/// The real server, started the way the package starts it: the token is made if missing, and a
/// request over loopback without it is refused.
#[tokio::test(flavor = "multi_thread")]
async fn serve_with_require_pos_token_makes_the_token_and_refuses_loopback() -> anyhow::Result<()> {
    let f = fx(&[]).await?;
    let dir = f.dir.path().to_owned();
    assert_eq!(pos::read_token(&dir)?, None);
    // Find a free port, then let serve bind it on every interface (as the package does).
    let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let listen: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let server = tokio::task::spawn_local(till::serve(f.cw, listen, "http://127.0.0.1:1".into(), None, true));
            let url = format!("http://127.0.0.1:{port}/v1/health");
            let client = reqwest::Client::new();
            let mut status = None;
            for _ in 0..100 {
                if let Ok(r) = client.get(&url).send().await {
                    status = Some(r.status().as_u16());
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_eq!(status, Some(401), "loopback without the token");
            let token = pos::read_token(&dir).unwrap().expect("serve made the POS token");
            let ok = client.get(&url).bearer_auth(&token).send().await.unwrap();
            assert_eq!(ok.status().as_u16(), 200);
            server.abort();
            drop(f._fed);
        })
        .await;
    Ok(())
}
