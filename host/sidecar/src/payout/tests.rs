//! The payout engine against a fake wallet and a fake Lightning address service: decisions,
//! refusals, replays, every crash point a payout can stop at, and payouts racing the card's own
//! spends on the wallet's admission guard.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use lightning_invoice::Currency;
use reqwest::Url;
use serde_json::{Value, json};
use tokio::sync::{OwnedRwLockWriteGuard, RwLock};

use super::backend::{Backend, ReclaimStart, ReclaimState, Tagged};
use super::engine::{Engine, ManualLightning, NOT_EXPORTED};
use super::fees::{Linear, LnSchedule, Schedules};
use super::funds::{Obligations, Snapshot};
use super::lnurl::tests::{invoice_with, meta_hash, well_known};
use super::net::{Fetch, FetchError};
use super::store::{Kind, Mode, Origin, PayoutStore, Record, Settings, State};
use crate::wallet::{WalletPay, parse_bolt11};

const WAIT: Duration = Duration::from_millis(10);
const GATEWAY_FEE: u64 = 2_000;

#[derive(Clone, Copy, PartialEq)]
enum PayMode {
    Paid,
    Refunded,
    Unknown,
    /// Funded, then the call errored (a crash or a dropped connection after the commit).
    ErrAfterFunding,
}

#[derive(Clone, Copy, PartialEq)]
enum ExportMode {
    Ok,
    /// Spent and logged, then the call errored.
    ErrAfterSpend,
    /// Refused before anything was spent.
    ErrBefore,
}

struct Fake {
    guard: Arc<RwLock<()>>,
    balance: Mutex<u64>,
    obligations: Mutex<Result<Obligations, String>>,
    pay_mode: Mutex<PayMode>,
    /// `(bolt11, max_fee_msat)` of every payment the wallet was asked to fund.
    pays: Mutex<Vec<(String, u64)>>,
    /// The wallet's own record of each invoice it funded.
    status: Mutex<HashMap<String, WalletPay>>,
    export_mode: Mutex<ExportMode>,
    export_quote: Mutex<Result<u64, String>>,
    /// The wallet's operation log of tagged operations.
    log: Mutex<HashMap<String, Tagged>>,
    log_broken: AtomicBool,
    reclaim_start: Mutex<Option<ReclaimStart>>,
    reclaim_state: Mutex<ReclaimState>,
    min_note: Mutex<u64>,
    fed_fees: Mutex<Option<u64>>,
    schedules: Mutex<Schedules>,
    /// The client's own quote of a Lightning funding's federation fees.
    ln_quote: Mutex<Result<u64, String>>,
    /// Charges recorded as owed whose money has not reached the wallet yet.
    incoming: Mutex<u64>,
    /// Set if the wallet ever held (with what is on its way) less than it owes holders.
    short: AtomicBool,
}

impl Fake {
    fn new(balance_sat: u64) -> Self {
        Self {
            guard: Arc::new(RwLock::new(())),
            balance: Mutex::new(balance_sat * 1000),
            obligations: Mutex::new(Ok(Obligations::default())),
            pay_mode: Mutex::new(PayMode::Paid),
            pays: Mutex::new(vec![]),
            status: Mutex::new(HashMap::new()),
            export_mode: Mutex::new(ExportMode::Ok),
            export_quote: Mutex::new(Ok(0)),
            log: Mutex::new(HashMap::new()),
            log_broken: AtomicBool::new(false),
            reclaim_start: Mutex::new(None),
            reclaim_state: Mutex::new(ReclaimState::Done),
            min_note: Mutex::new(1000),
            fed_fees: Mutex::new(Some(0)),
            schedules: Mutex::new(Schedules::zero()),
            ln_quote: Mutex::new(Ok(0)),
            incoming: Mutex::new(0),
            short: AtomicBool::new(false),
        }
    }

    fn funded(&self) -> usize {
        self.pays.lock().unwrap().len()
    }

    fn paid(op: &str) -> WalletPay {
        WalletPay::Paid { preimage: "11".repeat(32), fee_msat: GATEWAY_FEE, operation_id: op.into() }
    }

    fn owed(&self) -> u64 {
        self.obligations.lock().unwrap().clone().map(|o| o.total()).unwrap_or(0)
    }

    fn check(&self) {
        let have = *self.balance.lock().unwrap() + *self.incoming.lock().unwrap();
        if have < self.owed() {
            self.short.store(true, Ordering::SeqCst);
        }
    }

    // The card's own spends, as the till's actor and pays make them, on the same guard.

    /// A top-up's load: the debit, then (after it) the record drops the obligation.
    async fn holder_load(&self, msat: u64) {
        let _g = self.guard.clone().read_owned().await;
        *self.balance.lock().unwrap() -= msat;
        tokio::task::yield_now().await;
        if let Ok(o) = self.obligations.lock().unwrap().as_mut() {
            o.topups_msat -= msat;
        }
        self.check();
    }

    /// A pay's `charging` recorded under the guard, then the card's charge paying into the wallet.
    async fn liability_then_credit(&self, msat: u64) {
        {
            let _g = self.guard.clone().read_owned().await;
            if let Ok(o) = self.obligations.lock().unwrap().as_mut() {
                o.pays_msat += msat;
            }
            *self.incoming.lock().unwrap() += msat;
        }
        tokio::task::yield_now().await;
        *self.balance.lock().unwrap() += msat;
        *self.incoming.lock().unwrap() -= msat;
        self.check();
    }

    /// A card issue: never spends what is owed (`till::holder_room`).
    async fn issue(&self, msat: u64) {
        let _g = self.guard.clone().read_owned().await;
        let (b, owed) = (*self.balance.lock().unwrap(), self.owed());
        if b.saturating_sub(owed) >= msat {
            tokio::task::yield_now().await;
            *self.balance.lock().unwrap() -= msat;
        }
        self.check();
    }
}

