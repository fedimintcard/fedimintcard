//! Paying an invoice with a card (`POST /v1/pay`, `GET /v1/pay/{id}`, `GET /v1/pays`): the
//! till's routes against the in-process federation of `till_service.rs` and a `SoftCard`, with a
//! fake wallet side (the `PayLightning` trait) that remembers what it paid, as a federation does.
//! The last test pays through the real wallet, inside an in-process federation with the ln
//! module (no gateway needed).
//!
//! Every failure rule has a test: nothing touches the card before the invoice passes; a repeated
//! `client_ref` neither charges nor pays twice; a certain failure is `pay_failed` with the value
//! in the till; an unknown outcome stays `paying` and is resolved, never paid again; and a crash
//! after each step leaves, after a restart, a record that neither loses nor doubles money.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use fedimint_card_host_card::{Card, SoftCard, open_pin_session};
use fedimint_card_sidecar::glue::{CardWallet, CrashPoint};
use fedimint_card_sidecar::pay::{self, PayConfig, PayCrash, PayLightning, PayState, PayStore, Pays};
use fedimint_card_sidecar::sales::SalesStore;
use fedimint_card_sidecar::state::StateStore;
use fedimint_card_sidecar::till::{self, TillHandle};
use fedimint_card_sidecar::wallet::{Wallet, WalletPay};
use fedimint_client::ClientHandleArc;
use fedimint_client::transaction::TransactionBuilder;
use fedimint_core::Amount;
use fedimint_core::bitcoin::hashes::{Hash as _, sha256};
use fedimint_core::core::OperationId;
use fedimint_core::secp256k1::schnorr::Signature;
use fedimint_core::secp256k1::{SECP256K1, SecretKey, XOnlyPublicKey};
use fedimint_dummy_client::{DummyClientInit, DummyClientModule};
use fedimint_dummy_server::DummyInit;
use fedimint_mint_client::MintClientInit;
use fedimint_mint_server::MintInit;
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;
use futures::future::BoxFuture;
use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
use serde_json::{Value, json};
use tower::ServiceExt as _;

// ------------------------------------------------------------------ the fake wallet side

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Paid, with a gateway fee of 300 msat.
    Pay,
    /// Refused before funding (a gateway asking more than the reserve).
    Refuse,
    /// Refused before funding: the wallet does not hold the amount.
    Insufficient,
    /// Funded, failed, refunded.
    Refund,
    /// Funded and still in flight.
    InFlight,
}

/// What a till wallet does with invoices, remembered as the federation client remembers it:
/// a funded payment is found again by its invoice (`status`) or operation id (`resolve`), and
/// paying an invoice again answers the earlier payment.
struct FakeLn {
    mode: Mutex<Mode>,
    by_invoice: Mutex<HashMap<String, String>>,
    by_op: Mutex<HashMap<String, WalletPay>>,
    /// `pay` calls per invoice.
    calls: Mutex<HashMap<String, usize>>,
    /// How long `pay` takes before it answers.
    delay: Mutex<Duration>,
    statuses: AtomicUsize,
}

const FEE_PAID: u64 = 300;

impl FakeLn {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            mode: Mutex::new(Mode::Pay),
            by_invoice: Mutex::new(HashMap::new()),
            by_op: Mutex::new(HashMap::new()),
            calls: Mutex::new(HashMap::new()),
            delay: Mutex::new(Duration::ZERO),
            statuses: AtomicUsize::new(0),
        })
    }

    fn set(&self, m: Mode) {
        *self.mode.lock().unwrap() = m;
    }

    fn delay(&self, d: Duration) {
        *self.delay.lock().unwrap() = d;
    }

    fn calls(&self, bolt11: &str) -> usize {
        self.calls.lock().unwrap().get(bolt11).copied().unwrap_or(0)
    }

    fn total_calls(&self) -> usize {
        self.calls.lock().unwrap().values().sum()
    }

    /// The federation settles the payment of `bolt11` in flight.
    fn settle(&self, bolt11: &str, paid: bool) {
        let op = self.by_invoice.lock().unwrap().get(bolt11).cloned().expect("a funded payment");
        let o = if paid {
            WalletPay::Paid { preimage: "77".repeat(32), fee_msat: FEE_PAID, operation_id: op.clone() }
        } else {
            WalletPay::Failed { reason: "refunded".into(), operation_id: Some(op.clone()), insufficient_funds: false }
        };
        self.by_op.lock().unwrap().insert(op, o);
    }

    /// The wallet paid `bolt11` outside the pay route.
    fn paid_elsewhere(&self, bolt11: &str) {
        let op = hex::encode(rand::random::<[u8; 32]>());
        self.by_invoice.lock().unwrap().insert(bolt11.to_owned(), op.clone());
        self.by_op.lock().unwrap().insert(op.clone(), WalletPay::Paid { preimage: "11".repeat(32), fee_msat: 0, operation_id: op });
    }

    fn status_of(&self, bolt11: &str) -> Option<WalletPay> {
        let op = self.by_invoice.lock().unwrap().get(bolt11).cloned()?;
        self.by_op.lock().unwrap().get(&op).cloned()
    }
}

impl PayLightning for FakeLn {
    fn pay(&self, bolt11: String, max_fee_msat: u64, _wait: Duration) -> BoxFuture<'_, anyhow::Result<WalletPay>> {
        Box::pin(async move {
            *self.calls.lock().unwrap().entry(bolt11.clone()).or_default() += 1;
            let delay = *self.delay.lock().unwrap();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            // Paying again answers the earlier payment, unless it was refunded.
            if let Some(o) = self.status_of(&bolt11)
                && !matches!(o, WalletPay::Failed { .. })
            {
                return Ok(o);
            }
            let op = hex::encode(rand::random::<[u8; 32]>());
            let o = match *self.mode.lock().unwrap() {
                Mode::Pay => {
                    assert!(FEE_PAID <= max_fee_msat, "the fee cap reaches the wallet");
                    WalletPay::Paid { preimage: "99".repeat(32), fee_msat: FEE_PAID, operation_id: op.clone() }
                }
                Mode::Refuse => {
                    return Ok(WalletPay::Failed { reason: "the gateway asks more".into(), operation_id: None, insufficient_funds: false });
                }
                Mode::Insufficient => {
                    return Ok(WalletPay::Failed { reason: "insufficient funds".into(), operation_id: None, insufficient_funds: true });
                }
                Mode::Refund => WalletPay::Failed { reason: "no route".into(), operation_id: Some(op.clone()), insufficient_funds: false },
                Mode::InFlight => WalletPay::Unknown { reason: "in flight".into(), operation_id: op.clone() },
            };
            self.by_invoice.lock().unwrap().insert(bolt11, op.clone());
            self.by_op.lock().unwrap().insert(op, o.clone());
            Ok(o)
        })
    }

    fn resolve(&self, operation_id: String, _wait: Duration) -> BoxFuture<'_, anyhow::Result<WalletPay>> {
        Box::pin(async move { self.by_op.lock().unwrap().get(&operation_id).cloned().ok_or_else(|| anyhow::anyhow!("no operation")) })
    }

    fn status(&self, bolt11: String, _wait: Duration) -> BoxFuture<'_, anyhow::Result<Option<WalletPay>>> {
        Box::pin(async move {
            self.statuses.fetch_add(1, Ordering::SeqCst);
            Ok(self.status_of(&bolt11))
        })
    }
}

