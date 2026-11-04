//! O3: the till on ops (docs/OPS-PLAN.md § 7), against a running registry Worker.
//!
//! Skipped unless `OPS_TEST_URL` names a registry with a bootstrapped operator, e.g. a local
//! `wrangler dev` (from `registry/`):
//!
//! ```text
//! npx wrangler dev --port 8799 --persist-to <scratch dir> \
//!   --var 'OPERATOR_SETUP:{"action":"bootstrap","nonce":"local-o3-nonce-0001","name":"op","password":"local o3 password"}'
//! OPS_TEST_URL=http://127.0.0.1:8799 cargo test -p fedimint-card-sidecar --test ops_till
//! ```
//!
//! `OPS_TEST_OPERATOR` and `OPS_TEST_PASSWORD` default to that bootstrap. Each test makes its own
//! batch, till tokens and cards (random short ids), so a persisted registry can be reused. Never
//! point this at the staging or production registry: it writes rows there.
//!
//! The federation is the in-process fixture of `till_service.rs`; the card is a [`SoftCard`],
//! registered with its eight keys as the provisioner registers one, and loaded through the till's
//! own `topup` (a load, recorded in ops as an intent with no inputs).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fedimint_card_host_card::{Card, SoftCard, open_pin_session};
use fedimint_card_sidecar::glue::{CardWallet, CrashPoint};
use fedimint_card_sidecar::ops::{Ops, OpsConfig, short_id_of};
use fedimint_card_sidecar::sales::SalesStore;
use fedimint_card_sidecar::state::StateStore;
use fedimint_card_sidecar::till::{card_balance_job, charge_job};
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
use serde_json::{Value, json};

// ------------------------------------------------------------------ the registry

struct Registry {
    url: String,
    http: reqwest::Client,
    operator: String,
    batch_id: u64,
}