impl Backend for Fake {
    fn exclusive(&self) -> BoxFuture<'_, OwnedRwLockWriteGuard<()>> {
        Box::pin(self.guard.clone().write_owned())
    }

    fn snapshot(&self) -> BoxFuture<'_, anyhow::Result<Snapshot>> {
        // The claims first, then the balance, as the till's snapshot reads them.
        let o = self.obligations.lock().unwrap().clone().map_err(anyhow::Error::msg);
        let b = *self.balance.lock().unwrap();
        let schedules = self.schedules.lock().unwrap().clone();
        Box::pin(async move {
            // A read that takes a moment, as the wallet's does: other spends may run meanwhile.
            tokio::task::yield_now().await;
            Ok(Snapshot { balance_msat: b, obligations: o?, schedules })
        })
    }

    fn currency(&self) -> BoxFuture<'_, Option<Currency>> {
        Box::pin(async { Some(Currency::Regtest) })
    }

    fn ln_send_fee_quote(&self, _contract_msat: u64) -> BoxFuture<'_, anyhow::Result<u64>> {
        let q = self.ln_quote.lock().unwrap().clone().map_err(anyhow::Error::msg);
        Box::pin(async move { q })
    }

    fn quote_gateway_fee(&self, _bolt11: String) -> BoxFuture<'_, anyhow::Result<u64>> {
        Box::pin(async { Ok(GATEWAY_FEE) })
    }

    fn export_fee_quote(&self, _amount_msat: u64) -> BoxFuture<'_, anyhow::Result<u64>> {
        let q = self.export_quote.lock().unwrap().clone().map_err(anyhow::Error::msg);
        Box::pin(async move { q })
    }

    fn min_note_msat(&self) -> u64 {
        *self.min_note.lock().unwrap()
    }

    fn pay(&self, bolt11: String, max_fee_msat: u64, _wait: Duration) -> BoxFuture<'_, anyhow::Result<WalletPay>> {
        Box::pin(async move {
            self.pays.lock().unwrap().push((bolt11.clone(), max_fee_msat));
            let amount = parse_bolt11(&bolt11).unwrap().amount_milli_satoshis().unwrap();
            let op = format!("{:064x}", self.funded());
            let mode = *self.pay_mode.lock().unwrap();
            let answer = match mode {
                PayMode::Paid => {
                    *self.balance.lock().unwrap() -= amount + GATEWAY_FEE;
                    Fake::paid(&op)
                }
                PayMode::Refunded => WalletPay::Failed {
                    reason: "refunded".into(),
                    operation_id: Some(op.clone()),
                    insufficient_funds: false,
                    retryable: false,
                },
                PayMode::Unknown | PayMode::ErrAfterFunding => {
                    *self.balance.lock().unwrap() -= amount + GATEWAY_FEE;
                    WalletPay::Unknown { reason: "in flight".into(), operation_id: op.clone() }
                }
            };
            self.check();
            self.status.lock().unwrap().insert(bolt11, answer.clone());
            if mode == PayMode::ErrAfterFunding {
                anyhow::bail!("the connection dropped");
            }
            Ok(answer)
        })
    }

    fn resolve_pay(&self, operation_id: String, _wait: Duration) -> BoxFuture<'_, anyhow::Result<WalletPay>> {
        let s = self.status.lock().unwrap().values().find(|p| p.operation_id() == Some(operation_id.as_str())).cloned();
        Box::pin(async move { s.ok_or_else(|| anyhow::anyhow!("no such operation")) })
    }

    fn pay_status(&self, bolt11: String, _wait: Duration) -> BoxFuture<'_, anyhow::Result<Option<WalletPay>>> {
        let s = self.status.lock().unwrap().get(&bolt11).cloned();
        Box::pin(async move { Ok(s) })
    }

    fn operation_fees(&self, _operation_id: String) -> BoxFuture<'_, anyhow::Result<Option<u64>>> {
        let f = *self.fed_fees.lock().unwrap();
        Box::pin(async move { Ok(f) })
    }

    fn export(&self, amount_msat: u64, tag: String) -> BoxFuture<'_, anyhow::Result<(String, u64)>> {
        Box::pin(async move {
            let mode = *self.export_mode.lock().unwrap();
            if mode == ExportMode::ErrBefore {
                anyhow::bail!("insufficient balance");
            }
            *self.balance.lock().unwrap() -= amount_msat;
            let notes = format!("ecash-{tag}");
            self.log.lock().unwrap().insert(tag, Tagged::Export { notes: notes.clone() });
            if mode == ExportMode::ErrAfterSpend {
                anyhow::bail!("the process stopped after the spend");
            }
            Ok((notes, amount_msat))
        })
    }

    fn find_tagged(&self, tags: Vec<String>) -> BoxFuture<'_, anyhow::Result<HashMap<String, Tagged>>> {
        let r = if self.log_broken.load(Ordering::SeqCst) {
            Err(anyhow::anyhow!("the operation log cannot be read"))
        } else {
            let log = self.log.lock().unwrap();
            Ok(tags.into_iter().filter_map(|t| log.get(&t).map(|n| (t, n.clone()))).collect())
        };
        Box::pin(async move { r })
    }

    fn reclaim_start(&self, _notes: String, tag: String) -> BoxFuture<'_, anyhow::Result<ReclaimStart>> {
        let forced = self.reclaim_start.lock().unwrap().clone();
        let r = match forced {
            Some(f) => Ok(f),
            None => {
                let op = format!("re-{tag}");
                self.log.lock().unwrap().insert(tag, Tagged::Reclaim { operation_id: op.clone() });
                Ok(ReclaimStart::Started(op))
            }
        };
        Box::pin(async move { r })
    }

    fn reclaim_status(&self, _operation_id: String, _wait: Duration) -> BoxFuture<'_, anyhow::Result<ReclaimState>> {
        let s = self.reclaim_state.lock().unwrap().clone();
        Box::pin(async move { Ok(s) })
    }
}

struct Service {
    well_known: Mutex<Value>,
    /// Answer this invoice instead of a fresh one.
    pr: Mutex<Option<String>>,
    calls: Mutex<Vec<String>>,
}