// ------------------------------------------------------------------ the till

/// A card shared with the test: counts signatures.
struct Shared(Rc<RefCell<SoftCard>>, Rc<Cell<u32>>);
impl Card for Shared {
    fn gen_key(&mut self, s: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.borrow_mut().gen_key(s)
    }
    fn gen_key_overwrite(&mut self, s: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.borrow_mut().gen_key_overwrite(s)
    }
    fn get_pub(&mut self, s: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.borrow_mut().get_pub(s)
    }
    fn sign(&mut self, s: u8, m: &[u8; 32]) -> anyhow::Result<Signature> {
        self.1.set(self.1.get() + 1);
        self.0.borrow_mut().sign(s, m)
    }
    fn apdu(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.0.borrow_mut().apdu(apdu)
    }
}

type Actor = Pin<Box<dyn Future<Output = ()>>>;

struct Fx {
    /// Keep alive: dropping it stops the guardians.
    _fed: FederationTest,
    dir: tempfile::TempDir,
    handle: TillHandle,
    actor: Actor,
    ln: Arc<FakeLn>,
    /// Times the till opened the card.
    opens: Rc<Cell<u32>>,
    signs: Rc<Cell<u32>>,
    /// The card behind the opener.
    card: Rc<RefCell<SoftCard>>,
}

/// Give `client` ecash out of thin air through the dummy module.
async fn fund(client: &ClientHandleArc, amount: Amount) -> anyhow::Result<()> {
    let dummy = client.get_first_module::<DummyClientModule>()?;
    let op = OperationId::new_random();
    let range = client
        .finalize_and_submit_transaction(op, "fund", |_| (), TransactionBuilder::new().with_inputs(dummy.create_input(amount)))
        .await?;
    client.await_primary_bitcoin_module_outputs(op, range.into_iter().collect()).await?;
    Ok(())
}

fn cfg() -> PayConfig {
    PayConfig { enabled: true, max_fee_msat: 2_000, margin_msat: 50, margin_bps: 100, wait: Duration::from_secs(5), ..PayConfig::default() }
}

async fn fixture(fixtures: Fixtures) -> anyhow::Result<(FederationTest, ClientHandleArc)> {
    assert!(
        std::env::var_os("FM_TEST_USE_REAL_DAEMONS").is_none(),
        "FM_TEST_USE_REAL_DAEMONS is set: these tests need the in-process fake bitcoin backend"
    );
    let fed = tokio::time::timeout(Duration::from_secs(60), fixtures.new_fed_builder(0).build()).await?;
    let client = fed.new_client().await;
    fund(&client, Amount::from_sats(10_000)).await?;
    Ok((fed, client))
}

/// A till with a topped-up card behind its opener, and pays with `cfg` on the fake wallet side.
/// `crash` is the card wallet's crash point for every charge.
async fn fx_with(cfg: PayConfig, crash: Option<CrashPoint>) -> anyhow::Result<Fx> {
    let (fed, client) = fixture(Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit)).await?;
    let wallet = Wallet::from_client(client).await?;
    let ln = FakeLn::new();
    let (dir, handle, actor, opens, signs, card) = till_on(wallet, cfg, ln.clone(), crash).await?;
    Ok(Fx { _fed: fed, dir, handle, actor, ln, opens, signs, card })
}

type Till = (tempfile::TempDir, TillHandle, Actor, Rc<Cell<u32>>, Rc<Cell<u32>>, Rc<RefCell<SoftCard>>);

async fn till_on(wallet: Wallet, cfg: PayConfig, ln: Arc<dyn PayLightning>, crash: Option<CrashPoint>) -> anyhow::Result<Till> {
    let dir = tempfile::tempdir()?;
    let store = StateStore::open(dir.path().join("card_state.json"))?;
    let card = Rc::new(RefCell::new(SoftCard::new()));
    let signs = Rc::new(Cell::new(0));
    let mut cw = CardWallet::new(wallet, store, Box::new(Shared(card.clone(), signs.clone())));
    cw.card_setup(false)?;
    open_pin_session(cw.card.as_mut())?;
    cw.topup(&[1 << 17, 1 << 16]).await?;
    cw.crash_point = crash;
    let sales = SalesStore::open(dir.path().join("sales.json"))?;
    let opens = Rc::new(Cell::new(0));
    let (o, s, c) = (opens.clone(), signs.clone(), card.clone());
    let opener = Box::new(move || {
        o.set(o.get() + 1);
        Ok(Box::new(Shared(c.clone(), s.clone())) as Box<dyn Card>)
    });
    let (handle, actor) = till::new(cw, closed_port_url(), sales, opener).await;
    let pays = Pays::new(PayStore::open(PayStore::path_in(dir.path()))?, ln, cfg, Currency::Regtest);
    Ok((dir, handle.with_pays(pays), Box::pin(actor), opens, signs, card))
}

fn closed_port_url() -> String {
    let p = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    format!("http://127.0.0.1:{p}")
}

/// Run `f` while polling the till's actor (it is not `Send`) on this task.
async fn with_actor<T: Send + 'static>(actor: &mut Actor, f: impl Future<Output = T> + Send + 'static) -> T {
    let t = tokio::spawn(f);
    tokio::select! {
        r = t => r.unwrap(),
        _ = actor.as_mut() => panic!("the actor ended"),
    }
}

async fn call(h: &TillHandle, method: &str, uri: &str, body: Value) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(if body.is_null() { Body::empty() } else { Body::from(body.to_string()) })
        .unwrap();
    let resp = till::router(h.clone()).oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

impl Fx {
    async fn post(&mut self, body: Value) -> (u16, Value) {
        let h = self.handle.clone();
        with_actor(&mut self.actor, async move { call(&h, "POST", "/v1/pay", body).await }).await
    }

    async fn get(&self, uri: &str) -> (u16, Value) {
        call(&self.handle, "GET", uri, Value::Null).await
    }