impl Registry {
    /// Sign in and make a batch, or `None` (and a note on stderr) when `OPS_TEST_URL` is unset.
    async fn connect(federation_id: &str) -> anyhow::Result<Option<Self>> {
        let Ok(url) = std::env::var("OPS_TEST_URL") else {
            eprintln!("skipped: set OPS_TEST_URL to a local `wrangler dev` of registry/ (see this file's docs)");
            return Ok(None);
        };
        let name = std::env::var("OPS_TEST_OPERATOR").unwrap_or_else(|_| "op".into());
        let password = std::env::var("OPS_TEST_PASSWORD").unwrap_or_else(|_| "local o3 password".into());
        let http = reqwest::Client::new();
        let url = url.trim_end_matches('/').to_owned();
        let session: Value = http
            .post(format!("{url}/auth/operator/session"))
            .json(&json!({"name": name, "password": password}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let operator = session["token"].as_str().expect("an operator token").to_owned();
        let mut r = Self { url, http, operator, batch_id: 0 };
        let b = r
            .op_post("/registry/batches", json!({"federation_id": federation_id, "count": 10, "pin_policy": "batch", "load_sha256": "a".repeat(64)}))
            .await?;
        r.batch_id = b["batch"]["batch_id"].as_u64().expect("a batch id");
        Ok(Some(r))
    }

    async fn op_post(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        let resp = self.http.post(format!("{}{path}", self.url)).bearer_auth(&self.operator).json(&body).send().await?;
        let status = resp.status();
        let v: Value = resp.json().await?;
        anyhow::ensure!(status.is_success(), "{path}: {status} {v}");
        Ok(v)
    }

    /// A till token and the till's `Ops`, with its own data directory.
    async fn till(&self, name: &str, dir: &std::path::Path) -> anyhow::Result<Ops> {
        let t = self.op_post("/tills", json!({"name": name})).await?;
        let cfg = OpsConfig { url: self.url.clone(), token: t["token"].as_str().expect("a till token").to_owned() };
        Ops::new(&cfg, dir)
    }

    /// Key a blank card, set its PIN, and register its row with the eight keys.
    async fn provision(&self, card: &Rc<RefCell<SoftCard>>) -> anyhow::Result<String> {
        let mut keys = vec![];
        for slot in 0..8 {
            keys.push(card.borrow_mut().gen_key(slot)?);
        }
        open_pin_session(&mut *card.borrow_mut())?;
        let short_id = short_id_of(&keys[0]);
        let row = json!({
            "short_id": short_id,
            "batch_id": self.batch_id,
            "installed_at": 1_762_000_000_000u64,
            "load_sha256": "a".repeat(64),
            "pin_set": true,
            "pin_policy": "batch",
            "notes_issued": 0,
            "slot_pubkeys": keys.iter().map(|k| k.to_string()).collect::<Vec<_>>(),
        });
        self.op_post("/registry/cards", row).await?;
        Ok(short_id)
    }

    async fn state(&self, short_id: &str) -> anyhow::Result<Value> {
        let resp = self.http.get(format!("{}/cards/{short_id}/state", self.url)).bearer_auth(&self.operator).send().await?;
        Ok(resp.error_for_status()?.json().await?)
    }
}

fn live_msat(state: &Value) -> u64 {
    state["notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["status"] == "live")
        .map(|n| n["amount_msat"].as_u64().unwrap())
        .sum()
}

fn open_txs(state: &Value) -> Vec<Value> {
    state["open_txs"].as_array().unwrap().clone()
}

// ------------------------------------------------------------------ the federation and tills

/// The physical card, handed to whichever till it is tapped on.
struct Shared(Rc<RefCell<SoftCard>>);

impl Card for Shared {
    fn gen_key(&mut self, slot: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.borrow_mut().gen_key(slot)
    }
    fn gen_key_overwrite(&mut self, slot: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.borrow_mut().gen_key_overwrite(slot)
    }
    fn get_pub(&mut self, slot: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.borrow_mut().get_pub(slot)
    }
    fn sign(&mut self, slot: u8, msg32: &[u8; 32]) -> anyhow::Result<Signature> {
        self.0.borrow_mut().sign(slot, msg32)
    }
    fn apdu(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.0.borrow_mut().apdu(apdu)
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

async fn federation() -> anyhow::Result<FederationTest> {
    assert!(
        std::env::var_os("FM_TEST_USE_REAL_DAEMONS").is_none(),
        "FM_TEST_USE_REAL_DAEMONS is set: these tests need the in-process fake bitcoin backend"
    );
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit);
    Ok(tokio::time::timeout(Duration::from_secs(60), fixtures.new_fed_builder(0).build()).await?)
}

/// One till: its own federation client, data directory, sales ledger and ops token.
struct Till {
    cw: CardWallet,
    ops: Ops,
    sales: Arc<Mutex<SalesStore>>,
    dir: tempfile::TempDir,
}

impl Till {
    async fn new(fed: &FederationTest, reg: &Registry, name: &str, funds: Amount) -> anyhow::Result<Self> {
        let client = fed.new_client().await;
        if funds.msats > 0 {
            fund(&client, funds).await?;
        }
        let wallet = Wallet::from_client(client).await?;
        let dir = tempfile::tempdir()?;
        let store = StateStore::open(dir.path().join("card_state.json"))?;
        let cw = CardWallet::new(wallet, store, Box::new(fedimint_card_sidecar::till::NoCard));
        let ops = reg.till(name, dir.path()).await?;
        let sales = Arc::new(Mutex::new(SalesStore::open(dir.path().join("sales.json"))?));
        Ok(Self { cw, ops, sales, dir })
    }

    fn tap(&mut self, card: &Rc<RefCell<SoftCard>>) {
        self.cw.card = Box::new(Shared(card.clone()));
    }

    /// Load `amounts` onto the card from this till's wallet, recorded in ops.
    async fn load(&mut self, card: &Rc<RefCell<SoftCard>>, amounts: &[u64]) -> anyhow::Result<()> {
        self.tap(card);
        let base = self.ops.attach(&mut self.cw).await?;
        let r = self.cw.topup(amounts).await;
        Ops::finish(&mut self.cw, base).await;
        r.map(|_| ())
    }

    async fn charge(&mut self, card: &Rc<RefCell<SoftCard>>, price: u64, client_ref: &str, pin: &str) -> (u16, Value) {
        self.tap(card);
        self.cw.pin = Some(pin.as_bytes().to_vec());
        let r = charge_job(&mut self.cw, Some(&self.ops), price, Some(client_ref.into()), &self.sales).await;
        self.cw.pin = None;
        (r.status, r.body)
    }

    fn sale_refs(&self) -> Vec<String> {
        self.sales.lock().unwrap().file.sales.iter().filter_map(|s| s.client_ref.clone()).collect()
    }
}

fn fed_id(fed: &FederationTest) -> String {
    fed.id().to_string()
}

// ------------------------------------------------------------------ (a) a charge end to end

#[tokio::test(flavor = "multi_thread")]
async fn a_charge_runs_through_ops_end_to_end() -> anyhow::Result<()> {
    let fed = federation().await?;
    let Some(reg) = Registry::connect(&fed_id(&fed)).await? else { return Ok(()) };
    let mut till = Till::new(&fed, &reg, "o3 front desk", Amount::from_sats(10_000)).await?;
    let card = Rc::new(RefCell::new(SoftCard::new()));
    let short_id = reg.provision(&card).await?;

    till.load(&card, &[1 << 16, 1 << 14]).await?;
    let s = reg.state(&short_id).await?;
    assert_eq!(live_msat(&s), (1 << 16) + (1 << 14), "the load is live in ops: {s}");
    assert!(open_txs(&s).is_empty(), "{s}");
    assert!(till.dir.path().join("cards").join(format!("{short_id}.json")).exists());

    // A wrong PIN is its own error, and the intent it left is recorded rejected.
    let (status, body) = till.charge(&card, 50_000, "o3-a-wrong", "9999").await;
    assert_eq!((status, body["error"].as_str()), (403, Some("wrong_pin")), "{body}");
    assert!(!body.to_string().contains("9999"), "the PIN is never echoed");
    let s = reg.state(&short_id).await?;
    assert!(open_txs(&s).is_empty(), "the refused charge is not left open: {s}");
    assert_eq!(live_msat(&s), (1 << 16) + (1 << 14));

    let (status, body) = till.charge(&card, 50_000, "o3-a", "1234").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["collection_pending"], false, "{body}");
    let sum = |k: &str| -> u64 { body[k].as_array().unwrap().iter().map(|c| c["msat"].as_u64().unwrap()).sum() };
    let left = (1 << 16) + (1 << 14) - sum("inputs") + sum("change");
    let s = reg.state(&short_id).await?;
    assert!(open_txs(&s).is_empty(), "the outcome reached ops: {s}");
    assert_eq!(live_msat(&s), left, "ops holds the untouched notes plus the change: {s}");
    assert_eq!(till.sale_refs(), vec!["o3-a".to_string()]);

    // What `serve` does at startup: the federation's name, for the provisioner to show.
    fedimint_card_sidecar::ops::report_federation(&till.ops.client, &fed_id(&fed), Some("O3 test federation")).await;
    let feds = reg.http.get(format!("{}/registry/federations", reg.url)).bearer_auth(&reg.operator).send().await?.json::<Value>().await?;
    assert!(
        feds["federations"].as_array().unwrap().iter().any(|f| f["federation_id"] == fed_id(&fed) && f["name"] == "O3 test federation"),
        "{feds}"
    );

    // The balance job reads the same state back from ops.
    till.tap(&card);
    let b = card_balance_job(&mut till.cw, Some(&till.ops), &till.sales).await;
    assert_eq!(b.status, 200, "{:?}", b.body);
    assert_eq!(b.body["live_msat"].as_u64(), Some(left));
    Ok(())
}

// ------------------------------------------------------------------ (b) recovery by another till

#[tokio::test(flavor = "multi_thread")]
async fn a_charge_killed_between_sign_and_outcome_is_finished_by_a_second_till() -> anyhow::Result<()> {
    let fed = federation().await?;
    let Some(reg) = Registry::connect(&fed_id(&fed)).await? else { return Ok(()) };
    let mut first = Till::new(&fed, &reg, "o3 first", Amount::from_sats(10_000)).await?;
    let mut second = Till::new(&fed, &reg, "o3 second", Amount::ZERO).await?;
    let card = Rc::new(RefCell::new(SoftCard::new()));
    let short_id = reg.provision(&card).await?;
    first.load(&card, &[1 << 16, 1 << 14]).await?;

    // The first till dies after the card signed and ops has the signed bytes, before it sends.
    first.cw.crash_point = Some(CrashPoint::BeforeSubmit);
    let (status, body) = first.charge(&card, 20_000, "o3-b-first", "1234").await;
    first.cw.crash_point = None;
    assert_eq!(status, 500, "{body}");
    let s = reg.state(&short_id).await?;
    let open = open_txs(&s);
    assert_eq!(open.len(), 1, "{s}");
    assert!(open[0]["signed_hex"].is_string(), "ops has the signed bytes: {s}");
    let crashed = open[0]["txid"].as_str().unwrap().to_owned();

    // The holder walks to another till. It resubmits the signed bytes (never re-signs), records
    // the federation's answer, and then makes its own sale.
    let (status, body) = second.charge(&card, 10_000, "o3-b-second", "1234").await;
    assert_eq!(status, 200, "{body}");
    let s = reg.state(&short_id).await?;
    assert!(open_txs(&s).is_empty(), "both charges are resolved in ops: {s}");
    assert_eq!(second.sale_refs(), vec!["o3-b-second".to_string()], "another till's charge is not its sale");
    assert_ne!(body["txid"].as_str(), Some(crashed.as_str()));

    // The first till, back up, collects its own output from the charge it made and records it.
    first.tap(&card);
    let b = card_balance_job(&mut first.cw, Some(&first.ops), &first.sales).await;
    assert_eq!(b.status, 200, "{:?}", b.body);
    let sales = first.sales.lock().unwrap().file.sales.clone();
    let sale = sales.iter().find(|s| s.client_ref.as_deref() == Some("o3-b-first")).expect("the first till's sale");
    assert_eq!(sale.txid, crashed);
    assert!(sale.received_msat > 0 && !sale.collection_pending, "{sale:?}");
    let local = StateStore::open(first.ops.card_path(&short_id))?.state;
    assert!(local.pending_txs.is_empty() && local.host_pending.is_empty() && local.ops_outbox.is_empty(), "{local:?}");
    Ok(())
}

// ------------------------------------------------------------------ (c) two cards on one till

#[tokio::test(flavor = "multi_thread")]
async fn two_cards_are_charged_on_one_till() -> anyhow::Result<()> {
    let fed = federation().await?;
    let Some(reg) = Registry::connect(&fed_id(&fed)).await? else { return Ok(()) };
    let mut till = Till::new(&fed, &reg, "o3 two cards", Amount::from_sats(20_000)).await?;
    let a = Rc::new(RefCell::new(SoftCard::new()));
    let b = Rc::new(RefCell::new(SoftCard::new()));
    let id_a = reg.provision(&a).await?;
    let id_b = reg.provision(&b).await?;
    assert_ne!(id_a, id_b);
    till.load(&a, &[1 << 16]).await?;
    till.load(&b, &[1 << 15, 1 << 14]).await?;

    for (card, price, cref) in [(&a, 20_000, "o3-c-a1"), (&b, 15_000, "o3-c-b1"), (&a, 10_000, "o3-c-a2")] {
        let (status, body) = till.charge(card, price, cref, "1234").await;
        assert_eq!(status, 200, "{cref}: {body}");
    }
    let (sa, sb) = (reg.state(&id_a).await?, reg.state(&id_b).await?);
    assert!(open_txs(&sa).is_empty() && open_txs(&sb).is_empty());
    assert!(live_msat(&sa) < 1 << 16 && live_msat(&sb) < (1 << 15) + (1 << 14));
    assert_eq!(till.sale_refs(), vec!["o3-c-a1", "o3-c-b1", "o3-c-a2"]);
    for id in [&id_a, &id_b] {
        assert!(till.ops.card_path(id).exists(), "one local file per card");
    }
    // The till's own card_state.json is not where card state went.
    let own = StateStore::open(till.dir.path().join("card_state.json"))?.state;
    assert!(own.notes.is_empty() && own.slots.is_empty());
    Ok(())
}

// ------------------------------------------------------------------ no answer to `submitted`

#[tokio::test(flavor = "multi_thread")]
async fn signed_bytes_ops_never_answered_for_are_posted_again_never_abandoned() -> anyhow::Result<()> {
    let fed = federation().await?;
    let Some(reg) = Registry::connect(&fed_id(&fed)).await? else { return Ok(()) };
    let mut till = Till::new(&fed, &reg, "o3 unanswered", Amount::from_sats(20_000)).await?;
    let mut other = Till::new(&fed, &reg, "o3 other", Amount::ZERO).await?;
    let card = Rc::new(RefCell::new(SoftCard::new()));
    let short_id = reg.provision(&card).await?;
    till.load(&card, &[1 << 16, 1 << 15, 1 << 14]).await?;
    let local = |t: &Till| StateStore::open(t.ops.card_path(&short_id)).unwrap().state;

    // 1. No answer, and ops never got the bytes: undecided, kept, nothing queued. The repeat
    //    posts them again, ops takes them, and the charge is sent.
    till.cw.crash_point = Some(CrashPoint::SubmittedUnreachable);
    let (status, body) = till.charge(&card, 10_000, "o3-u1", "1234").await;
    till.cw.crash_point = None;
    assert_eq!((status, body["error"].as_str()), (202, Some("undecided")), "{body}");
    let s = reg.state(&short_id).await?;
    assert!(open_txs(&s)[0]["signed_hex"].is_null(), "{s}");
    let st = local(&till);
    assert!(st.pending_txs[0].ops_unconfirmed && !st.pending_txs[0].submitted && st.pending_txs[0].tx_hex.is_some());
    assert!(st.ops_outbox.is_empty(), "no rejection queued");
    let (status, body) = till.charge(&card, 10_000, "o3-u1", "1234").await;
    assert_eq!(status, 200, "{body}");
    assert!(open_txs(&reg.state(&short_id).await?).is_empty());

    // 2. Ops took the bytes but the answer was lost: the next job sees them in ops and sends.
    till.cw.crash_point = Some(CrashPoint::SubmittedAnswerLost);
    let (status, body) = till.charge(&card, 10_000, "o3-u2", "1234").await;
    till.cw.crash_point = None;
    assert_eq!(status, 202, "{body}");
    assert!(open_txs(&reg.state(&short_id).await?)[0]["signed_hex"].is_string());
    let (status, body) = till.charge(&card, 10_000, "o3-u2", "1234").await;
    assert_eq!(status, 200, "{body}");

    // 3. No answer, and meanwhile another till records the charge rejected (ops had no signed
    //    bytes): the re-post gets 409 and the charge is abandoned, never sent.
    till.cw.crash_point = Some(CrashPoint::SubmittedUnreachable);
    let (status, _) = till.charge(&card, 10_000, "o3-u3", "1234").await;
    till.cw.crash_point = None;
    assert_eq!(status, 202);
    let lost = local(&till).pending_txs[0].txid.clone();
    let (status, body) = other.charge(&card, 10_000, "o3-other", "1234").await;
    assert_eq!(status, 200, "{body}");
    till.tap(&card);
    let b = card_balance_job(&mut till.cw, Some(&till.ops), &till.sales).await;
    assert_eq!(b.status, 200, "{:?}", b.body);
    let st = local(&till);
    assert!(st.pending_txs.is_empty() && st.host_pending.iter().all(|h| h.group != lost), "{st:?}");
    assert_eq!(till.sale_refs(), vec!["o3-u1".to_string(), "o3-u2".to_string()], "o3-u3 never became a sale");
    assert!(open_txs(&reg.state(&short_id).await?).is_empty());
    Ok(())
}