impl Service {
    fn new() -> Self {
        Self { well_known: Mutex::new(well_known("https://example.com/lnurlp/shop/cb")), pr: Mutex::new(None), calls: Mutex::new(vec![]) }
    }
}

impl Fetch for Service {
    fn get_json(&self, url: Url) -> BoxFuture<'_, Result<Value, FetchError>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(url.to_string());
            if url.path().starts_with("/.well-known/lnurlp/") {
                return Ok(self.well_known.lock().unwrap().clone());
            }
            let amount: u64 = url.query_pairs().find(|(k, _)| k == "amount").map(|(_, v)| v.parse().unwrap()).unwrap();
            let fixed = self.pr.lock().unwrap().clone();
            let pr = fixed
                .unwrap_or_else(|| invoice_with(Currency::Regtest, amount, Some(meta_hash()), Duration::from_secs(600), rand::random()));
            Ok(json!({"pr": pr}))
        })
    }
}

struct Fx {
    dir: tempfile::TempDir,
    wallet: Fake,
    service: Service,
}

impl Fx {
    fn new(balance_sat: u64) -> Self {
        Self { dir: tempfile::tempdir().unwrap(), wallet: Fake::new(balance_sat), service: Service::new() }
    }

    fn engine(&self) -> Engine<'_> {
        Engine { dir: self.dir.path(), backend: &self.wallet, fetch: &self.service }
    }

    fn store(&self) -> PayoutStore {
        PayoutStore::open(self.dir.path()).unwrap()
    }

    fn settings(&self, f: impl FnOnce(&mut Settings)) {
        let mut s = self.store().file().settings;
        f(&mut s);
        self.engine().update_settings(s, false).unwrap();
    }

    fn mode(&self, mode: Mode) {
        self.settings(|s| s.mode = mode);
    }

    async fn confirmed(&self, mode: Mode) {
        self.mode(mode);
        let p = self.engine().propose_destination("shop@example.com").await.unwrap();
        assert_eq!(p.dest.callback_host, "example.com");
        self.engine().confirm_destination(&p.token).unwrap();
    }

    fn manual(&self, id: &str) -> Option<ManualLightning> {
        let dest = self.store().file().destination.clone().unwrap();
        Some(ManualLightning { request_id: id.into(), expected_destination: dest.address, expected_confirmed_at_ms: dest.confirmed_at_ms })
    }

    fn put(&self, rec: Record) {
        self.store().insert(rec).unwrap();
    }

    fn get(&self, id: &str) -> Record {
        self.store().get(id).cloned().unwrap()
    }
}

fn code(r: &Result<Record, crate::till::Reply>) -> String {
    match r {
        Ok(rec) => format!("ok:{}", rec.state.name()),
        Err(e) => e.body["error"].as_str().unwrap_or("").to_owned(),
    }
}

fn funding(bolt11: &str, amount_msat: u64) -> Record {
    let mut r = Record::new(Kind::Lightning, Origin::Manual, Some(format!("crash-{}", rand::random::<u32>())), amount_msat, 1);
    r.state = State::Funding;
    r.bolt11 = Some(bolt11.into());
    r.payment_hash = Some(parse_bolt11(bolt11).unwrap().payment_hash().to_string());
    r
}

fn invoice(msat: u64) -> String {
    invoice_with(Currency::Regtest, msat, Some(meta_hash()), Duration::from_secs(600), rand::random())
}

// ------------------------------------------------------------------ Lightning

#[tokio::test]
async fn manual_lightning_binds_the_reviewed_recipient_and_replays_the_recorded_withdrawal() {
    let fx = Fx::new(200_000);
    fx.confirmed(Mode::Both).await;
    let e = fx.engine();
    let reviewed = fx.manual("paid");
    let paid = e.lightning(reviewed.clone(), Some(1_000), WAIT).await.unwrap();
    let stale = fx.manual("new");
    let mut store = fx.store();
    store
        .commit(|f| {
            let dest = f.destination.as_mut().unwrap();
            dest.address = "other@example.com".into();
            dest.confirmed_at_ms += 1;
        })
        .unwrap();
    let calls = fx.service.calls.lock().unwrap().len();
    assert_eq!(code(&e.lightning(stale.clone(), Some(1_000), WAIT).await), "destination_changed");
    let mut reconfirmed = stale.unwrap();
    reconfirmed.expected_destination = "other@example.com".into();
    assert_eq!(code(&e.lightning(Some(reconfirmed), Some(1_000), WAIT).await), "destination_changed");
    assert!(fx.store().by_request("new").is_none());
    assert_eq!(e.lightning(reviewed, Some(1_000), WAIT).await.unwrap().id, paid.id);
    assert_eq!(code(&e.lightning(fx.manual("paid"), Some(1_000), WAIT).await), "request_conflict");
    assert_eq!(fx.service.calls.lock().unwrap().len(), calls);
    assert_eq!(fx.wallet.funded(), 1);
}

#[tokio::test]
async fn a_manual_payout_pays_the_exact_invoice_with_the_fee_cap_once_per_request() {
    let fx = Fx::new(200_000);
    fx.confirmed(Mode::Both).await;
    let e = fx.engine();
    let rec = e.lightning(fx.manual("r1"), Some(25_000), WAIT).await.unwrap();
    assert_eq!(rec.state, State::Paid);
    assert_eq!(rec.amount_msat, 25_000_000);
    assert_eq!(rec.fee_paid_msat, Some(GATEWAY_FEE));
    assert_eq!(rec.federation_fee_msat, Some(0));
    let pays = fx.wallet.pays.lock().unwrap().clone();
    assert_eq!(pays.len(), 1);
    // 0.5% of 25,000 sat is 125 sat: the 100 sat cap binds.
    assert_eq!(pays[0].1, 100_000);
    assert_eq!(parse_bolt11(&pays[0].0).unwrap().amount_milli_satoshis(), Some(25_000_000));
    assert!(fx.service.calls.lock().unwrap().iter().any(|c| c.ends_with("cb?amount=25000000")));

    // The same request answers the same payout and funds nothing more.
    let again = e.lightning(fx.manual("r1"), Some(25_000), WAIT).await.unwrap();
    assert_eq!(again.id, rec.id);
    assert_eq!(e.lightning(fx.manual("r1"), None, WAIT).await.unwrap().id, rec.id);
    assert_eq!(code(&e.lightning(fx.manual("r1"), Some(1), WAIT).await), "request_conflict");
    assert_eq!(fx.wallet.funded(), 1);
    let v = rec.to_json();
    assert!(v.get("bolt11").is_none() && v.get("operation_id").is_none(), "{v}");
}

