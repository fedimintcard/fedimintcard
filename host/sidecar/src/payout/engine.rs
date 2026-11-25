//! Payout decisions. Every function here that writes expects the caller to hold the admin payout
//! lock ([`crate::admin::AdminState::payout_lock`]): decisions are made one at a time, from the
//! store as last saved, and at most one payout is open.
//!
//! The money moves under the wallet's admission guard, exclusive ([`Backend::exclusive`],
//! `crate::wallet::SpendGuard`): the final snapshot (the store as saved, then the obligations,
//! then the balance) and the wallet call that spends are made with no other debit and no new
//! liability between them. Every other debit (a top-up's load, a pay's funding, a refund, a card
//! issue, an offline send) and every new liability (a pay's `charging`, a top-up's `waiting`)
//! holds the guard shared. The guard is waited for at most [`GUARD_WAIT`]; a payout that cannot
//! have it then funds nothing.
//!
//! Lightning, each state saved before the step it guards:
//!
//! 1. `resolving`: the decision (amount, fee cap, address) is recorded before any network I/O.
//! 2. The address is resolved and its callback asked for exactly that amount; the invoice must be
//!    exactly what was asked ([`super::lnurl::check_invoice`]) and its payment hash new.
//! 3. `invoiced`: the invoice is saved.
//! 4. Under the guard: the final snapshot, the gateway's fee quoted (at most the cap), the
//!    federation's fees bounded; `funding` saved; the wallet funds the payment. The guard is let
//!    go once the wallet has answered (at most [`FUND_WAIT`] after the funding).
//! 5. `paid` or `failed` from the wallet's answer, followed outside the guard while in flight;
//!    once paid, the federation fees the wallet's record names are checked against what was
//!    held back, and an overrun halts new payouts until the merchant resumes.
//!
//! After a crash: `resolving` is failed (nothing can have been funded without a saved invoice);
//! `invoiced` and `funding` are settled from the wallet's own record of the invoice
//! ([`Backend::pay_status`]): never funded is `failed`, anything else is followed. A payout is
//! never started again; the next decision is a new payout with a new invoice.
//!
//! eCash: under the guard, the mint's exact fee quote held back, `exporting` saved before the
//! wallet spends (tagged); the notes are saved with `exported`. A spend that errors, or a crash
//! anywhere, is settled from the wallet's operation log by the tag ([`Backend::find_tagged`]).
//! While that log cannot be read the payout stays `exporting`: it blocks every new payout and its
//! amount stays held. A reclaim is `reclaiming` (saved, then a tagged reissue) and ends as the
//! reissue operation itself ends; the notes are wiped only once the wallet's reissue is done.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::OwnedRwLockWriteGuard;

use super::backend::{Backend, ReclaimStart, ReclaimState, Tagged};
use super::fees::{self, Schedules};
use super::funds::Obligations;
use super::lnurl::{self, Expect, LnAddress, LnurlError};
use super::net::{Fetch, FetchError};
use super::store::{Destination, Kind, Origin, PayoutStore, PendingDestination, Record, Settings, State};
use crate::till::Reply;
use crate::wallet::WalletPay;

/// Least time an invoice must have left when it is accepted.
pub const MIN_EXPIRY_SECS: u64 = 120;
/// A failed automatic payout is tried again after this long (or the interval, if shorter).
pub const RETRY_MS: u64 = 15 * 60_000;
/// How long a resolved destination waits for the merchant's confirm.
pub const PENDING_TTL_MS: u64 = 10 * 60_000;
/// How long a failed eCash export is looked for again in the wallet's log.
pub const LATE_EXPORT_MS: u64 = 24 * 3_600_000;
/// The longest a payout waits for the wallet's admission guard.
pub const GUARD_WAIT: Duration = Duration::from_secs(15);
/// How long the wallet follows a payment while the guard is held; the rest is followed after.
pub const FUND_WAIT: Duration = Duration::from_secs(3);
/// The detail of an eCash payout the wallet's log proved was never exported.
pub const NOT_EXPORTED: &str = "the wallet never exported it; nothing left the wallet";
const RECENT: usize = 50;

/// The destination shown to the merchant before a manual withdrawal.
#[derive(Clone, Debug)]
pub struct ManualLightning {
    pub request_id: String,
    pub expected_destination: String,
    pub expected_confirmed_at_ms: u64,
}

pub fn now_ms() -> u64 {
    crate::topup::now_ms()
}

/// Smallest whole-sat payment representable by this mint's note denominations.
fn ecash_step_sat(note_msat: u64) -> u64 {
    let note_msat = note_msat.max(1);
    let (mut a, mut b) = (note_msat, 1000);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    note_msat / a
}

/// The wallet's money, as a payout sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Funds {
    pub balance_msat: u64,
    pub obligations: Obligations,
    /// Held by open payouts.
    pub open_msat: u64,
    pub reserve_msat: u64,
    /// Balance less all of the above.
    pub spendable_msat: u64,
    pub schedules: Schedules,
}

impl Funds {
    /// The bound on the federation's fees of funding a contract anywhere in `lo..=hi`
    /// ([`fees::ln_send_fee_bound_over`]); a schedule that cannot be bounded refuses.
    pub fn ln_fee_bound(&self, lo: u64, hi: u64) -> Result<u64, Reply> {
        fees::ln_send_fee_bound_over(&self.schedules, self.balance_msat, lo, hi)
            .map_err(|e| Reply::fail(409, "fee_unbounded", format!("Lightning payouts are off: {e}")))
    }
}