    fn sales_for(&self, client_ref: &str) -> usize {
        let s = SalesStore::open(self.dir.path().join("sales.json")).unwrap();
        s.file.sales.iter().filter(|r| r.client_ref.as_deref() == Some(&pay::charge_ref(client_ref))).count()
    }

    fn state_of(&self, pay_id: u64) -> PayState {
        self.handle.pays().unwrap().get(pay_id).unwrap().state
    }

    /// A restart of the pay layer: the records are read back from `pays.json`; the card wallet,
    /// the sales and the wallet side (the federation's memory) are the same.
    /// Rewrite pay `pay_id` in `pays.json` with `edit` (then [`Self::restart`] reads it back).
    fn edit_on_disk(&self, pay_id: u64, edit: impl FnOnce(&mut pay::PayRec)) {
        let mut disk = PayStore::open(PayStore::path_in(self.dir.path())).unwrap();
        let mut r = disk.get(pay_id).unwrap().clone();
        edit(&mut r);
        disk.put(&r).unwrap();
    }

    fn restart(&mut self, cfg: PayConfig) {
        let store = PayStore::open(PayStore::path_in(self.dir.path())).unwrap();
        self.handle = self.handle.clone().with_pays(Pays::new(store, self.ln.clone(), cfg, Currency::Regtest));
    }
}

fn invoice(msat: u64, expiry: Duration) -> String {
    let key = SecretKey::from_slice(&[9u8; 32]).unwrap();
    InvoiceBuilder::new(Currency::Regtest)
        .description("pay route test".into())
        .payment_hash(sha256::Hash::hash(&rand::random::<[u8; 32]>()))
        .payment_secret(PaymentSecret([42u8; 32]))
        .current_timestamp()
        .min_final_cltv_expiry_delta(144)
        .expiry_time(expiry)
        .amount_milli_satoshis(msat)
        .build_signed(|m| SECP256K1.sign_ecdsa_recoverable(m, &key))
        .unwrap()
        .to_string()
}

fn hour_invoice(msat: u64) -> String {
    invoice(msat, Duration::from_secs(3600))
}

fn req(bolt11: &str, client_ref: &str) -> Value {
    json!({"bolt11": bolt11, "max_fee_msat": 1_000, "client_ref": client_ref})
}

/// 10,000 msat + a 1,000 msat fee reserve + 50 msat + 1 % margin.
const PRICE: u64 = 10_000 + 1_000 + 50 + 100;

// ------------------------------------------------------------------ the tests