#[tokio::test]
async fn the_mode_governs_lightning_only_and_ecash_is_always_open() {
    let fx = Fx::new(200_000);
    fx.confirmed(Mode::Auto).await;
    let e = fx.engine();
    assert_eq!(code(&e.lightning(fx.manual("m"), Some(1_000), WAIT).await), "mode_disallows");
    // Auto-only: eCash is the merchant's to take out all the same.
    assert_eq!(code(&e.ecash("x".into(), 1_000, WAIT).await), "ok:exported");
    fx.mode(Mode::Manual);
    assert_eq!(code(&e.lightning(None, Some(1_000), WAIT).await), "mode_disallows");
    fx.mode(Mode::Off);
    assert_eq!(code(&e.lightning(fx.manual("m"), Some(1_000), WAIT).await), "mode_disallows");
    assert_eq!(code(&e.ecash("y".into(), 1_000, WAIT).await), "ok:exported");
    assert_eq!(fx.wallet.funded(), 0);
}

#[tokio::test]
async fn holders_value_and_the_reserve_are_never_paid_out_and_unknown_obligations_stop_everything() {
    let fx = Fx::new(200_000);
    fx.confirmed(Mode::Both).await;
    let e = fx.engine();
    *fx.wallet.obligations.lock().unwrap() = Ok(Obligations { topups_msat: 100_000_000, pays_msat: 50_000_000, fees_msat: 0 });
    // 200,000 - 150,000 owed - 10,000 reserve = 40,000 sat, less the 100 sat fee cap.
    let r = e.lightning(fx.manual("big"), Some(40_000), WAIT).await;
    assert_eq!(code(&r), "insufficient_available");
    assert_eq!(r.unwrap_err().body["available_sat"], 39_900);
    let rec = e.lightning(fx.manual("all"), None, WAIT).await.unwrap();
    assert_eq!(rec.amount_msat, 39_900_000);
    assert!(!fx.wallet.short.load(Ordering::SeqCst));

    *fx.wallet.obligations.lock().unwrap() = Err("pays.json: version 9".into());
    assert_eq!(code(&e.lightning(fx.manual("x"), Some(1), WAIT).await), "obligations_unknown");
    assert_eq!(code(&e.ecash("y".into(), 1, WAIT).await), "obligations_unknown");
    let s = e.status().await.unwrap();
    assert_eq!(s["blocked"]["error"], "obligations_unknown");
    assert!(s["funds"].is_null());
    assert_eq!(fx.store().file().records.len(), 1, "no record for a refused payout");
}

#[tokio::test]
async fn payouts_racing_the_cards_own_spends_never_leave_holders_short_and_never_pay_twice() {
    let fx = Fx::new(300_000);
    fx.confirmed(Mode::Both).await;
    fx.settings(|s| s.reserve_sat = 5_000);
    *fx.wallet.obligations.lock().unwrap() = Ok(Obligations { topups_msat: 150_000_000, pays_msat: 0, fees_msat: 0 });
    // The admin payout lock: payouts are decided one at a time; the card's spends are not.
    let payout_lock = tokio::sync::Mutex::new(());
    let n = AtomicU64::new(0);
    let payouts = async {
        for i in 0..12 {
            let _one = payout_lock.lock().await;
            // Everything available each time, and a restart (a new engine over the same store)
            // every few payouts.
            let rid = format!("p{i}");
            let r = fx.engine().lightning(fx.manual(&rid), None, WAIT).await;
            if r.is_ok() {
                n.fetch_add(1, Ordering::SeqCst);
            }
            // The same request again, from a new engine (a restart): the same payout, never a
            // second funding.
            if let Some(had) = fx.store().by_request(&rid).cloned() {
                let funded = fx.wallet.funded();
                let again = fx.engine().lightning(fx.manual(&rid), None, WAIT).await;
                let id = match &again {
                    Ok(rec) => rec.id.clone(),
                    Err(e) => e.body["payout"]["id"].as_str().unwrap_or_default().to_owned(),
                };
                assert!(id.is_empty() || id == had.id, "{id} vs {}", had.id);
                assert_eq!(fx.wallet.funded(), funded);
            }
            tokio::task::yield_now().await;
        }
    };
    let cards = async {
        for _ in 0..12 {
            fx.wallet.holder_load(10_000_000).await;
            fx.wallet.liability_then_credit(7_000_000).await;
            fx.wallet.issue(20_000_000).await;
            tokio::task::yield_now().await;
        }
    };
    tokio::join!(payouts, cards);
    assert!(!fx.wallet.short.load(Ordering::SeqCst), "the wallet never held less than it owes holders");
    let owed = fx.wallet.owed();
    let balance = *fx.wallet.balance.lock().unwrap();
    assert!(balance >= owed, "{balance} < {owed}");
    let paid: Vec<Record> = fx.store().file().records.iter().filter(|r| r.state == State::Paid).cloned().collect();
    assert_eq!(paid.len(), fx.wallet.funded(), "each funded payment is one paid payout");
    assert_eq!(paid.len() as u64, n.load(Ordering::SeqCst));
    let hashes: std::collections::HashSet<_> = paid.iter().map(|r| r.payment_hash.clone()).collect();
    assert_eq!(hashes.len(), paid.len(), "no invoice paid twice");

    // A restart finds nothing to do and funds nothing.
    let funded = fx.wallet.funded();
    fx.engine().sweep(WAIT).await;
    assert_eq!(fx.wallet.funded(), funded);
}

