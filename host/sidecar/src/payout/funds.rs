//! What in the till's wallet belongs to someone else, so a payout (or a card issue) never takes
//! it.
//!
//! Counted conservatively: every record whose value is (or may be) in the wallet and owed to a
//! holder counts in full, with a bound on the federation fees of the spends that will pay it out
//! ([`super::fees::obligation_fee_bound`]), until its own record says the value left, or the
//! source of truth says it never arrived.
//!
//! - Every transition that lowers an obligation happens after the debit it records (a top-up is
//!   `loaded` after its load, a pay `paid` after its funding, a refund's `refund_msat` after its
//!   load), so a snapshot that reads the obligations before the balance never counts a debit's
//!   value as free. [`snapshot`] reads them in that order, and the payout takes it under the
//!   wallet's exclusive admission guard, which every debit and every new liability takes shared
//!   (`crate::wallet::SpendGuard`).
//! - Two records say "nothing here" while money may still have arrived: a top-up `expired` or
//!   `failed` by its watcher (whose timer can give up while the client's receive can still be
//!   claimed), and a pay `not_charged` whose charge later resolves as taken. They count in full
//!   unless the source of truth says otherwise, read after the balance:
//!   - a top-up is released only when the client's own receive operation ended canceled
//!     ([`crate::wallet::Wallet::ln_receive_settled`]), which is terminal and credits nothing;
//!   - a pay is released only when no charge job for its ref is in flight, no unresolved charge
//!     carries it, and no sale records it (`crate::pay::never_charged`), read in that order: a
//!     charge that paid the wallet before the balance was read is in one of them by then.
//! - The stores of a service that is not running now (no Lightning module this run, paying
//!   turned off) are read from disk. A store that cannot be read, or is of a newer version, is an
//!   error, and the caller refuses.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use super::fees::{Schedules, obligation_fee_bound};
use crate::pay::{PayRec, PayState, PayStore, Pays};
use crate::topup::{TopupRec, TopupState, TopupStore, Topups};
use crate::wallet::Wallet;

/// The `topups.json` version this build reads (`topup::TOPUPS_VERSION`).
const TOPUPS_VERSION: u32 = 1;
/// How long one top-up's receive is followed for evidence.
const RECEIVE_WAIT: Duration = Duration::from_millis(500);
/// How long one snapshot spends looking for evidence in all.
const EVIDENCE_BUDGET: Duration = Duration::from_secs(5);

/// Receives the client said ended canceled: terminal, so kept for the life of the process.
static CANCELED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Obligations {
    pub topups_msat: u64,
    pub pays_msat: u64,
    /// Bounds on the federation fees of paying them out.
    pub fees_msat: u64,
}

impl Obligations {
    pub fn total(&self) -> u64 {
        self.topups_msat.saturating_add(self.pays_msat).saturating_add(self.fees_msat)
    }
}

/// The balance and what of it is owed, read together ([`snapshot`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub balance_msat: u64,
    pub obligations: Obligations,
    pub schedules: Schedules,
}

/// One record's claim on the wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owed {
    Topup { amount_msat: u64, release: Option<String> },
    Pay { amount_msat: u64, release: Option<Box<PayRec>> },
}

/// A top-up's claim. `waiting`: it may be paid any moment (the claim runs in the client whether a
/// watcher runs or not). `failed` and `expired` count unless their receive is proven canceled
/// (`release`: its operation id).
pub fn topup_owed(t: &TopupRec) -> Option<Owed> {
    let release = match t.state {
        TopupState::Waiting | TopupState::Paid => None,
        TopupState::Failed | TopupState::Expired => Some(t.operation_id.clone()),
        TopupState::Loaded => return None,
    };
    Some(Owed::Topup { amount_msat: t.amount_msat, release })
}