#[tokio::test(flavor = "multi_thread")]
async fn a_card_pays_an_invoice_and_a_repeat_neither_charges_nor_pays_again() -> anyhow::Result<()> {
    let mut f = fx_with(cfg(), None).await?;
    let bolt11 = hour_invoice(10_000);
    let (status, b) = f.post(req(&bolt11, "a")).await;
    assert_eq!(status, 200, "{b}");
    assert_eq!((b["ok"].as_bool(), b["state"].as_str(), b["pay_id"].as_u64()), (Some(true), Some("paid"), Some(1)));
    assert_eq!(b["preimage"], "99".repeat(32));
    assert_eq!(
        (b["invoice_msat"].as_u64(), b["fee_reserve_msat"].as_u64(), b["margin_msat"].as_u64(), b["price_msat"].as_u64()),
        (Some(10_000), Some(1_000), Some(150), Some(PRICE))
    );
    assert_eq!((b["fee_paid_msat"].as_u64(), b["fee_refund_msat"].as_u64()), (Some(FEE_PAID), Some(1_000 - FEE_PAID)));
    assert!(b["sale_id"].as_u64().is_some() && b["txid"].as_str().is_some_and(|t| t.len() == 64), "{b}");
    // Nothing secret in the answer: no invoice (its payment secret), no operation id.
    assert!(!b.to_string().contains(&bolt11) && b.get("bolt11").is_none() && b.get("operation_id").is_none(), "{b}");
    assert_eq!((f.sales_for("a"), f.ln.calls(&bolt11)), (1, 1));
    let signed = f.signs.get();

    // The same pay again: the same answer, no charge, no payment.
    let (status, again) = f.post(req(&bolt11, "a")).await;
    assert_eq!((status, &again), (200, &b));
    assert_eq!((f.sales_for("a"), f.ln.calls(&bolt11), f.signs.get()), (1, 1, signed));

    // The same ref for another invoice, or another fee reserve: a conflict, nothing charged.
    let (status, c) = f.post(req(&hour_invoice(10_000), "a")).await;
    assert_eq!((status, c["error"].as_str()), (409, Some("conflict")), "{c}");
    let (status, c) = f.post(json!({"bolt11": bolt11, "max_fee_msat": 999, "client_ref": "a"})).await;
    assert_eq!((status, c["error"].as_str()), (409, Some("conflict")), "{c}");
    // The same invoice under another ref: never paid (or charged) twice.
    let (status, c) = f.post(req(&bolt11, "b")).await;
    assert_eq!((status, c["error"].as_str()), (409, Some("invoice_in_use")), "{c}");
    assert_eq!((f.ln.total_calls(), f.signs.get(), f.sales_for("b")), (1, signed, 0));

    // The record and the list. The preimage goes only to the caller (its POST, and that POST
    // replayed, above), never to whoever reads the record.
    let (status, g) = f.get("/v1/pay/1").await;
    assert_eq!((status, g["state"].as_str(), g["fee_paid_msat"].as_u64()), (200, Some("paid"), Some(FEE_PAID)), "{g}");
    assert!(g.get("preimage").is_none() && !g.to_string().contains(&"99".repeat(32)), "{g}");
    assert_eq!(g["client_ref"], "a");
    assert_eq!(f.get("/v1/pay/2").await.0, 404);
    assert_eq!(f.get("/v1/pay/x").await.0, 404);
    let (status, l) = f.get("/v1/pays").await;
    assert_eq!((status, l["count"].as_u64(), l["total_price_msat"].as_u64()), (200, Some(1), Some(PRICE)), "{l}");
    assert_eq!(l["pays"][0]["state"], "paid");
    assert!(l["pays"][0].get("preimage").is_none(), "{l}");

    // On disk, 0600, and read back.
    let path = PayStore::path_in(f.dir.path());
    let disk = PayStore::open(&path)?;
    assert_eq!(disk.get(1).unwrap().state, PayState::Paid);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_touches_the_card_before_the_invoice_and_the_limits_pass() -> anyhow::Result<()> {
    let mut f = fx_with(PayConfig { max_invoice_msat: 50_000, daily_cap_msat: 30_000, ..cfg() }, None).await?;
    let ok = hour_invoice(10_000);
    let key = SecretKey::from_slice(&[9u8; 32]).unwrap();
    let mainnet = InvoiceBuilder::new(Currency::Bitcoin)
        .description("mainnet".into())
        .payment_hash(sha256::Hash::hash(b"m"))
        .payment_secret(PaymentSecret([42u8; 32]))
        .current_timestamp()
        .min_final_cltv_expiry_delta(144)
        .amount_milli_satoshis(10_000)
        .build_signed(|m| SECP256K1.sign_ecdsa_recoverable(m, &key))?
        .to_string();
    let no_amount = InvoiceBuilder::new(Currency::Regtest)
        .description("open".into())
        .payment_hash(sha256::Hash::hash(b"o"))
        .payment_secret(PaymentSecret([42u8; 32]))
        .current_timestamp()
        .min_final_cltv_expiry_delta(144)
        .build_signed(|m| SECP256K1.sign_ecdsa_recoverable(m, &key))?
        .to_string();
    let cases: Vec<(Value, u16, &str)> = vec![
        (json!("not json"), 400, "bad_request"),
        (json!({"bolt11": ok}), 400, "bad_request"),
        (json!({"bolt11": ok, "client_ref": ""}), 400, "bad_request"),
        (json!({"bolt11": ok, "client_ref": "x".repeat(pay::MAX_CLIENT_REF + 1)}), 400, "bad_request"),
        (json!({"client_ref": "r"}), 400, "bad_request"),
        (json!({"bolt11": ok, "client_ref": "r", "pin": "12a4"}), 400, "bad_request"),
        (req("lnbcrt1garbage", "r"), 400, "bad_invoice"),
        (req(&no_amount, "r"), 400, "bad_invoice"),
        (req(&mainnet, "r"), 400, "wrong_network"),
        (req(&invoice(10_000, Duration::from_secs(60)), "r"), 400, "invoice_expired"),
        (req(&hour_invoice(50_001), "r"), 400, "over_limit"),
        // 30,000 msat a day: this one alone is over.
        (req(&hour_invoice(29_000), "r"), 403, "daily_cap"),
    ];
    for (body, status, code) in cases {
        let (s, b) = if body.is_string() {
            let h = f.handle.clone();
            with_actor(&mut f.actor, async move {
                let req = Request::builder().method("POST").uri("/v1/pay").body(Body::from("not json")).unwrap();
                let resp = till::router(h).oneshot(req).await.unwrap();
                (resp.status().as_u16(), Value::Null)
            })
            .await
        } else {
            f.post(body.clone()).await
        };
        assert_eq!(s, status, "{body}: {b}");
        if !b.is_null() {
            assert_eq!(b["error"], code, "{body}: {b}");
            assert!(!b.to_string().contains("12a4"), "a PIN is never echoed: {b}");
        }
    }
    // An invoice this wallet already paid outside the route.
    let elsewhere = hour_invoice(10_000);
    f.ln.paid_elsewhere(&elsewhere);
    let (s, b) = f.post(req(&elsewhere, "r")).await;
    assert_eq!((s, b["error"].as_str()), (409, Some("invoice_in_use")), "{b}");
    assert_eq!((f.opens.get(), f.signs.get(), f.ln.total_calls()), (0, 0, 0), "the card was never opened, nothing paid");
    assert_eq!(f.handle.pays().unwrap().store.lock().unwrap().all().len(), 0, "nothing recorded");

    // Paying is off: refused before anything else. GET still answers.
    let off = Pays::new(PayStore::open(PayStore::path_in(f.dir.path()))?, f.ln.clone(), PayConfig::default(), Currency::Regtest);
    let h = f.handle.clone().with_pays(off);
    let (s, b) = call(&h, "POST", "/v1/pay", req(&ok, "r")).await;
    assert_eq!((s, b["error"].as_str()), (503, Some("pay_disabled")), "{b}");
    assert_eq!(call(&h, "GET", "/v1/pays", Value::Null).await.0, 200);

    // Within the cap: paid. The next one would pass the cap.
    let (s, b) = f.post(req(&ok, "r")).await;
    assert_eq!((s, b["state"].as_str()), (200, Some("paid")), "{b}");
    let (s, b) = f.post(req(&hour_invoice(18_000), "r2")).await;
    assert_eq!((s, b["error"].as_str()), (403, Some("daily_cap")), "{b}");
    assert_eq!(f.opens.get(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_certain_failure_is_pay_failed_and_a_refused_charge_can_be_tapped_again() -> anyhow::Result<()> {
    let mut f = fx_with(cfg(), None).await?;

    // Funded, failed, refunded: pay_failed, the card's value stays with the till.
    f.ln.set(Mode::Refund);
    let refunded = hour_invoice(10_000);
    let (s, b) = f.post(req(&refunded, "refund")).await;
    assert_eq!((s, b["error"].as_str(), b["state"].as_str()), (502, Some("pay_failed"), Some("pay_failed")), "{b}");
    assert_eq!((b["price_msat"].as_u64(), b["pay_id"].as_u64()), (Some(PRICE), Some(1)));
    assert!(b["sale_id"].as_u64().is_some(), "{b}");
    assert!(b["detail"].as_str().unwrap().contains("owed to the holder"), "{b}");
    assert_eq!((f.sales_for("refund"), f.ln.calls(&refunded)), (1, 1));
    // A repeat: the same answer; neither charged nor paid again, even with the gateway back.
    f.ln.set(Mode::Pay);
    let (s, again) = f.post(req(&refunded, "refund")).await;
    assert_eq!((s, &again), (502, &b));
    assert_eq!((f.sales_for("refund"), f.ln.calls(&refunded)), (1, 1));
    // The sweeper leaves it alone too.
    pay::sweep(&f.handle).await;
    assert_eq!((f.state_of(1), f.ln.calls(&refunded)), (PayState::PayFailed, 1));

    // Refused before funding (a gateway asking more than the reserve): certain, pay_failed.
    f.ln.set(Mode::Refuse);
    let refused = hour_invoice(10_000);
    let (s, b) = f.post(req(&refused, "refuse")).await;
    assert_eq!((s, b["state"].as_str()), (502, Some("pay_failed")), "{b}");
    assert_eq!(f.sales_for("refuse"), 1);

    // A wrong PIN: the card is not charged (not_charged); the same ref with the right PIN pays.
    f.ln.set(Mode::Pay);
    let tapped = hour_invoice(10_000);
    let signed = f.signs.get();
    let (s, b) = f.post(json!({"bolt11": tapped, "max_fee_msat": 1_000, "client_ref": "pin", "pin": "9999"})).await;
    assert_eq!((s, b["error"].as_str(), b["state"].as_str()), (403, Some("wrong_pin"), Some("not_charged")), "{b}");
    assert!(!b.to_string().contains("9999"), "{b}");
    assert_eq!((f.signs.get(), f.sales_for("pin"), f.ln.calls(&tapped)), (signed, 0, 0));
    let id = b["pay_id"].as_u64().unwrap();
    // Not charged does not count to the cap, and the sweeper leaves it.
    pay::sweep(&f.handle).await;
    assert_eq!(f.state_of(id), PayState::NotCharged);
    let (s, b) = f.post(json!({"bolt11": tapped, "max_fee_msat": 1_000, "client_ref": "pin", "pin": "1234"})).await;
    assert_eq!((s, b["state"].as_str(), b["pay_id"].as_u64()), (200, Some("paid"), Some(id)), "{b}");
    assert_eq!((f.sales_for("pin"), f.ln.calls(&tapped)), (1, 1));

    // More than the card holds: not charged, nothing paid.
    let big = hour_invoice(90_000_000);
    let (s, b) = f.post(req(&big, "big")).await;
    assert_eq!((s, b["error"].as_str(), b["state"].as_str()), (402, Some("insufficient"), Some("not_charged")), "{b}");
    assert_eq!(f.ln.calls(&big), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pay_records_the_card_that_paid_and_only_a_till_on_ops_refunds() -> anyhow::Result<()> {
    let mut f = fx_with(cfg(), None).await?;
    let card_id = fedimint_card_sidecar::ops::short_id_of(&f.card.borrow_mut().get_pub(0)?);

    // The card's short id is read at the charge's tap and kept with the pay.
    f.ln.set(Mode::Refund);
    let (s, b) = f.post(req(&hour_invoice(10_000), "failed")).await;
    assert_eq!((s, b["state"].as_str(), b["short_id"].as_str()), (502, Some("pay_failed"), Some(card_id.as_str())), "{b}");
    assert_eq!(f.handle.pays().unwrap().get(1).unwrap().short_id.as_deref(), Some(card_id.as_str()));
    assert_eq!(PayStore::open(PayStore::path_in(f.dir.path()))?.get(1).unwrap().short_id.as_deref(), Some(card_id.as_str()));
    f.ln.set(Mode::Pay);
    let (s, b) = f.post(req(&hour_invoice(10_000), "paid")).await;
    assert_eq!((s, b["short_id"].as_str()), (200, Some(card_id.as_str())), "{b}");

    // A refund is a load recorded in the registry: a till off ops refunds nothing, and the pay
    // stays pay_failed, owed to the holder.
    let refund = |id: u64| {
        let h = f.handle.clone();
        async move { call(&h, "POST", &format!("/v1/pay/{id}/refund"), json!({})).await }
    };
    let (s, r) = with_actor(&mut f.actor, refund(1)).await;
    assert_eq!((s, r["error"].as_str(), r["state"].as_str()), (503, Some("ops_not_connected"), Some("pay_failed")), "{r}");
    assert_eq!(f.state_of(1), PayState::PayFailed);
    // Only a pay_failed pay is refunded; an unknown one is 404.
    let (s, r) = with_actor(&mut f.actor, refund(2)).await;
    assert_eq!((s, r["error"].as_str(), r["state"].as_str()), (409, Some("not_refundable"), Some("paid")), "{r}");
    let (s, r) = with_actor(&mut f.actor, refund(3)).await;
    assert_eq!((s, r["error"].as_str()), (404, Some("unknown_pay")), "{r}");
    assert_eq!(f.state_of(2), PayState::Paid);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_outcome_stays_paying_and_is_resolved_never_paid_again() -> anyhow::Result<()> {
    let mut f = fx_with(cfg(), None).await?;

    // In flight: 202, `paying` with its operation; a repeat follows it and pays nothing.
    f.ln.set(Mode::InFlight);
    let slow = hour_invoice(10_000);
    let (s, b) = f.post(req(&slow, "slow")).await;
    assert_eq!((s, b["error"].as_str(), b["state"].as_str()), (202, Some("undecided"), Some("paying")), "{b}");
    let id = b["pay_id"].as_u64().unwrap();
    assert!(f.handle.pays().unwrap().get(id).unwrap().operation_id.is_some());
    f.ln.set(Mode::Pay);
    let (s, b) = f.post(req(&slow, "slow")).await;
    assert_eq!((s, b["state"].as_str()), (202, Some("paying")), "{b}");
    pay::sweep(&f.handle).await;
    assert_eq!((f.state_of(id), f.ln.calls(&slow), f.sales_for("slow")), (PayState::Paying, 1, 1));
    // The federation settles it: the sweeper records it, and the repeat answers it.
    f.ln.settle(&slow, true);
    pay::sweep(&f.handle).await;
    assert_eq!(f.state_of(id), PayState::Paid);
    let (s, b) = f.post(req(&slow, "slow")).await;
    assert_eq!((s, b["preimage"].as_str()), (200, Some("77".repeat(32).as_str())), "{b}");
    assert_eq!((f.ln.calls(&slow), f.sales_for("slow")), (1, 1));

    // In flight, then refunded: pay_failed, never paid again.
    f.ln.set(Mode::InFlight);
    let lost = hour_invoice(10_000);
    let (s, b) = f.post(req(&lost, "lost")).await;
    assert_eq!(s, 202, "{b}");
    let lost_id = b["pay_id"].as_u64().unwrap();
    f.ln.settle(&lost, false);
    f.ln.set(Mode::Pay);
    pay::sweep(&f.handle).await;
    assert_eq!((f.state_of(lost_id), f.ln.calls(&lost)), (PayState::PayFailed, 1));

    // The wallet does not hold the amount yet: refused before funding, so not a failure. The pay
    // waits `charged` and the sweeper pays it once the wallet can.
    f.ln.set(Mode::Insufficient);
    let wait = hour_invoice(10_000);
    let (s, b) = f.post(req(&wait, "wait")).await;
    assert_eq!((s, b["state"].as_str()), (202, Some("charged")), "{b}");
    let wait_id = b["pay_id"].as_u64().unwrap();
    f.ln.set(Mode::Pay);
    pay::sweep(&f.handle).await;
    assert_eq!((f.state_of(wait_id), f.sales_for("wait")), (PayState::Paid, 1));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_after_each_step_neither_loses_nor_doubles_money() -> anyhow::Result<()> {
    let mut f = fx_with(cfg(), None).await?;
    let points = [PayCrash::AfterRecord, PayCrash::AfterCharge, PayCrash::AfterCharged, PayCrash::AfterPaying, PayCrash::AfterPay];
    let mut cases = vec![];
    for (i, point) in points.into_iter().enumerate() {
        let (bolt11, cref) = (hour_invoice(10_000), format!("crash{i}"));
        *f.handle.pays().unwrap().crash.lock().unwrap() = Some(point);
        let (s, b) = f.post(req(&bolt11, &cref)).await;
        assert_eq!((s, b["detail"].as_str()), (500, Some(format!("crash point {point:?}").as_str())), "{b}");
        let disk = PayStore::open(PayStore::path_in(f.dir.path()))?;
        let rec = disk.by_ref(&cref).unwrap().clone();
        let want = match point {
            PayCrash::AfterRecord | PayCrash::AfterCharge => PayState::Charging,
            PayCrash::AfterCharged => PayState::Charged,
            PayCrash::AfterPaying | PayCrash::AfterPay => PayState::Paying,
        };
        assert_eq!(rec.state, want, "{point:?}");
        let charged = !matches!(point, PayCrash::AfterRecord);
        assert_eq!(f.sales_for(&cref), usize::from(charged), "{point:?}");
        assert_eq!(f.ln.calls(&bolt11), usize::from(point == PayCrash::AfterPay), "{point:?}");
        cases.push((point, bolt11, cref, rec.pay_id));
    }

    // Restart, and the sweeper's first pass.
    f.restart(cfg());
    pay::sweep(&f.handle).await;
    for (point, bolt11, cref, id) in &cases {
        let rec = f.handle.pays().unwrap().get(*id).unwrap();
        if *point == PayCrash::AfterRecord {
            // The card was never charged: not_charged, nothing owed, nothing paid.
            assert_eq!((rec.state, f.sales_for(cref), f.ln.calls(bolt11)), (PayState::NotCharged, 0, 0));
            continue;
        }
        // Charged once, paid once: an attempt the crash cut off is found, not made again.
        assert_eq!((rec.state, f.sales_for(cref), f.ln.calls(bolt11)), (PayState::Paid, 1, 1), "{point:?}");
        assert!(rec.sale_id.is_some() && rec.preimage.is_some(), "{point:?}");
    }
    // A second pass changes nothing.
    pay::sweep(&f.handle).await;
    assert_eq!(f.ln.total_calls(), 4);

    // The pay whose card was never charged: the holder taps again and it pays, charged once.
    let (_, bolt11, cref, id) = &cases[0];
    let (s, b) = f.post(req(bolt11, cref)).await;
    assert_eq!((s, b["state"].as_str(), b["pay_id"].as_u64()), (200, Some("paid"), Some(*id)), "{b}");
    assert_eq!((f.sales_for(cref), f.ln.calls(bolt11)), (1, 1));
    // And the others answer their success from the record, with no card or payment.
    let signed = f.signs.get();
    for (_, bolt11, cref, _) in &cases[1..] {
        let (s, b) = f.post(req(bolt11, cref)).await;
        assert_eq!((s, b["state"].as_str()), (200, Some("paid")), "{b}");
    }
    assert_eq!((f.signs.get(), f.ln.total_calls()), (signed, 5));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_charge_whose_answer_was_lost_pays_once_and_an_unresolved_one_never_pays() -> anyhow::Result<()> {
    // The federation took the charge, then the charge errored: its sale is recorded, so the pay
    // is charged, and it pays once.
    let mut f = fx_with(cfg(), Some(CrashPoint::AfterSubmit)).await?;
    let bolt11 = hour_invoice(10_000);
    let (s, b) = f.post(req(&bolt11, "lost")).await;
    assert_eq!((s, b["state"].as_str()), (200, Some("paid")), "{b}");
    assert_eq!((f.sales_for("lost"), f.ln.calls(&bolt11)), (1, 1));

    // The charge is undecided (signed, never sent): the pay waits `charging` and never pays, and
    // the sweeper leaves it as long as the charge is unresolved.
    let mut f = fx_with(cfg(), Some(CrashPoint::BeforeSubmit)).await?;
    let bolt11 = hour_invoice(10_000);
    let (s, b) = f.post(req(&bolt11, "unresolved")).await;
    assert_eq!((s, b["error"].as_str(), b["state"].as_str()), (202, Some("undecided"), Some("charging")), "{b}");
    let id = b["pay_id"].as_u64().unwrap();
    pay::sweep(&f.handle).await;
    f.restart(cfg());
    pay::sweep(&f.handle).await;
    assert_eq!((f.state_of(id), f.ln.calls(&bolt11), f.ln.total_calls(), f.sales_for("unresolved")), (PayState::Charging, 0, 0, 0));

    // Recorded not_charged, as a refusal read before the charge showed up would leave it: a
    // repeat finds the charge unresolved, and neither charges again nor refuses the pay for it.
    f.edit_on_disk(id, |r| r.state = PayState::NotCharged);
    f.restart(cfg());
    let signed = f.signs.get();
    let (s, b) = f.post(req(&bolt11, "unresolved")).await;
    assert_eq!((s, b["error"].as_str(), b["state"].as_str()), (202, Some("undecided"), Some("charging")), "{b}");
    assert_eq!((f.state_of(id), f.signs.get(), f.ln.total_calls(), f.sales_for("unresolved")), (PayState::Charging, signed, 0, 0));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repeat_replays_whatever_the_invoice_the_settings_or_the_till_say_now() -> anyhow::Result<()> {
    let mut f = fx_with(cfg(), None).await?;
    let bolt11 = hour_invoice(10_000);
    let (s, paid) = f.post(req(&bolt11, "a")).await;
    assert_eq!((s, paid["state"].as_str()), (200, Some("paid")), "{paid}");
    f.ln.set(Mode::InFlight);
    let slow = hour_invoice(10_000);
    let (s, b) = f.post(req(&slow, "slow")).await;
    assert_eq!((s, b["state"].as_str()), (202, Some("paying")), "{b}");
    let (signed, opens, calls) = (f.signs.get(), f.opens.get(), f.ln.total_calls());

    // The operator restarts with settings no pay of these invoices would pass now: an hour is too
    // little time left, the margin and the fee cap differ.
    f.restart(PayConfig { min_expiry_secs: 7200, margin_msat: 999, max_fee_msat: 500, ..cfg() });
    let (s, again) = f.post(req(&bolt11, "a")).await;
    assert_eq!((s, &again), (200, &paid));
    let (s, b) = f.post(req(&slow, "slow")).await;
    assert_eq!((s, b["error"].as_str(), b["state"].as_str()), (202, Some("undecided"), Some("paying")), "{b}");
    // Another invoice, or another fee asked, under the ref is still a conflict.
    let (s, c) = f.post(req(&hour_invoice(10_000), "a")).await;
    assert_eq!((s, c["error"].as_str()), (409, Some("conflict")), "{c}");
    let (s, c) = f.post(json!({"bolt11": bolt11, "max_fee_msat": 999, "client_ref": "a"})).await;
    assert_eq!((s, c["error"].as_str()), (409, Some("conflict")), "{c}");
    // A new pay meets the new settings.
    let (s, c) = f.post(req(&hour_invoice(10_000), "new")).await;
    assert_eq!((s, c["error"].as_str()), (400, Some("invoice_expired")), "{c}");
    assert_eq!((f.signs.get(), f.opens.get(), f.ln.total_calls()), (signed, opens, calls), "no card, no payment");
    assert_eq!((f.sales_for("a"), f.sales_for("slow")), (1, 1));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_charge_that_exists_after_all_is_this_pays_and_pays_once() -> anyhow::Result<()> {
    let mut f = fx_with(cfg(), None).await?;
    // Two pays whose card paid, then a crash: one left `charging`, one recorded `not_charged` as a
    // refusal read before the charge showed up would leave it.
    let mut cases = vec![];
    for cref in ["cut", "refused"] {
        let bolt11 = hour_invoice(10_000);
        *f.handle.pays().unwrap().crash.lock().unwrap() = Some(PayCrash::AfterCharge);
        let (s, b) = f.post(req(&bolt11, cref)).await;
        assert_eq!(s, 500, "{b}");
        let id = PayStore::open(PayStore::path_in(f.dir.path()))?.by_ref(cref).unwrap().pay_id;
        assert_eq!(f.sales_for(cref), 1);
        cases.push((cref, bolt11, id));
    }
    f.edit_on_disk(cases[1].2, |r| r.state = PayState::NotCharged);
    // The invoices now have less time left than a new pay needs: these were charged already.
    f.restart(PayConfig { min_expiry_secs: 7200, ..cfg() });
    let signed = f.signs.get();
    for (cref, bolt11, id) in &cases {
        let (s, b) = f.post(req(bolt11, cref)).await;
        assert_eq!((s, b["state"].as_str(), b["pay_id"].as_u64()), (200, Some("paid"), Some(*id)), "{cref}: {b}");
        assert_eq!((f.sales_for(cref), f.ln.calls(bolt11)), (1, 1), "{cref}");
    }
    assert_eq!(f.signs.get(), signed, "the card was not charged again");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pay_cut_off_after_its_charge_still_knows_the_card_that_paid() -> anyhow::Result<()> {
    let mut f = fx_with(cfg(), None).await?;
    let card_id = fedimint_card_sidecar::ops::short_id_of(&f.card.borrow_mut().get_pub(0)?);
    let bolt11 = hour_invoice(10_000);
    // The card is charged, then a crash before the pay recorded the charge's answer.
    *f.handle.pays().unwrap().crash.lock().unwrap() = Some(PayCrash::AfterCharge);
    let (s, b) = f.post(req(&bolt11, "cut")).await;
    assert_eq!(s, 500, "{b}");
    let rec = PayStore::open(PayStore::path_in(f.dir.path()))?.by_ref("cut").unwrap().clone();
    assert_eq!((rec.state, rec.short_id), (PayState::Charging, None));
    // The charge's sale recorded the card in the same append.
    let sales = SalesStore::open(f.dir.path().join("sales.json"))?;
    assert_eq!(sales.find_ref(&pay::charge_ref("cut")).unwrap().short_id.as_deref(), Some(card_id.as_str()));

    // Restart; the sweeper settles the pay from its sale, and the payment fails.
    f.restart(cfg());
    f.ln.set(Mode::Refund);
    pay::sweep(&f.handle).await;
    let got = f.handle.pays().unwrap().get(rec.pay_id).unwrap();
    assert_eq!((got.state, got.short_id.as_deref()), (PayState::PayFailed, Some(card_id.as_str())));
    assert_eq!(PayStore::open(PayStore::path_in(f.dir.path()))?.get(rec.pay_id).unwrap().short_id.as_deref(), Some(card_id.as_str()));
    // So its refund knows its card: not card_unknown (this till is off the registry, which a
    // refund needs next).
    let (h, id) = (f.handle.clone(), rec.pay_id);
    let (s, r) = with_actor(&mut f.actor, async move { call(&h, "POST", &format!("/v1/pay/{id}/refund"), json!({})).await }).await;
    assert_eq!((s, r["error"].as_str(), r["short_id"].as_str()), (503, Some("ops_not_connected"), Some(card_id.as_str())), "{r}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_card_that_cannot_say_which_card_it_is_is_not_charged_for_a_pay() -> anyhow::Result<()> {
    let mut f = fx_with(cfg(), None).await?;
    let card_id = fedimint_card_sidecar::ops::short_id_of(&f.card.borrow_mut().get_pub(0)?);
    let bolt11 = hour_invoice(10_000);
    // Slot 0 answers no key: the till cannot tell which card would pay, so it charges nothing.
    let real = std::mem::replace(&mut *f.card.borrow_mut(), SoftCard::new());
    let signed = f.signs.get();
    let (s, b) = f.post(req(&bolt11, "who")).await;
    assert_ne!(s, 200, "{b}");
    assert_eq!(b["state"], "not_charged", "{b}");
    assert_eq!((f.signs.get(), f.sales_for("who"), f.ln.calls(&bolt11)), (signed, 0, 0));
    // The holder taps their card again: charged once, paid, its card recorded.
    *f.card.borrow_mut() = real;
    let (s, b) = f.post(req(&bolt11, "who")).await;
    assert_eq!((s, b["state"].as_str(), b["short_id"].as_str()), (200, Some("paid"), Some(card_id.as_str())), "{b}");
    assert_eq!((f.sales_for("who"), f.ln.calls(&bolt11)), (1, 1));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retried_pay_counts_to_the_daily_cap_from_its_charge() -> anyhow::Result<()> {
    let capped = PayConfig { daily_cap_msat: 2 * PRICE, ..cfg() };
    let mut f = fx_with(capped, None).await?;
    let old = hour_invoice(10_000);
    let (s, b) = f.post(json!({"bolt11": old, "max_fee_msat": 1_000, "client_ref": "old", "pin": "9999"})).await;
    assert_eq!((s, b["state"].as_str()), (403, Some("not_charged")), "{b}");
    let id = b["pay_id"].as_u64().unwrap();
    // Recorded a day and a bit ago.
    f.edit_on_disk(id, |r| r.created_at_ms -= 86_400_001);
    f.restart(capped);
    let (s, b) = f.post(req(&hour_invoice(10_000), "today")).await;
    assert_eq!((s, b["state"].as_str()), (200, Some("paid")), "{b}");
    let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64;
    let (s, b) = f.post(json!({"bolt11": old, "max_fee_msat": 1_000, "client_ref": "old", "pin": "1234"})).await;
    assert_eq!((s, b["state"].as_str(), b["pay_id"].as_u64()), (200, Some("paid"), Some(id)), "{b}");
    assert!(b["created_at_ms"].as_u64().unwrap() >= before, "{b}");
    // Both charged today: the cap is reached.
    let (s, b) = f.post(req(&hour_invoice(10_000), "over")).await;
    assert_eq!((s, b["error"].as_str()), (403, Some("daily_cap")), "{b}");
    assert_eq!(f.sales_for("over"), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invoice_that_expires_while_the_till_waits_for_funds_is_pay_failed() -> anyhow::Result<()> {
    let mut f = fx_with(PayConfig { min_expiry_secs: 1, ..cfg() }, None).await?;

    // The wallet cannot pay yet: `charged`. The invoice expires before it can; the sweeper finds
    // nothing funded and calls it failed, the value owed to the holder.
    f.ln.set(Mode::Insufficient);
    let waiting = invoice(10_000, Duration::from_secs(6));
    let (s, b) = f.post(req(&waiting, "waiting")).await;
    assert_eq!((s, b["state"].as_str()), (202, Some("charged")), "{b}");
    let id = b["pay_id"].as_u64().unwrap();
    tokio::time::sleep(Duration::from_secs(7)).await;
    pay::sweep(&f.handle).await;
    let rec = f.handle.pays().unwrap().get(id).unwrap();
    assert_eq!((rec.state, f.ln.calls(&waiting), f.sales_for("waiting")), (PayState::PayFailed, 1, 1));
    assert!(rec.detail.as_deref().unwrap().contains("expired"), "{rec:?}");
    assert_eq!(PayStore::open(PayStore::path_in(f.dir.path()))?.get(id).unwrap().state, PayState::PayFailed);

    // The wallet answers that it cannot pay only once the invoice has expired: failed at once.
    f.ln.delay(Duration::from_secs(7));
    let late = invoice(10_000, Duration::from_secs(6));
    let (s, b) = f.post(req(&late, "late")).await;
    assert_eq!((s, b["error"].as_str(), b["state"].as_str()), (502, Some("pay_failed"), Some("pay_failed")), "{b}");
    assert!(b["detail"].as_str().unwrap().contains("expired"), "{b}");
    assert_eq!((f.ln.calls(&late), f.sales_for("late")), (1, 1));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repeat_while_the_first_is_driven_neither_charges_nor_pays() -> anyhow::Result<()> {
    let mut f = fx_with(cfg(), None).await?;
    f.ln.set(Mode::InFlight);
    f.ln.delay(Duration::from_secs(3));
    let bolt11 = hour_invoice(10_000);
    let body = req(&bolt11, "twice");
    let h = f.handle.clone();
    let (first, second) = with_actor(&mut f.actor, async move {
        let first = tokio::spawn({
            let (h, body) = (h.clone(), body.clone());
            async move { call(&h, "POST", "/v1/pay", body).await }
        });
        // The first is paying (its card charged, the wallet not answered yet).
        let pays = h.pays().unwrap();
        for _ in 0..200 {
            if pays.store.lock().unwrap().by_ref("twice").is_some_and(|r| r.state == PayState::Paying) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let second = call(&h, "POST", "/v1/pay", body).await;
        (first.await.unwrap(), second)
    })
    .await;
    let (s, b) = second;
    assert_eq!((s, b["error"].as_str(), b["state"].as_str()), (202, Some("undecided"), Some("paying")), "{b}");
    assert!(b["detail"].as_str().unwrap().contains("in flight"), "{b}");
    let (s, b) = first;
    assert_eq!((s, b["state"].as_str()), (202, Some("paying")), "{b}");
    assert_eq!((f.ln.total_calls(), f.sales_for("twice")), (1, 1));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_card_pays_an_invoice_through_the_till_wallet() -> anyhow::Result<()> {
    use fedimint_ln_client::{LightningClientInit, LightningClientModule, LnReceiveState};
    use futures::StreamExt as _;

    let fixtures = Fixtures::new_primary(MintClientInit, MintInit)
        .with_module(DummyClientInit, DummyInit)
        .with_module(LightningClientInit { gateway_conn: None }, fedimint_ln_server::LightningInit);
    let (fed, client) = fixture(fixtures).await?;
    let wallet = Wallet::from_client(client).await?;
    let dir = tempfile::tempdir()?;
    let pays = Pays::for_wallet(dir.path(), &wallet, cfg()).await?.expect("the till pays invoices with ln");
    assert_eq!(pays.currency, Currency::Regtest);

    // Someone else in the federation asks to be paid 100 sat.
    let receiver = fed.new_client().await;
    let ln = receiver.get_first_module::<LightningClientModule>()?;
    let description = lightning_invoice::Description::new("card pay test".into())?;
    let (receive_op, bolt11, _) = ln
        .create_bolt11_invoice(Amount::from_sats(100), lightning_invoice::Bolt11InvoiceDescription::Direct(description), None, (), None)
        .await?;
    let mut receive = ln.subscribe_ln_receive(receive_op).await?.into_stream();
    while let Some(state) = receive.next().await {
        if matches!(state, LnReceiveState::WaitingForPayment { .. }) {
            break;
        }
    }
    let bolt11 = bolt11.to_string();

    let (_dir, handle, mut actor, _opens, _signs, _card) = till_on(wallet, cfg(), pays.ln.clone(), None).await?;
    let h = handle.clone();
    let b = bolt11.clone();
    let (s, body) = with_actor(&mut actor, async move { call(&h, "POST", "/v1/pay", req(&b, "real")).await }).await;
    assert_eq!((s, body["state"].as_str()), (200, Some("paid")), "{body}");
    // Inside the federation: no gateway fee, the whole reserve stays with the till.
    assert_eq!((body["fee_paid_msat"].as_u64(), body["fee_refund_msat"].as_u64()), (Some(0), Some(1_000)), "{body}");
    assert_eq!(body["price_msat"].as_u64(), Some(100_000 + 1_000 + 50 + 1_000));
    let preimage = body["preimage"].as_str().unwrap();
    let hash = fedimint_card_sidecar::wallet::parse_bolt11(&bolt11)?;
    assert_eq!(sha256::Hash::hash(&hex::decode(preimage)?).to_string(), hash.payment_hash().to_string());
    // The receiver got it.
    let claimed = tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(state) = receive.next().await {
            if matches!(state, LnReceiveState::Claimed) {
                return true;
            }
        }
        false
    })
    .await?;
    assert!(claimed);
    // A repeat answers the record; the wallet pays nothing again.
    let h = handle.clone();
    let b = bolt11.clone();
    let (s, again) = with_actor(&mut actor, async move { call(&h, "POST", "/v1/pay", req(&b, "real")).await }).await;
    assert_eq!((s, &again), (200, &body));
    Ok(())
}