#[tokio::test]
async fn an_invoice_that_is_not_exactly_the_one_asked_for_is_never_funded() {
    let fx = Fx::new(200_000);
    fx.confirmed(Mode::Both).await;
    let e = fx.engine();
    for (rid, bad) in [
        ("amount", invoice_with(Currency::Regtest, 25_000_001, Some(meta_hash()), Duration::from_secs(600), rand::random())),
        ("network", invoice_with(Currency::Bitcoin, 25_000_000, Some(meta_hash()), Duration::from_secs(600), rand::random())),
        ("hash", invoice_with(Currency::Regtest, 25_000_000, Some([3; 32]), Duration::from_secs(600), rand::random())),
        ("expiry", invoice_with(Currency::Regtest, 25_000_000, Some(meta_hash()), Duration::from_secs(10), rand::random())),
    ] {
        *fx.service.pr.lock().unwrap() = Some(bad);
        let r = e.lightning(fx.manual(rid), Some(25_000), WAIT).await;
        assert_eq!(code(&r), "invoice_rejected", "{rid}");
        assert_eq!(r.unwrap_err().body["payout"]["state"], "failed");
    }
    assert_eq!(fx.wallet.funded(), 0);

    let one = invoice(25_000_000);
    *fx.service.pr.lock().unwrap() = Some(one);
    assert_eq!(code(&e.lightning(fx.manual("first"), Some(25_000), WAIT).await), "ok:paid");
    assert_eq!(code(&e.lightning(fx.manual("second"), Some(25_000), WAIT).await), "invoice_rejected");
    assert_eq!(fx.wallet.funded(), 1);

    // A gateway asking more than the cap: refused under the guard, nothing funded.
    *fx.service.pr.lock().unwrap() = None;
    fx.settings(|s| s.max_fee_sat = 1);
    assert_eq!(code(&e.lightning(fx.manual("cap"), Some(25_000), WAIT).await), "fee_too_high");
    assert_eq!(fx.wallet.funded(), 1);
}