pub struct Engine<'a> {
    pub dir: &'a Path,
    pub backend: &'a dyn Backend,
    pub fetch: &'a dyn Fetch,
}

fn store_failed(e: anyhow::Error) -> Reply {
    Reply::fail(503, "payout_store", format!("the payout record could not be saved: {e:#}"))
}

fn with_payout(mut r: Reply, rec: &Record) -> Reply {
    r.body["payout"] = rec.summary_json();
    r
}

/// A refusal's detail, for a record.
fn detail_of(r: &Reply) -> String {
    r.body["detail"].as_str().unwrap_or("").to_owned()
}

fn in_progress(open: &Record) -> Reply {
    with_payout(Reply::fail(409, "payout_in_progress", "another payout is still settling"), open)
}

impl Engine<'_> {
    pub fn store(&self) -> Result<PayoutStore, Reply> {
        PayoutStore::open(self.dir).map_err(|e| Reply::fail(503, "payout_store", format!("{e:#}")))
    }

    async fn exclusive(&self) -> Result<OwnedRwLockWriteGuard<()>, Reply> {
        tokio::time::timeout(GUARD_WAIT, self.backend.exclusive())
            .await
            .map_err(|_| Reply::fail(503, "wallet_busy", "other wallet spends are running; nothing was paid out, try again shortly"))
    }

    /// The balance and the obligations, read in the backend's snapshot order (the claims, the
    /// balance, then the evidence that releases a claim: `funds::snapshot`).
    pub async fn funds(&self, store: &PayoutStore) -> Result<Funds, Reply> {
        let snap = self.backend.snapshot().await.map_err(|e| {
            Reply::fail(
                503,
                "obligations_unknown",
                format!("what the wallet holds for holders cannot be read or bounded, so nothing is paid out: {e:#}"),
            )
        })?;
        let open_msat = store.open_records().fold(0u64, |a, r| a.saturating_add(r.held_msat()));
        let reserve_msat = store.file().settings.reserve_sat.saturating_mul(1000);
        let spendable_msat =
            snap.balance_msat.saturating_sub(snap.obligations.total()).saturating_sub(open_msat).saturating_sub(reserve_msat);
        Ok(Funds {
            balance_msat: snap.balance_msat,
            obligations: snap.obligations,
            open_msat,
            reserve_msat,
            spendable_msat,
            schedules: snap.schedules,
        })
    }

    /// A Lightning payout of `amount_msat` with everything it may cost: the gateway's fee up to the
    /// cap (the contract is `amount..=amount + cap`), and the federation's fee bound over that.
    fn lightning_cost(&self, s: &Settings, f: &Funds, amount_msat: u64) -> Result<u64, Reply> {
        let cap = s.fee_cap_msat(amount_msat);
        let fed = f.ln_fee_bound(amount_msat, amount_msat.saturating_add(cap))?;
        Ok(amount_msat.saturating_add(cap).saturating_add(fed))
    }

    /// The largest Lightning payout, whole sats, whose amount and fees fit in what is spendable.
    /// The fee cap grows with the amount, so that of the whole bounds it; the federation's bound
    /// is taken over every contract up to the whole.
    pub fn max_lightning_msat(&self, s: &Settings, f: &Funds) -> Result<u64, Reply> {
        let all = f.spendable_msat;
        let fed = f.ln_fee_bound(0, all)?;
        Ok(all.saturating_sub(s.fee_cap_msat(all)).saturating_sub(fed) / 1000 * 1000)
    }

    fn save(&self, store: &mut PayoutStore, rec: &mut Record) -> Result<(), Reply> {
        rec.updated_at_ms = now_ms();
        store.put(rec).map_err(store_failed)
    }

    /// Save what already happened. When the save fails the store on disk is behind, and the
    /// sweeper settles the payout again from the wallet.
    fn record(&self, store: &mut PayoutStore, rec: &mut Record) {
        rec.updated_at_ms = now_ms();
        if let Err(e) = store.put(rec) {
            tracing::warn!("payout {}: {} not saved ({e:#}); settled again from the wallet", rec.id, rec.state.name());
        } else {
            tracing::info!("payout {}: {}", rec.id, rec.state.name());
        }
    }

    /// Fail a payout that funded nothing, and answer `why` with it.
    fn fail(&self, store: &mut PayoutStore, mut rec: Record, why: Reply) -> Result<Record, Reply> {
        rec.state = State::Failed;
        rec.detail = Some(detail_of(&why));
        self.record(store, &mut rec);
        Err(with_payout(why, &rec))
    }

    fn halted(store: &PayoutStore) -> Result<(), Reply> {
        match &store.file().halted {
            Some(h) => Err(Reply::fail(409, "halted", format!("{h}; review it, then resume payouts in the settings"))),
            None => Ok(()),
        }
    }

    // ------------------------------------------------------------------ status and settings

    pub async fn status(&self) -> Result<Value, Reply> {
        let store = self.store()?;
        let f = store.file();
        let now = now_ms();
        let (funds, mut blocked) = match self.funds(&store).await {
            Ok(x) => (Some(x), Value::Null),
            Err(r) => (None, json!({"error": r.body["error"], "detail": r.body["detail"]})),
        };
        if blocked.is_null()
            && let Some(h) = &f.halted
        {
            blocked = json!({"error": "halted", "detail": h});
        }
        if blocked.is_null()
            && let Some(p) = f.destination.as_ref().and_then(|d| d.problem.as_ref())
        {
            blocked = json!({"error": "destination_changed", "detail": p});
        }
        let funds = funds.map(|x| {
            let (available, lightning_blocked) = match self.max_lightning_msat(&f.settings, &x) {
                Ok(a) => (a, Value::Null),
                Err(r) => {
                    let reason =
                        if x.schedules.ln.is_none() && x.schedules.ln_problem.is_none() { "no_lightning_module" } else { "fee_unbounded" };
                    (0, json!({"error": r.body["error"], "detail": r.body["detail"], "reason": reason}))
                }
            };
            json!({
                "balance_sat": x.balance_msat / 1000,
                "obligations_sat": x.obligations.total().div_ceil(1000),
                "obligations": {
                    "topups_sat": x.obligations.topups_msat.div_ceil(1000),
                    "pays_sat": x.obligations.pays_msat.div_ceil(1000),
                    "fees_sat": x.obligations.fees_msat.div_ceil(1000),
                },
                "open_payouts_sat": x.open_msat.div_ceil(1000),
                "reserve_sat": x.reserve_msat / 1000,
                "before_withdrawal_fees_sat": x.spendable_msat / 1000,
                "fee_allowance_sat": x.spendable_msat.saturating_sub(available).div_ceil(1000),
                "available_sat": available / 1000,
                "fee_schedule": {
                    "mint_base_msat": x.schedules.mint.base_msat,
                    "mint_ppm": x.schedules.mint.ppm,
                    "max_tx_items": fees::max_items(),
                },
                "lightning_blocked": lightning_blocked,
            })
        });
        Ok(json!({
            "ok": true,
            "settings": f.settings.to_json(),
            "ecash_step_sat": ecash_step_sat(self.backend.min_note_msat()),
            "destination": f.destination.as_ref().map(Destination::to_json),
            "pending_destination": f.pending.as_ref().filter(|p| p.expires_at_ms > now).map(PendingDestination::to_json),
            "funds": funds,
            "blocked": blocked,
            "next_auto_at_ms": f.next_auto_at_ms,
            "open": store.open_records().next().map(Record::summary_json),
            "recent": store.recent(RECENT).into_iter().map(Record::summary_json).collect::<Vec<_>>(),
        }))
    }

    /// Save `s`; `resume` clears a halt.
    pub fn update_settings(&self, s: Settings, resume: bool) -> Result<(), Reply> {
        s.check().map_err(|e| Reply::fail(400, "bad_settings", e))?;
        let mut store = self.store()?;
        let now = now_ms();
        store
            .commit(|f| {
                f.settings = s;
                // A shorter interval takes effect now, not after the old one.
                f.next_auto_at_ms = f.next_auto_at_ms.min(now.saturating_add(s.interval_ms()));
                if resume {
                    f.halted = None;
                }
            })
            .map_err(store_failed)
    }

    // ------------------------------------------------------------------ the destination

    pub async fn propose_destination(&self, address: &str) -> Result<PendingDestination, Reply> {
        let addr = LnAddress::parse(address).map_err(|e| Reply::fail(400, "bad_address", e))?;
        let p = lnurl::pay_params(self.fetch, &addr).await.map_err(|e| e.reply())?;
        let pending = PendingDestination {
            dest: Destination {
                address: addr.to_text(),
                callback_host: p.callback_host,
                min_sendable_msat: p.min_msat,
                max_sendable_msat: p.max_msat,
                description: p.description,
                confirmed_at_ms: 0,
                problem: None,
            },
            token: hex::encode(rand::random::<[u8; 32]>()),
            expires_at_ms: now_ms().saturating_add(PENDING_TTL_MS),
        };
        let mut store = self.store()?;
        store.commit(|f| f.pending = Some(pending.clone())).map_err(store_failed)?;
        Ok(pending)
    }

    pub fn confirm_destination(&self, token: &str) -> Result<(), Reply> {
        let mut store = self.store()?;
        let Some(p) = store.file().pending.clone() else {
            return Err(Reply::fail(409, "no_pending_destination", "resolve a Lightning address first"));
        };
        let now = now_ms();
        if p.expires_at_ms <= now {
            return Err(Reply::fail(410, "pending_expired", "the resolved address expired; resolve it again"));
        }
        if !crate::pos::token_eq(token.as_bytes(), p.token.as_bytes()) {
            return Err(Reply::fail(409, "confirm_token_mismatch", "this is not the address that was resolved last"));
        }
        let mut d = p.dest;
        d.confirmed_at_ms = now;
        d.problem = None;
        store
            .commit(|f| {
                f.destination = Some(d);
                f.pending = None;
                if f.next_auto_at_ms <= now {
                    f.next_auto_at_ms = now.saturating_add(f.settings.interval_ms());
                }
            })
            .map_err(store_failed)
    }

    pub fn remove_destination(&self) -> Result<(), Reply> {
        let mut store = self.store()?;
        store
            .commit(|f| {
                f.destination = None;
                f.pending = None;
            })
            .map_err(store_failed)
    }

    fn mark_destination(&self, store: &mut PayoutStore, problem: String) {
        let r = store.commit(|f| {
            if let Some(d) = f.destination.as_mut() {
                d.problem = Some(problem);
            }
        });
        if let Err(e) = r {
            tracing::warn!("the payout destination's problem was not saved: {e:#}");
        }
    }

    // ------------------------------------------------------------------ Lightning

    /// A replay of `request_id`: its payout, driven on if it is open.
    async fn replay(
        &self,
        store: &mut PayoutStore,
        had: Record,
        kind: Kind,
        amount_msat: Option<u64>,
        wait: Duration,
    ) -> Result<Record, Reply> {
        if had.kind != kind || amount_msat.is_some_and(|a| a != had.amount_msat) {
            return Err(with_payout(Reply::fail(409, "request_conflict", "this request_id was used for another payout"), &had));
        }
        if had.state.needs_drive() { self.drive(store, had, wait).await } else { Ok(had) }
    }

    /// One Lightning payout. Manual requests bind the recipient the merchant reviewed under the payout lock.
    pub async fn lightning(&self, manual: Option<ManualLightning>, amount_sat: Option<u64>, wait: Duration) -> Result<Record, Reply> {
        let mut store = self.store()?;
        let origin = if manual.is_some() { Origin::Manual } else { Origin::Auto };
        let amount_req = match amount_sat {
            Some(0) => return Err(Reply::fail(400, "bad_amount", "amount_sat must be positive")),
            Some(a) => Some(a.checked_mul(1000).ok_or_else(|| Reply::fail(400, "bad_amount", "amount_sat is too large"))?),
            None => None,
        };
        if let Some(request) = &manual
            && let Some(had) = store.by_request(&request.request_id).cloned()
        {
            if had.address.as_ref().is_some_and(|address| address != &request.expected_destination)
                || had.destination_confirmed_at_ms.is_some_and(|stamp| stamp != request.expected_confirmed_at_ms)
            {
                return Err(with_payout(
                    Reply::fail(409, "request_conflict", "this request_id was used for a different destination"),
                    &had,
                ));
            }
            return self.replay(&mut store, had, Kind::Lightning, amount_req, wait).await;
        }
        Self::halted(&store)?;
        let settings = store.file().settings;
        let allowed = match origin {
            Origin::Manual => settings.mode.manual(),
            Origin::Auto => settings.mode.auto(),
        };
        if !allowed {
            return Err(Reply::fail(409, "mode_disallows", format!("payout mode is {}", settings.mode.name())));
        }
        if let Some(open) = store.open_records().next() {
            return Err(in_progress(open));
        }
        let dest = match store.file().destination.clone() {
            Some(d) if d.confirmed_at_ms > 0 => d,
            _ => return Err(Reply::fail(409, "no_destination", "confirm a Lightning address first")),
        };
        if manual.as_ref().is_some_and(|r| r.expected_destination != dest.address || r.expected_confirmed_at_ms != dest.confirmed_at_ms) {
            return Err(Reply::fail(409, "destination_changed", "refresh and review the current Lightning destination before sending"));
        }
        if let Some(p) = &dest.problem {
            return Err(Reply::fail(409, "destination_changed", format!("{p}; confirm the address again")));
        }
        let Some(currency) = self.backend.currency().await else {
            return Err(Reply::fail(503, "lightning_unavailable", "this federation has no Lightning module the till can use"));
        };
        // A first look, to decide the amount; the final one is made under the guard (`fund`).
        let funds = self.funds(&store).await?;
        // A fee schedule the bound does not hold for refuses here, with its reason.
        let max = self.max_lightning_msat(&settings, &funds)?;
        let amount_msat = amount_req.unwrap_or_else(|| max.min(dest.max_sendable_msat / 1000 * 1000));
        if amount_msat == 0 || self.lightning_cost(&settings, &funds, amount_msat)? > funds.spendable_msat {
            let mut r = Reply::fail(409, "insufficient_available", format!("{} sat can be paid out now", max / 1000));
            r.body["available_sat"] = json!(max / 1000);
            return Err(r);
        }
        if amount_msat < dest.min_sendable_msat {
            return Err(Reply::fail(
                400,
                "below_min_sendable",
                format!("the address takes at least {} sat", dest.min_sendable_msat.div_ceil(1000)),
            ));
        }
        if amount_msat > dest.max_sendable_msat {
            return Err(Reply::fail(400, "above_max_sendable", format!("the address takes at most {} sat", dest.max_sendable_msat / 1000)));
        }

        // 1. The decision, recorded before anything leaves the till.
        let now = now_ms();
        let mut rec = Record::new(Kind::Lightning, origin, manual.map(|r| r.request_id), amount_msat, now);
        rec.destination_confirmed_at_ms = Some(dest.confirmed_at_ms);
        rec.address = Some(dest.address.clone());
        rec.fee_cap_msat = settings.fee_cap_msat(amount_msat);
        rec.fee_allowance_msat = funds.ln_fee_bound(amount_msat, amount_msat.saturating_add(rec.fee_cap_msat))?;
        store.insert(rec.clone()).map_err(store_failed)?;
        if origin == Origin::Auto {
            let retry = now.saturating_add(RETRY_MS.min(settings.interval_ms()));
            if let Err(e) = store.commit(|f| f.next_auto_at_ms = retry) {
                tracing::warn!("the next automatic payout time was not saved: {e:#}");
            }
        }

        // 2. The invoice.
        let addr = match LnAddress::parse(&dest.address) {
            Ok(a) => a,
            Err(e) => return self.fail(&mut store, rec, Reply::fail(409, "destination_changed", e)),
        };
        let params = match lnurl::pay_params(self.fetch, &addr).await {
            Ok(p) => p,
            Err(e) => {
                if matches!(e, LnurlError::Identity(_) | LnurlError::Fetch(FetchError::Unsafe(_))) {
                    self.mark_destination(&mut store, format!("the address no longer resolves as confirmed: {e}"));
                }
                return self.fail(&mut store, rec, e.reply());
            }
        };
        if params.callback_host != dest.callback_host {
            let p = format!("the address now calls back on {}, not {} as confirmed", params.callback_host, dest.callback_host);
            self.mark_destination(&mut store, p.clone());
            return self.fail(&mut store, rec, Reply::fail(409, "destination_changed", p));
        }
        if amount_msat < params.min_msat || amount_msat > params.max_msat {
            let r = Reply::fail(400, "outside_sendable", format!("the address now takes {} to {} msat", params.min_msat, params.max_msat));
            return self.fail(&mut store, rec, r);
        }
        let pr = match lnurl::request_invoice(self.fetch, &params, amount_msat).await {
            Ok(pr) => pr,
            Err(e) => return self.fail(&mut store, rec, e.reply()),
        };
        let expect =
            Expect { amount_msat, currency, description_hash: params.metadata_hash, now_ms: now_ms(), min_expiry_secs: MIN_EXPIRY_SECS };
        let checked = match lnurl::check_invoice(&pr, &expect) {
            Ok(c) => c,
            Err(d) => return self.fail(&mut store, rec, Reply::fail(502, "invoice_rejected", d)),
        };
        if store.hash_seen(&checked.payment_hash, &rec.id) {
            let r = Reply::fail(502, "invoice_rejected", "the service answered an invoice an earlier payout already used");
            return self.fail(&mut store, rec, r);
        }

        // 3. The invoice saved before anything is funded.
        rec.bolt11 = Some(checked.bolt11);
        rec.payment_hash = Some(checked.payment_hash);
        rec.payee = Some(checked.payee);
        rec.state = State::Invoiced;
        self.save(&mut store, &mut rec)?;
        self.fund(&mut store, rec, wait).await
    }

    /// Steps 4–5 for an `invoiced` payout.
    async fn fund(&self, store: &mut PayoutStore, mut rec: Record, wait: Duration) -> Result<Record, Reply> {
        let Some(bolt11) = rec.bolt11.clone() else {
            return self.fail(store, rec, Reply::fail(500, "internal", "no invoice saved"));
        };
        let guard = match self.exclusive().await {
            Ok(g) => g,
            Err(r) => return self.fail(store, rec, r),
        };
        // The final snapshot: the store as saved, the obligations, the balance.
        match self.store() {
            Ok(fresh) => *store = fresh,
            Err(r) => return self.fail(store, rec, r),
        }
        let other = store.open_records().find(|r| r.id != rec.id).map(in_progress);
        if let Some(r) = other {
            return self.fail(store, rec, r);
        }
        let funds = match self.funds(store).await {
            Ok(f) => f,
            Err(r) => return self.fail(store, rec, r),
        };
        // The gateway's own fee for this invoice now; the wallet refuses to fund above the cap.
        let gateway = match self.backend.quote_gateway_fee(bolt11.clone()).await {
            Ok(f) => f,
            Err(e) => {
                return self.fail(
                    store,
                    rec,
                    Reply::fail(503, "lightning_unavailable", format!("no gateway can pay the invoice now: {e:#}")),
                );
            }
        };
        if gateway > rec.fee_cap_msat {
            let r = Reply::fail(502, "fee_too_high", format!("the gateway asks {gateway} msat; the cap is {} msat", rec.fee_cap_msat));
            return self.fail(store, rec, r);
        }
        // This payout's own hold is among the open ones.
        let held = store.get(&rec.id).map(Record::held_msat).unwrap_or(0);
        // The federation's fees: the bound at this balance over every contract the gateway's fee
        // can make (up to the cap), checked against the client's own quote at the gateway's fee
        // now. A schedule the bound does not hold for, or a quote above it, funds nothing.
        let contract_hi = rec.amount_msat.saturating_add(rec.fee_cap_msat);
        rec.fee_allowance_msat = match funds.ln_fee_bound(rec.amount_msat, contract_hi) {
            Ok(b) => b,
            Err(r) => return self.fail(store, rec, r),
        };
        match self.backend.ln_send_fee_quote(rec.amount_msat.saturating_add(gateway)).await {
            Ok(q) if q <= rec.fee_allowance_msat => {}
            Ok(q) => {
                let r = Reply::fail(
                    409,
                    "fee_unbounded",
                    format!(
                        "the client quotes {q} msat in federation fees, above the {} msat bound; nothing was paid",
                        rec.fee_allowance_msat
                    ),
                );
                return self.fail(store, rec, r);
            }
            Err(e) => {
                let r =
                    Reply::fail(503, "fee_unknown", format!("the client gave no fee quote for this payment, so nothing was paid: {e:#}"));
                return self.fail(store, rec, r);
            }
        }
        let cost = rec.amount_msat.saturating_add(rec.fee_cap_msat).saturating_add(rec.fee_allowance_msat);
        if cost > funds.spendable_msat.saturating_add(held) {
            let r = Reply::fail(409, "insufficient_available", "the funds moved while the invoice was fetched; nothing was paid");
            return self.fail(store, rec, r);
        }
        // Saved before the wallet is asked to pay: a crash from here on is settled from the
        // wallet's own record of this invoice.
        rec.state = State::Funding;
        self.save(store, &mut rec)?;
        let outcome = self.backend.pay(bolt11, rec.fee_cap_msat, FUND_WAIT.min(wait)).await;
        self.settle_pay(&mut rec, outcome.map(Some));
        self.record(store, &mut rec);
        // Funded or not, the debit is in the balance and the record holds it: follow the rest
        // without the guard.
        drop(guard);
        if rec.state == State::Funding
            && wait > FUND_WAIT
            && let Some(op) = rec.operation_id.clone()
        {
            let o = self.backend.resolve_pay(op, wait - FUND_WAIT).await;
            self.settle_pay(&mut rec, o.map(Some));
            self.record(store, &mut rec);
        }
        self.after_pay(store, &mut rec).await;
        match rec.state {
            State::Failed => {
                let d = rec.detail.clone().unwrap_or_default();
                Err(with_payout(Reply::fail(502, "payment_failed", d), &rec))
            }
            _ => Ok(rec),
        }
    }

    /// Once paid: the next automatic payout, and the federation fees the wallet's record names
    /// against what was held back for them. An overrun halts new payouts.
    async fn after_pay(&self, store: &mut PayoutStore, rec: &mut Record) {
        if rec.state != State::Paid {
            return;
        }
        if rec.origin == Origin::Auto {
            let next = now_ms().saturating_add(store.file().settings.interval_ms());
            if let Err(e) = store.commit(|f| f.next_auto_at_ms = next) {
                tracing::warn!("the next automatic payout time was not saved: {e:#}");
            }
        }
        let Some(op) = rec.operation_id.clone() else { return };
        match self.backend.operation_fees(op).await {
            Ok(Some(fed)) => {
                rec.federation_fee_msat = Some(fed);
                self.record(store, rec);
                let spent = rec.fee_paid_msat.unwrap_or(0).saturating_add(fed);
                let held = rec.fee_cap_msat.saturating_add(rec.fee_allowance_msat);
                if spent > held {
                    let h = format!("payout {} paid {spent} msat in fees, more than the {held} msat held back for them", rec.id);
                    tracing::error!("{h}");
                    if let Err(e) = store.commit(|f| f.halted = Some(h)) {
                        tracing::warn!("the payout halt was not saved: {e:#}");
                    }
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("payout {}: its federation fees could not be read: {e:#}", rec.id),
        }
    }

    /// Apply the wallet's answer about a payout's invoice. `Ok(None)`: it never funded one.
    fn settle_pay(&self, rec: &mut Record, outcome: anyhow::Result<Option<WalletPay>>) {
        match outcome {
            Ok(None) => {
                rec.state = State::Failed;
                rec.detail = Some("stopped before the wallet funded the payment; nothing was paid".into());
            }
            Ok(Some(WalletPay::Paid { fee_msat, operation_id, .. })) => {
                rec.state = State::Paid;
                rec.fee_paid_msat = Some(fee_msat);
                rec.operation_id = Some(operation_id);
                rec.detail = None;
            }
            Ok(Some(WalletPay::Failed { reason, operation_id, .. })) => {
                rec.state = State::Failed;
                rec.operation_id = operation_id.or(rec.operation_id.take());
                rec.detail = Some(format!("not paid; nothing left the wallet: {reason}"));
            }
            Ok(Some(WalletPay::Unknown { reason, operation_id })) => {
                rec.state = State::Funding;
                rec.operation_id = Some(operation_id);
                rec.detail = Some(format!("not settled yet: {reason}"));
            }
            Err(e) => {
                // Could not be asked: nothing is assumed. The payout stays open.
                if rec.state == State::Invoiced {
                    rec.state = State::Funding;
                }
                rec.detail = Some(format!("not settled yet: {e:#}"));
            }
        }
    }

    /// Drive a payout the worker drives to an outcome from the wallet's records.
    pub async fn drive(&self, store: &mut PayoutStore, mut rec: Record, wait: Duration) -> Result<Record, Reply> {
        match rec.state {
            State::Resolving => {
                rec.state = State::Failed;
                rec.detail = Some("stopped before an invoice was saved; nothing was paid".into());
                self.record(store, &mut rec);
            }
            State::Invoiced | State::Funding => match rec.bolt11.clone() {
                Some(b) => {
                    let outcome = self.backend.pay_status(b, wait).await;
                    self.settle_pay(&mut rec, outcome);
                    self.record(store, &mut rec);
                    self.after_pay(store, &mut rec).await;
                }
                None => {
                    rec.state = State::Failed;
                    rec.detail = Some("no invoice was saved; nothing was paid".into());
                    self.record(store, &mut rec);
                }
            },
            State::Exporting => {
                self.resolve_exports(store, vec![rec.clone()]).await;
                rec = store.get(&rec.id).cloned().unwrap_or(rec);
            }
            State::Reclaiming => return self.follow_reclaim(store, rec, wait, None).await,
            _ => {}
        }
        Ok(rec)
    }

    // ------------------------------------------------------------------ eCash

    /// Export eCash of this federation worth `amount_sat`. Open to the merchant in every mode: the
    /// mode governs Lightning payouts only.
    pub async fn ecash(&self, request_id: String, amount_sat: u64, wait: Duration) -> Result<Record, Reply> {
        let mut store = self.store()?;
        let amount_msat = match amount_sat.checked_mul(1000) {
            Some(a) if a > 0 => a,
            _ => return Err(Reply::fail(400, "bad_amount", "amount_sat must be positive")),
        };
        if let Some(had) = store.by_request(&request_id).cloned() {
            return self.replay(&mut store, had, Kind::Ecash, Some(amount_msat), wait).await;
        }
        Self::halted(&store)?;
        let min = self.backend.min_note_msat().max(1);
        if amount_msat % min != 0 {
            let mut r = Reply::fail(
                400,
                "bad_amount",
                format!("Choose a multiple of {} sats for eCash from this federation.", ecash_step_sat(min)),
            );
            r.body["min_note_sat"] = json!(ecash_step_sat(min));
            return Err(r);
        }
        let guard = self.exclusive().await?;
        // The final snapshot, under the guard.
        let mut store = self.store()?;
        if let Some(open) = store.open_records().next() {
            return Err(in_progress(open));
        }
        let funds = self.funds(&store).await?;
        // The mint's own quote of this export's fees: without one, nothing is exported.
        let quote = self.backend.export_fee_quote(amount_msat).await.map_err(|e| {
            Reply::fail(503, "fee_unknown", format!("the mint gave no fee quote for this export, so nothing was exported: {e:#}"))
        })?;
        // The quote is exact for the inventory now; notes credited meanwhile can change the
        // layout, so the larger of it and the bound of one mint transaction is held back.
        let quote = quote.max(fees::mint_fee_bound(&funds.schedules, funds.balance_msat, 0));
        if amount_msat.saturating_add(quote) > funds.spendable_msat {
            let avail = funds.spendable_msat.saturating_sub(quote) / min * min / 1000;
            let mut r = Reply::fail(409, "insufficient_available", format!("{avail} sat can be paid out as eCash now"));
            r.body["available_sat"] = json!(avail);
            return Err(r);
        }
        let mut rec = Record::new(Kind::Ecash, Origin::Manual, Some(request_id), amount_msat, now_ms());
        rec.fee_allowance_msat = quote;
        // Saved, with the tag the spend will carry, before the wallet spends.
        store.insert(rec.clone()).map_err(store_failed)?;
        let r = match self.backend.export(amount_msat, rec.export_tag()).await {
            Ok((notes, face)) => {
                rec.notes = Some(notes);
                rec.state = State::Exported;
                if face != amount_msat {
                    rec.detail = Some(format!("the wallet handed out {face} msat for {amount_msat} msat"));
                    rec.amount_msat = face;
                }
                self.record(&mut store, &mut rec);
                Ok(rec)
            }
            Err(e) => {
                tracing::warn!("payout {}: the export answered an error; settling it from the wallet's log: {e:#}", rec.id);
                self.resolve_exports(&mut store, vec![rec.clone()]).await;
                let rec = store.get(&rec.id).cloned().unwrap_or(rec);
                match rec.state {
                    State::Failed => Err(with_payout(Reply::fail(502, "export_failed", format!("{e:#}")), &rec)),
                    _ => Ok(rec),
                }
            }
        };
        drop(guard);
        r
    }

    /// Settle `recs` (eCash, `exporting`, or `failed` as never exported) from the wallet's log.
    async fn resolve_exports(&self, store: &mut PayoutStore, recs: Vec<Record>) {
        if recs.is_empty() {
            return;
        }
        let tags = recs.iter().map(Record::export_tag).collect();
        match self.backend.find_tagged(tags).await {
            Ok(found) => {
                for mut rec in recs {
                    match found.get(&rec.export_tag()) {
                        Some(Tagged::Export { notes }) => {
                            rec.notes = Some(notes.clone());
                            rec.state = State::Exported;
                            rec.detail = Some("found in the wallet's log after an interruption".into());
                            self.record(store, &mut rec);
                        }
                        _ if rec.state == State::Exporting => {
                            rec.state = State::Failed;
                            rec.detail = Some(NOT_EXPORTED.into());
                            self.record(store, &mut rec);
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                // Fail closed: the payouts stay open, holding their amounts and blocking new ones.
                for mut rec in recs.into_iter().filter(|r| r.state == State::Exporting) {
                    rec.detail = Some(format!("the wallet's log could not be read; this export stays open: {e:#}"));
                    self.record(store, &mut rec);
                }
            }
        }
    }

    /// Reissue an exported payout's notes back into the wallet. Ends as the wallet's reissue
    /// operation ends; until then it is `reclaiming`, and the notes are kept.
    pub async fn reclaim(&self, id: &str, wait: Duration) -> Result<Record, Reply> {
        let mut store = self.store()?;
        let mut rec = store.get(id).cloned().ok_or_else(|| Reply::fail(404, "unknown_payout", "no such payout"))?;
        if rec.state == State::Reclaiming {
            return self.follow_reclaim(&mut store, rec, wait, None).await;
        }
        let Some(notes) = rec.notes.clone().filter(|_| rec.kind == Kind::Ecash && rec.state == State::Exported) else {
            return Err(with_payout(Reply::fail(409, "not_exported", "only exported eCash can be reclaimed"), &rec));
        };
        // The reissue is a transaction the mint balances: it may consolidate the wallet's notes and
        // pay their fees. It starts under the guard, as every debit does; busy, nothing changes.
        let guard = self.exclusive().await?;
        // Saved before the wallet reissues, so a crash is settled from the reissue's tag.
        rec.state = State::Reclaiming;
        rec.reclaim_operation_id = None;
        rec.detail = None;
        self.save(&mut store, &mut rec)?;
        let started = self.backend.reclaim_start(notes, rec.reclaim_tag()).await;
        drop(guard);
        let missing = match started {
            Ok(ReclaimStart::Started(op)) => {
                rec.reclaim_operation_id = Some(op);
                self.record(&mut store, &mut rec);
                None
            }
            Ok(ReclaimStart::AlreadyReissued) => Some((
                409,
                "reissued_elsewhere",
                "this wallet already reissued these notes outside this payout; the balance shows whether they came back".to_owned(),
            )),
            Err(e) => Some((502, "reclaim_failed", format!("the reissue did not start: {e:#}; the notes are kept, try again"))),
        };
        self.follow_reclaim(&mut store, rec, wait, missing).await
    }

    /// Settle a `reclaiming` payout from the wallet's reissue: its operation, found by the tag when
    /// it was not saved. `missing`: the answer when the wallet has no reissue by this payout.
    async fn follow_reclaim(
        &self,
        store: &mut PayoutStore,
        mut rec: Record,
        wait: Duration,
        missing: Option<(u16, &str, String)>,
    ) -> Result<Record, Reply> {
        if rec.reclaim_operation_id.is_none() {
            match self.backend.find_tagged(vec![rec.reclaim_tag()]).await {
                Ok(found) => match found.get(&rec.reclaim_tag()) {
                    Some(Tagged::Reclaim { operation_id }) => {
                        rec.reclaim_operation_id = Some(operation_id.clone());
                        self.record(store, &mut rec);
                    }
                    _ => {
                        let (status, code, detail) =
                            missing.unwrap_or((502, "reclaim_failed", "the reissue never started; the notes are kept, try again".into()));
                        rec.state = State::Exported;
                        rec.detail = Some(detail.clone());
                        self.record(store, &mut rec);
                        return Err(with_payout(Reply::fail(status, code, detail), &rec));
                    }
                },
                Err(e) => {
                    rec.detail = Some(format!("the wallet's log could not be read; the reclaim stays open: {e:#}"));
                    self.record(store, &mut rec);
                    return Ok(rec);
                }
            }
        }
        let op = rec.reclaim_operation_id.clone().unwrap_or_default();
        match self.backend.reclaim_status(op, wait).await {
            Ok(ReclaimState::Done) => {
                rec.state = State::Reclaimed;
                rec.notes = None;
                rec.detail = Some("reissued back into the till's wallet".into());
                self.record(store, &mut rec);
                Ok(rec)
            }
            Ok(ReclaimState::Rejected(e)) => {
                let d = format!(
                    "the federation rejected the reissue ({e}); the notes are kept. The recipient may have redeemed them; this wallet does not reissue them again"
                );
                rec.state = State::Exported;
                rec.detail = Some(d.clone());
                self.record(store, &mut rec);
                Err(with_payout(Reply::fail(409, "reclaim_rejected", d), &rec))
            }
            Ok(ReclaimState::Pending) => {
                rec.detail = Some("the reissue is still running".into());
                self.record(store, &mut rec);
                Ok(rec)
            }
            Err(e) => {
                rec.detail = Some(format!("the reissue's state could not be read: {e:#}"));
                self.record(store, &mut rec);
                Ok(rec)
            }
        }
    }

    /// The merchant marks exported notes handed over. The notes are kept: this may be the only copy
    /// of their value, and nothing here can tell whether the recipient redeemed them.
    pub fn delivered(&self, id: &str) -> Result<Record, Reply> {
        let mut store = self.store()?;
        let mut rec = store.get(id).cloned().ok_or_else(|| Reply::fail(404, "unknown_payout", "no such payout"))?;
        if rec.kind != Kind::Ecash || rec.state != State::Exported {
            return Err(with_payout(Reply::fail(409, "not_exported", "only exported eCash can be marked delivered"), &rec));
        }
        rec.delivered_at_ms.get_or_insert(now_ms());
        self.save(&mut store, &mut rec)?;
        Ok(rec)
    }

    // ------------------------------------------------------------------ the worker

    /// One pass of the worker: settle what is open, then the automatic payout if one is due.
    pub async fn sweep(&self, wait: Duration) {
        let mut store = match self.store() {
            Ok(s) => s,
            Err(r) => {
                tracing::warn!("payouts: {}", r.body["detail"]);
                return;
            }
        };
        let now = now_ms();
        let open: Vec<Record> = store.file().records.iter().filter(|r| r.state.needs_drive()).cloned().collect();
        let mut exports = Vec::new();
        for rec in open {
            if rec.state == State::Exporting {
                exports.push(rec);
            } else {
                let _ = self.drive(&mut store, rec, wait).await;
            }
        }
        exports.extend(
            store
                .file()
                .records
                .iter()
                .filter(|r| {
                    r.kind == Kind::Ecash
                        && r.state == State::Failed
                        && r.detail.as_deref() == Some(NOT_EXPORTED)
                        && now.saturating_sub(r.created_at_ms) < LATE_EXPORT_MS
                })
                .cloned(),
        );
        self.resolve_exports(&mut store, exports).await;

        let f = store.file();
        let due = f.settings.mode.auto()
            && f.halted.is_none()
            && f.destination.as_ref().is_some_and(|d| d.confirmed_at_ms > 0 && d.problem.is_none())
            && now >= f.next_auto_at_ms
            && store.open_records().next().is_none();
        if !due {
            return;
        }
        let (settings, dest) = (f.settings, f.destination.clone().expect("checked"));
        let Ok(funds) = self.funds(&store).await else { return };
        let amount = match self.max_lightning_msat(&settings, &funds) {
            Ok(m) => m.min(dest.max_sendable_msat / 1000 * 1000),
            Err(r) => {
                tracing::warn!("automatic payout: {} ({})", r.body["error"], r.body["detail"]);
                return;
            }
        };
        if amount < settings.threshold_sat.saturating_mul(1000) || amount < dest.min_sendable_msat {
            return;
        }
        match self.lightning(None, Some(amount / 1000), wait).await {
            Ok(r) => tracing::info!("automatic payout {}: {}", r.id, r.state.name()),
            Err(r) => tracing::warn!("automatic payout: {} ({})", r.body["error"], r.body["detail"]),
        }
    }
}