/// A pay's claim. `refunding` counts everything the card paid: its `refund_msat` already counts
/// the load in flight, which has not left the wallet yet. `not_charged` counts its price unless
/// it is proven never charged (`release`).
pub fn pay_owed(p: &PayRec) -> Option<Owed> {
    let (amount_msat, release) = match p.state {
        PayState::Charging | PayState::Charged | PayState::Paying | PayState::Refunding => (p.held_msat(), None),
        PayState::PayFailed => (p.refund_owed_msat(), None),
        PayState::NotCharged => (p.price_msat, Some(Box::new(p.clone()))),
        PayState::Paid | PayState::Refunded => return None,
    };
    (amount_msat > 0).then_some(Owed::Pay { amount_msat, release })
}

#[derive(Deserialize)]
struct TopupsOnDisk {
    version: u32,
    topups: Vec<TopupRec>,
}

/// `topups.json` in `dir`; none when there is no file.
pub fn topups_on_disk(dir: &Path) -> Result<Vec<TopupRec>> {
    let path = TopupStore::path_in(dir);
    match std::fs::read(&path) {
        Ok(b) => {
            let f: TopupsOnDisk = serde_json::from_slice(&b).with_context(|| format!("parsing {}", path.display()))?;
            if f.version > TOPUPS_VERSION {
                bail!("{} has version {}, this build understands {TOPUPS_VERSION}", path.display(), f.version);
            }
            Ok(f.topups)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// `pays.json` in `dir`; none when there is no file.
pub fn pays_on_disk(dir: &Path) -> Result<Vec<PayRec>> {
    Ok(PayStore::open(PayStore::path_in(dir))?.all().to_vec())
}

/// Every claim on the wallet: of the services running now (`topups`, `pays`), and of those that
/// are not, from their files in `dir`.
pub fn owed(dir: &Path, topups: Option<&Topups>, pays: Option<&Pays>) -> Result<Vec<Owed>> {
    let mut v: Vec<Owed> = match topups {
        Some(t) => t.store.lock().map_err(|_| anyhow!("the top-up store is poisoned"))?.all().iter().filter_map(topup_owed).collect(),
        None => topups_on_disk(dir)?.iter().filter_map(topup_owed).collect(),
    };
    match pays {
        Some(p) => v.extend(p.store.lock().map_err(|_| anyhow!("the pay store is poisoned"))?.all().iter().filter_map(pay_owed)),
        None => v.extend(pays_on_disk(dir)?.iter().filter_map(pay_owed)),
    }
    Ok(v)
}

/// The total of `owed`, less what `released` proves never arrived, with each claim's fee bound
/// at `balance_msat`.
pub fn total(owed: &[Owed], s: &Schedules, balance_msat: u64, released: &dyn Fn(&Owed) -> bool) -> Result<Obligations> {
    let mut o = Obligations::default();
    for item in owed {
        if released(item) {
            continue;
        }
        let amount = match item {
            Owed::Topup { amount_msat, .. } => {
                o.topups_msat = o.topups_msat.saturating_add(*amount_msat);
                *amount_msat
            }
            Owed::Pay { amount_msat, .. } => {
                o.pays_msat = o.pays_msat.saturating_add(*amount_msat);
                *amount_msat
            }
        };
        o.fees_msat = o.fees_msat.saturating_add(obligation_fee_bound(s, balance_msat, amount).map_err(|e| anyhow!("{e}"))?);
    }
    Ok(o)
}

/// Whether the client says `operation_id`'s receive ended canceled: cached once it did.
async fn receive_canceled(w: &Wallet, operation_id: &str, deadline: tokio::time::Instant) -> bool {
    if CANCELED.lock().unwrap().contains(operation_id) {
        return true;
    }
    if tokio::time::Instant::now() >= deadline {
        return false;
    }
    match w.ln_receive_settled(operation_id, RECEIVE_WAIT).await {
        Ok(Some(false)) => {
            CANCELED.lock().unwrap().insert(operation_id.to_owned());
            true
        }
        _ => false,
    }
}

/// The wallet's balance and what of it is owed: the claims read first, then the balance, then the
/// evidence that releases a claim (`never_charged`, for a `not_charged` pay; `None` counts every
/// one), then the fee bounds at that balance. `Err` when any of it cannot be read or bounded.
pub async fn snapshot(
    w: &Wallet,
    dir: &Path,
    topups: Option<&Topups>,
    pays: Option<&Pays>,
    never_charged: Option<&(dyn Fn(&PayRec) -> bool + Send + Sync)>,
) -> Result<Snapshot> {
    let schedules = w.fee_schedules().await.map_err(|e| anyhow!("{e}"))?;
    let owed = owed(dir, topups, pays)?;
    let balance_msat = w.balance_msat().await?;
    let deadline = tokio::time::Instant::now() + EVIDENCE_BUDGET;
    let mut canceled = HashSet::new();
    for item in &owed {
        if let Owed::Topup { release: Some(op), .. } = item
            && receive_canceled(w, op, deadline).await
        {
            canceled.insert(op.clone());
        }
    }
    let released = |item: &Owed| match item {
        Owed::Topup { release: Some(op), .. } => canceled.contains(op),
        Owed::Pay { release: Some(rec), .. } => never_charged.is_some_and(|f| f(rec)),
        _ => false,
    };
    let obligations = total(&owed, &schedules, balance_msat, &released)?;
    Ok(Snapshot { balance_msat, obligations, schedules })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::payout::fees::{Linear, LnSchedule};

    pub(crate) fn topup(state: TopupState, msat: u64) -> TopupRec {
        TopupRec {
            topup_id: hex::encode(rand::random::<[u8; 8]>()),
            client_ref: None,
            requested_msat: msat,
            amount_msat: msat,
            invoice: "lnbcrt1".into(),
            operation_id: hex::encode(rand::random::<[u8; 32]>()),
            created_at_ms: 0,
            expires_at_ms: 0,
            state,
            load_txid: None,
            short_id: None,
            detail: None,
            for_card: None,
        }
    }

    pub(crate) fn pay(state: PayState, price: u64, received: Option<u64>, refunded: Option<u64>) -> PayRec {
        PayRec {
            pay_id: rand::random(),
            client_ref: "c".into(),
            bolt11: "lnbcrt1".into(),
            payment_hash: "00".into(),
            invoice_msat: price,
            fee_reserve_msat: 0,
            margin_msat: 0,
            price_msat: price,
            invoice_expires_at_ms: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
            state,
            sale_id: None,
            txid: None,
            operation_id: None,
            preimage: None,
            fee_paid_msat: None,
            detail: None,
            short_id: None,
            refund_msat: refunded,
            refund_txid: None,
            received_msat: received,
        }
    }

    fn none(_: &Owed) -> bool {
        false
    }

    fn owed_of(t: &[TopupRec], p: &[PayRec]) -> Vec<Owed> {
        t.iter().filter_map(topup_owed).chain(p.iter().filter_map(pay_owed)).collect()
    }

    #[test]
    fn holders_value_counts_until_its_record_says_it_left() {
        let t = [
            topup(TopupState::Waiting, 1_000),
            topup(TopupState::Paid, 2_000),
            topup(TopupState::Failed, 4_000),
            topup(TopupState::Loaded, 8_000),
            topup(TopupState::Expired, 16_000),
        ];
        let p = [
            pay(PayState::Charging, 100, None, None),
            pay(PayState::Charged, 100, Some(110), None),
            pay(PayState::Paying, 100, None, None),
            // The load in flight is already in refund_msat: all of it counts.
            pay(PayState::Refunding, 1_000, None, Some(600)),
            pay(PayState::PayFailed, 1_000, Some(1_050), Some(600)),
            pay(PayState::Paid, 10_000, None, None),
            pay(PayState::Refunded, 10_000, None, Some(10_000)),
            pay(PayState::NotCharged, 10_000, None, None),
        ];
        let o = total(&owed_of(&t, &p), &Schedules::zero(), 0, &none).unwrap();
        // Expired and failed top-ups and a refused pay count, however old, without evidence.
        assert_eq!(o.topups_msat, 1_000 + 2_000 + 4_000 + 16_000);
        assert_eq!(o.pays_msat, 100 + 110 + 100 + 1_000 + 450 + 10_000);
        assert_eq!(o.fees_msat, 0);
    }

    #[test]
    fn a_late_obligation_is_released_only_by_its_own_evidence() {
        let expired = topup(TopupState::Expired, 5_000);
        let failed = topup(TopupState::Failed, 7_000);
        let waiting = topup(TopupState::Waiting, 11_000);
        let refused = pay(PayState::NotCharged, 3_000, None, None);
        let charged = pay(PayState::Charged, 13_000, None, None);
        let owed = owed_of(&[expired.clone(), failed, waiting.clone()], &[refused.clone(), charged]);
        // Evidence for the expired top-up's receive and the refused pay: those two go.
        let released = |o: &Owed| match o {
            Owed::Topup { release: Some(op), .. } => *op == expired.operation_id,
            Owed::Pay { release: Some(r), .. } => r.pay_id == refused.pay_id,
            _ => false,
        };
        let o = total(&owed, &Schedules::zero(), 0, &released).unwrap();
        assert_eq!((o.topups_msat, o.pays_msat), (7_000 + 11_000, 13_000));
        // Evidence never releases a claim that is not waiting for it: a waiting top-up's own
        // operation id, or a charged pay.
        let only_releasable = |o: &Owed| !matches!(o, Owed::Topup { release: None, .. } | Owed::Pay { release: None, .. });
        let o = total(&owed_of(&[waiting], &[]), &Schedules::zero(), 0, &only_releasable).unwrap();
        assert_eq!(o.topups_msat, 11_000);
    }

    #[test]
    fn every_claim_carries_its_fee_bound_and_an_unbounded_schedule_fails_closed() {
        let s = Schedules {
            min_change_msat: 1,
            mint: Linear { base_msat: 100, ppm: 0 },
            ln: Some(LnSchedule::V1 { contract_input_msat: 0, contract_output_msat: 0 }),
            ln_problem: None,
        };
        let one = crate::payout::fees::obligation_fee_bound(&s, 1, 1).unwrap();
        let o =
            total(&owed_of(&[topup(TopupState::Paid, 10_000)], &[pay(PayState::PayFailed, 1_000, None, Some(400))]), &s, 1, &none).unwrap();
        assert_eq!(o.fees_msat, 2 * one);
        assert_eq!(o.total(), 10_000 + 600 + 2 * one);
        let bad = Schedules { ln: None, ln_problem: Some("stepped".into()), ..s };
        assert!(total(&owed_of(&[topup(TopupState::Paid, 1)], &[]), &bad, 1, &none).is_err());
        assert_eq!(total(&[], &bad, 1, &none).unwrap(), Obligations::default());
    }

    #[test]
    fn a_disabled_services_store_still_counts_and_an_unreadable_one_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(owed(dir.path(), None, None).unwrap().is_empty());
        let file = serde_json::json!({"version": 1, "topups": [topup(TopupState::Paid, 5_000), topup(TopupState::Loaded, 1)]});
        std::fs::write(TopupStore::path_in(dir.path()), serde_json::to_vec(&file).unwrap()).unwrap();
        let mut pays = PayStore::open(PayStore::path_in(dir.path())).unwrap();
        pays.insert(pay(PayState::PayFailed, 3_000, None, None)).unwrap();
        let o = total(&owed(dir.path(), None, None).unwrap(), &Schedules::zero(), 0, &none).unwrap();
        assert_eq!((o.topups_msat, o.pays_msat), (5_000, 3_000));

        std::fs::write(TopupStore::path_in(dir.path()), br#"{"version": 2, "topups": []}"#).unwrap();
        assert!(owed(dir.path(), None, None).is_err());
        std::fs::write(TopupStore::path_in(dir.path()), b"{").unwrap();
        assert!(owed(dir.path(), None, None).is_err());
        std::fs::remove_file(TopupStore::path_in(dir.path())).unwrap();
        std::fs::write(PayStore::path_in(dir.path()), b"{").unwrap();
        assert!(owed(dir.path(), None, None).is_err());
    }
}