#[tokio::test]
async fn a_service_that_changes_after_confirmation_stops_payouts_until_confirmed_again() {
    let fx = Fx::new(200_000);
    fx.confirmed(Mode::Both).await;
    let e = fx.engine();
    *fx.service.well_known.lock().unwrap() = well_known("https://pay.example.com/cb");
    assert_eq!(code(&e.lightning(fx.manual("a"), Some(1_000), WAIT).await), "destination_changed");
    assert!(fx.store().file().destination.as_ref().unwrap().problem.is_some());
    let calls = fx.service.calls.lock().unwrap().len();
    assert_eq!(code(&e.lightning(fx.manual("b"), Some(1_000), WAIT).await), "destination_changed");
    assert_eq!(fx.service.calls.lock().unwrap().len(), calls);
    assert_eq!(e.status().await.unwrap()["blocked"]["error"], "destination_changed");

    let p = e.propose_destination("shop@example.com").await.unwrap();
    e.confirm_destination(&p.token).unwrap();
    let mut other = well_known("https://pay.example.com/cb");
    other["metadata"] = json!(r#"[["text/plain","x"],["text/identifier","thief@example.com"]]"#);
    *fx.service.well_known.lock().unwrap() = other;
    assert_eq!(code(&e.lightning(fx.manual("c"), Some(1_000), WAIT).await), "lnurl_identity");
    assert!(fx.store().file().destination.as_ref().unwrap().problem.is_some());
    assert_eq!(fx.wallet.funded(), 0);
}

#[tokio::test]
async fn a_destination_is_confirmed_only_with_its_token_before_it_expires() {
    let fx = Fx::new(1);
    let e = fx.engine();
    assert_eq!(e.confirm_destination("00").unwrap_err().body["error"], "no_pending_destination");
    assert_eq!(e.propose_destination("shop@127.0.0.1").await.unwrap_err().body["error"], "bad_address");
    let p = e.propose_destination("shop@example.com").await.unwrap();
    assert_eq!(e.confirm_destination(&"ab".repeat(32)).unwrap_err().body["error"], "confirm_token_mismatch");
    assert!(fx.store().file().destination.is_none());
    fx.store().commit(|f| f.pending.as_mut().unwrap().expires_at_ms = 1).unwrap();
    assert_eq!(e.confirm_destination(&p.token).unwrap_err().body["error"], "pending_expired");
    let p = e.propose_destination("shop@example.com").await.unwrap();
    e.confirm_destination(&p.token).unwrap();
    assert_eq!(fx.store().file().destination.clone().unwrap().address, "shop@example.com");
    e.remove_destination().unwrap();
    assert!(fx.store().file().destination.is_none());
}

#[tokio::test]
async fn a_payout_stopped_anywhere_is_settled_from_the_wallet_and_never_paid_twice() {
    let fx = Fx::new(200_000);
    fx.confirmed(Mode::Both).await;
    let e = fx.engine();

    let resolving = Record::new(Kind::Lightning, Origin::Auto, None, 5_000_000, 1);
    fx.put(resolving.clone());
    let never = funding(&invoice(6_000_000), 6_000_000);
    fx.put(never.clone());
    let paid = funding(&invoice(7_000_000), 7_000_000);
    fx.wallet.status.lock().unwrap().insert(paid.bolt11.clone().unwrap(), Fake::paid("ab"));
    fx.put(paid.clone());
    let mut flight = funding(&invoice(8_000_000), 8_000_000);
    flight.state = State::Invoiced;
    fx.wallet
        .status
        .lock()
        .unwrap()
        .insert(flight.bolt11.clone().unwrap(), WalletPay::Unknown { reason: "in flight".into(), operation_id: "cd".into() });
    fx.put(flight.clone());

    assert_eq!(code(&e.lightning(fx.manual("new"), Some(1_000), WAIT).await), "payout_in_progress");
    assert_eq!(e.status().await.unwrap()["funds"]["open_payouts_sat"], 5_000 + 6_000 + 7_000 + 8_000);

    e.sweep(WAIT).await;
    assert_eq!(fx.get(&resolving.id).state, State::Failed);
    assert_eq!(fx.get(&never.id).state, State::Failed);
    assert_eq!(fx.get(&paid.id).state, State::Paid);
    let f = fx.get(&flight.id);
    assert_eq!((f.state, f.operation_id.as_deref()), (State::Funding, Some("cd")));
    assert_eq!(fx.wallet.funded(), 0, "recovery never pays");

    assert_eq!(e.lightning(fx.manual(flight.request_id.as_ref().unwrap()), None, WAIT).await.unwrap().state, State::Funding);
    fx.wallet.status.lock().unwrap().insert(flight.bolt11.clone().unwrap(), Fake::paid("cd"));
    e.sweep(WAIT).await;
    assert_eq!(fx.get(&flight.id).state, State::Paid);
    assert_eq!(fx.wallet.funded(), 0);
}

#[tokio::test]
async fn a_payment_whose_call_errors_after_funding_stays_open_until_the_wallet_settles_it() {
    let fx = Fx::new(200_000);
    fx.confirmed(Mode::Both).await;
    let e = fx.engine();
    *fx.wallet.pay_mode.lock().unwrap() = PayMode::ErrAfterFunding;
    let rec = e.lightning(fx.manual("e"), Some(10_000), WAIT).await.unwrap();
    assert_eq!(rec.state, State::Funding);
    e.sweep(WAIT).await;
    assert_eq!(fx.get(&rec.id).state, State::Funding, "unsettled is not failed");
    let bolt11 = fx.get(&rec.id).bolt11.unwrap();
    fx.wallet.status.lock().unwrap().insert(
        bolt11,
        WalletPay::Failed { reason: "refunded".into(), operation_id: Some("ef".into()), insufficient_funds: false, retryable: false },
    );
    e.sweep(WAIT).await;
    assert_eq!(fx.get(&rec.id).state, State::Failed);
    assert_eq!(fx.wallet.funded(), 1);

    *fx.wallet.pay_mode.lock().unwrap() = PayMode::Refunded;
    assert_eq!(code(&e.lightning(fx.manual("r"), Some(10_000), WAIT).await), "payment_failed");
    *fx.wallet.pay_mode.lock().unwrap() = PayMode::Unknown;
    assert_eq!(code(&e.lightning(fx.manual("u"), Some(10_000), WAIT).await), "ok:funding");
}

#[tokio::test]
async fn federation_fees_above_what_was_held_back_halt_payouts_until_resumed() {
    let fx = Fx::new(200_000);
    fx.confirmed(Mode::Both).await;
    let e = fx.engine();
    // The cap is 100 sat and the fake's bound 0: 2 sat to the gateway and 99 sat to the federation
    // is 1 sat more than was held back.
    *fx.wallet.fed_fees.lock().unwrap() = Some(99_000);
    assert_eq!(code(&e.lightning(fx.manual("a"), Some(25_000), WAIT).await), "ok:paid");
    assert!(fx.store().file().halted.is_some());
    assert_eq!(code(&e.lightning(fx.manual("b"), Some(1_000), WAIT).await), "halted");
    assert_eq!(code(&e.ecash("c".into(), 1_000, WAIT).await), "halted");
    assert_eq!(e.status().await.unwrap()["blocked"]["error"], "halted");
    let s = fx.store().file().settings;
    e.update_settings(s, true).unwrap();
    *fx.wallet.fed_fees.lock().unwrap() = Some(0);
    assert_eq!(code(&e.lightning(fx.manual("b"), Some(1_000), WAIT).await), "ok:paid");
}

#[tokio::test]
async fn the_worker_pays_out_everything_available_on_its_cadence_above_the_threshold() {
    let fx = Fx::new(40_000);
    fx.confirmed(Mode::Auto).await;
    let e = fx.engine();
    e.sweep(WAIT).await;
    assert!(fx.store().file().records.is_empty());
    fx.store().commit(|f| f.next_auto_at_ms = 0).unwrap();
    e.sweep(WAIT).await;
    assert!(fx.store().file().records.is_empty());

    *fx.wallet.balance.lock().unwrap() = 100_000_000;
    e.sweep(WAIT).await;
    let recs = fx.store().file().records.clone();
    assert_eq!(recs.len(), 1);
    assert_eq!((recs[0].origin, recs[0].state), (Origin::Auto, State::Paid));
    assert_eq!(recs[0].amount_msat, 89_900_000);
    assert!(fx.store().file().next_auto_at_ms > super::engine::now_ms() + 23 * 3_600_000);
    e.sweep(WAIT).await;
    assert_eq!(fx.store().file().records.len(), 1);

    fx.store().commit(|f| f.next_auto_at_ms = 0).unwrap();
    *fx.wallet.balance.lock().unwrap() = 200_000_000;
    *fx.wallet.pay_mode.lock().unwrap() = PayMode::Refunded;
    e.sweep(WAIT).await;
    assert!(fx.store().file().next_auto_at_ms <= super::engine::now_ms() + 15 * 60_000);
}

// ------------------------------------------------------------------ eCash

#[tokio::test]
async fn an_ecash_export_holds_its_exact_fee_quote_and_is_refused_without_one() {
    let fx = Fx::new(30_000);
    let e = fx.engine();
    // 30,000 - 10,000 reserve: 20,000 sat, less a 500 sat quote.
    *fx.wallet.export_quote.lock().unwrap() = Ok(500_000);
    let r = e.ecash("a".into(), 20_000, WAIT).await;
    assert_eq!(code(&r), "insufficient_available");
    assert_eq!(r.unwrap_err().body["available_sat"], 19_500);
    *fx.wallet.export_quote.lock().unwrap() = Err("no quote".into());
    assert_eq!(code(&e.ecash("b".into(), 1_000, WAIT).await), "fee_unknown");
    assert!(fx.store().file().records.is_empty());
    *fx.wallet.export_quote.lock().unwrap() = Ok(500_000);
    assert_eq!(code(&e.ecash("c".into(), 19_500, WAIT).await), "ok:exported");
}

#[tokio::test]
async fn delivered_notes_are_kept_and_a_reclaim_ends_as_the_wallets_reissue_ends() {
    let fx = Fx::new(100_000);
    let e = fx.engine();
    let rec = e.ecash("x1".into(), 20_000, WAIT).await.unwrap();
    assert_eq!(rec.state, State::Exported);
    let notes = format!("ecash-{}", rec.export_tag());
    assert_eq!(rec.to_json()["notes"], notes);
    assert_eq!(e.ecash("x1".into(), 20_000, WAIT).await.unwrap().id, rec.id, "a replay exports nothing more");
    assert_eq!(code(&e.ecash("x1".into(), 20_001, WAIT).await), "request_conflict");

    // Marked delivered: the notes stay, the only copy may be this one.
    let d = e.delivered(&rec.id).unwrap();
    assert!(d.delivered_at_ms.is_some());
    assert_eq!(d.notes.as_deref(), Some(notes.as_str()));
    assert!(std::fs::read_to_string(PayoutStore::path_in(fx.dir.path())).unwrap().contains(&notes));

    // Rejected by the federation: kept, exported, never called redeemed.
    *fx.wallet.reclaim_state.lock().unwrap() = ReclaimState::Rejected("spent".into());
    assert_eq!(code(&e.reclaim(&rec.id, WAIT).await), "reclaim_rejected");
    let r = fx.get(&rec.id);
    assert_eq!((r.state, r.notes.as_deref()), (State::Exported, Some(notes.as_str())));

    // Still running: reclaiming, notes kept; the worker settles it from the reissue.
    let two = e.ecash("x2".into(), 10_000, WAIT).await.unwrap();
    *fx.wallet.reclaim_state.lock().unwrap() = ReclaimState::Pending;
    assert_eq!(code(&e.reclaim(&two.id, WAIT).await), "ok:reclaiming");
    assert!(fx.get(&two.id).notes.is_some());
    *fx.wallet.reclaim_state.lock().unwrap() = ReclaimState::Done;
    e.sweep(WAIT).await;
    let t = fx.get(&two.id);
    assert_eq!((t.state, t.notes.is_none()), (State::Reclaimed, true));
    assert_eq!(code(&e.reclaim(&two.id, WAIT).await), "not_exported");

    // Stopped after `reclaiming` was saved, before the reissue's id: found by its tag.
    let three = e.ecash("x3".into(), 1_000, WAIT).await.unwrap();
    let mut cut = fx.get(&three.id);
    cut.state = State::Reclaiming;
    fx.store().put(&cut).unwrap();
    fx.wallet.log.lock().unwrap().insert(cut.reclaim_tag(), Tagged::Reclaim { operation_id: "op3".into() });
    e.sweep(WAIT).await;
    let t = fx.get(&three.id);
    assert_eq!((t.state, t.reclaim_operation_id.as_deref()), (State::Reclaimed, Some("op3")));

    // Stopped before the reissue started: back to exported, notes kept, retry possible.
    let four = e.ecash("x4".into(), 1_000, WAIT).await.unwrap();
    let mut cut = fx.get(&four.id);
    cut.state = State::Reclaiming;
    fx.store().put(&cut).unwrap();
    e.sweep(WAIT).await;
    assert_eq!(fx.get(&four.id).state, State::Exported);
    // Reissued outside this payout (the operator's own reclaim): not ours to call done.
    *fx.wallet.reclaim_start.lock().unwrap() = Some(ReclaimStart::AlreadyReissued);
    assert_eq!(code(&e.reclaim(&four.id, WAIT).await), "reissued_elsewhere");
    assert_eq!(fx.get(&four.id).state, State::Exported);
}

#[tokio::test]
async fn an_export_stopped_between_the_spend_and_the_save_is_found_by_its_tag() {
    let fx = Fx::new(100_000);
    let e = fx.engine();

    *fx.wallet.export_mode.lock().unwrap() = ExportMode::ErrAfterSpend;
    let rec = e.ecash("a".into(), 5_000, WAIT).await.unwrap();
    assert_eq!(rec.state, State::Exported);
    assert_eq!(rec.notes.as_deref(), Some(format!("ecash-{}", rec.export_tag()).as_str()));

    *fx.wallet.export_mode.lock().unwrap() = ExportMode::ErrBefore;
    let r = e.ecash("b".into(), 5_000, WAIT).await;
    assert_eq!(code(&r), "export_failed");
    assert_eq!(r.unwrap_err().body["payout"]["detail"], NOT_EXPORTED);

    let mut crashed = Record::new(Kind::Ecash, Origin::Manual, Some("c".into()), 7_000_000, 1);
    fx.wallet.log.lock().unwrap().insert(crashed.export_tag(), Tagged::Export { notes: "ecash-crashed".into() });
    crashed.state = State::Exporting;
    fx.put(crashed.clone());
    fx.wallet.log_broken.store(true, Ordering::SeqCst);
    e.sweep(WAIT).await;
    assert_eq!(fx.get(&crashed.id).state, State::Exporting);
    assert_eq!(code(&e.ecash("d".into(), 1_000, WAIT).await), "payout_in_progress");
    assert_eq!(e.status().await.unwrap()["funds"]["open_payouts_sat"], 7_000);
    fx.wallet.log_broken.store(false, Ordering::SeqCst);
    e.sweep(WAIT).await;
    let found = fx.get(&crashed.id);
    assert_eq!((found.state, found.notes.as_deref()), (State::Exported, Some("ecash-crashed")));

    let mut late = Record::new(Kind::Ecash, Origin::Manual, Some("late".into()), 1_000_000, super::engine::now_ms());
    late.state = State::Failed;
    late.detail = Some(NOT_EXPORTED.into());
    fx.put(late.clone());
    fx.wallet.log.lock().unwrap().insert(late.export_tag(), Tagged::Export { notes: "ecash-late".into() });
    e.sweep(WAIT).await;
    assert_eq!(fx.get(&late.id).state, State::Exported);
}

#[test]
fn exported_notes_come_with_a_qr_when_they_fit_and_always_as_text() {
    let mut r = Record::new(Kind::Ecash, Origin::Manual, None, 1_000, 1);
    r.state = State::Exported;
    r.notes = Some("A".repeat(100));
    let v = super::payout_json(&r);
    assert!(v["qr_svg"].as_str().unwrap().starts_with("<svg"));
    assert_eq!(v["notes"], "A".repeat(100));
    r.notes = Some("A".repeat(8_000));
    let v = super::payout_json(&r);
    assert!(v["qr_svg"].is_null());
    assert_eq!(v["qr_error"], "too_long");
    assert_eq!(v["notes"].as_str().unwrap().len(), 8_000);
}

// ------------------------------------------------------------------ fee schedules

/// What a Lightning payout holds back on each schedule the mint allows, ln and lnv2, and that the
/// wallet's debit stays within it.
#[tokio::test]
async fn a_lightning_payout_holds_back_the_provable_fee_bound_on_mixed_fee_schedules() {
    let v1 = LnSchedule::V1 { contract_input_msat: 0, contract_output_msat: 2_000 };
    let v2 = LnSchedule::V2(Linear { base_msat: 1_000, ppm: 1_000 });
    for (mint, ln) in [
        (Linear::ZERO, v1),
        (Linear { base_msat: 100, ppm: 0 }, v1),
        (Linear { base_msat: 100, ppm: 1_000 }, v1),
        (Linear { base_msat: 100, ppm: 1_000 }, v2),
    ] {
        let fx = Fx::new(500_000);
        *fx.wallet.schedules.lock().unwrap() = Schedules { mint, min_change_msat: 1, ln: Some(ln), ln_problem: None };
        fx.confirmed(Mode::Both).await;
        let rid = format!("{mint:?}{ln:?}");
        let rec = fx.engine().lightning(fx.manual(&rid), Some(25_000), WAIT).await.unwrap();
        assert_eq!(rec.state, State::Paid, "{rid}");
        let s = Schedules { mint, min_change_msat: 1, ln: Some(ln), ln_problem: None };
        // The bound at the balance the final snapshot read (nothing was spent before it).
        let want = super::fees::ln_send_fee_bound_over(&s, 500_000_000, 25_000_000, 25_100_000).unwrap();
        assert_eq!(rec.fee_allowance_msat, want, "{rid}");
        // The fake's debit (amount and the gateway's fee) is within what was held back.
        let spent = 500_000_000 - *fx.wallet.balance.lock().unwrap();
        assert!(spent <= rec.amount_msat + rec.fee_cap_msat + rec.fee_allowance_msat, "{rid}");
        // Typical schedule: about 104 sat for the federation, whatever the amount.
        if mint == (Linear { base_msat: 100, ppm: 0 }) {
            assert_eq!(want, 100 * 1_041 + 100 + 2_000);
        }
    }
}

#[tokio::test]
async fn a_lightning_schedule_that_cannot_be_bounded_refuses_lightning_and_leaves_ecash() {
    let fx = Fx::new(200_000);
    fx.confirmed(Mode::Both).await;
    *fx.wallet.schedules.lock().unwrap() = Schedules {
        min_change_msat: 1,
        mint: Linear { base_msat: 100, ppm: 0 },
        ln: None,
        ln_problem: Some("the lnv2 module's fees cannot be bounded".into()),
    };
    let r = fx.engine().lightning(fx.manual("ln"), Some(1_000), WAIT).await;
    assert_eq!(code(&r), "fee_unbounded");
    assert!(r.unwrap_err().body["detail"].as_str().unwrap().contains("lnv2"));
    assert!(fx.store().file().records.is_empty(), "nothing recorded");
    assert_eq!(fx.wallet.funded(), 0);
    let st = fx.engine().status().await.unwrap();
    assert_eq!(st["funds"]["lightning_blocked"]["error"], "fee_unbounded");
    assert_eq!(st["funds"]["lightning_blocked"]["reason"], "fee_unbounded");
    fx.wallet.schedules.lock().unwrap().ln_problem = None;
    assert_eq!(fx.engine().status().await.unwrap()["funds"]["lightning_blocked"]["reason"], "no_lightning_module");
    assert_eq!(st["funds"]["available_sat"], 0);
    // eCash needs only the mint's schedule.
    assert_eq!(code(&fx.engine().ecash("x".into(), 1_000, WAIT).await), "ok:exported");
}

#[tokio::test]
async fn a_client_quote_above_the_bound_funds_nothing() {
    let fx = Fx::new(200_000);
    fx.confirmed(Mode::Both).await;
    *fx.wallet.ln_quote.lock().unwrap() = Ok(1);
    let r = fx.engine().lightning(fx.manual("q"), Some(10_000), WAIT).await;
    assert_eq!(code(&r), "fee_unbounded");
    assert_eq!(r.unwrap_err().body["payout"]["state"], "failed");
    *fx.wallet.ln_quote.lock().unwrap() = Err("no notes".into());
    assert_eq!(code(&fx.engine().lightning(fx.manual("q2"), Some(10_000), WAIT).await), "fee_unknown");
    assert_eq!(fx.wallet.funded(), 0);
    *fx.wallet.ln_quote.lock().unwrap() = Ok(0);
    assert_eq!(code(&fx.engine().lightning(fx.manual("q3"), Some(10_000), WAIT).await), "ok:paid");
}

#[test]
fn settings_round_trip_with_defaults() {
    let s = Settings::default();
    assert_eq!(s.mode, Mode::Auto);
    assert_eq!(s.to_json()["interval_minutes"], 1_440);
}
