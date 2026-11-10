//! Offline charges (docs/OFFLINE-PROTOCOL.md § 4.4–4.6, OFFLINE-PLAN.md § 4): when the registry
//! or the federation does not answer, a till on ops still takes a 3.0 card. The card proves it is
//! one the registry certified (`GET_CERT`, `ATTEST`), the till checks each note's federation
//! signature itself, the card signs (each slot once, ever), and the signed transaction waits in a
//! durable queue (`<data_dir>/offline_queue.json`) until the till is online again.
//!
//! Then the sender reports each queued charge to the registry (`POST /cards/:id/offline-charges`),
//! writes it into the card's file as an open, signed charge, and lets the till's ordinary recovery
//! (`resolve_pending`) send it to the federation, collect the change and the till's notes, and post
//! `/outcome`. A charge whose notes the registry already knew spent (`409 double_spend`), or that
//! the federation rejects, is a double spend: the sale stays, marked, and is counted.
//!
//! Write order: the queue entry with every secret (blinding keys, the till notes' keys) is saved
//! (`signing`) before the card is told of change (`SET_CHANGE`) or signs; the signed bytes are saved
//! (`queued`) before the point of sale is answered. Nothing the card signed is ever lost: an entry
//! left `signing` by a crash is finished at the card's next offline tap from the card's own answer
//! (a SPENT slot repeats its stored signature for the same hash).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use fedimint_card_pos_core::charge::{Env, Pin, TillOuts, mint_decoders};
use serde_json::{Value, json};

use crate::glue::{CardWallet, on_card};
use crate::ops::{OfflineAnswer, Ops, OpsError};
use crate::state::{NoteRec, NoteStatus, PendingTx, StateStore};

// Moved to the core the POS phones share (docs/POS-OFFLINE.md); kept at the old paths.
pub use fedimint_card_pos_core::charge::{Taken, attest_card};
pub use fedimint_card_pos_core::queue::{
    ChargeState, DEFAULT_CAP_COUNT, DEFAULT_CAP_MSAT, OfflineCharge, OfflineConfig, OfflineError, OfflineInput, PoolRef, QUEUE_FILE, Queue,
    QueueFile, Stats,
};

/// How long the registry and the federation get to answer before the till takes a card offline.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a probe's answer is trusted.
pub const PROBE_TTL: Duration = Duration::from_secs(10);
/// How often the background sender looks for queued charges.
pub const SEND_EVERY: Duration = Duration::from_secs(30);
/// How long the sender waits on the federation for one charge.
const SEND_TIMEOUT: Duration = Duration::from_secs(20);

/// Whether the registry and the federation answer, probed with [`PROBE_TIMEOUT`] and kept for
/// [`PROBE_TTL`]. The registry's probe is `GET /registry/cert-key`, which also refreshes the
/// certificate key the till keeps.
#[derive(Default)]
pub struct Reach {
    last: Mutex<Option<(Instant, Option<bool>, bool)>>,
}

impl Reach {
    /// `(registry, federation)`; the registry is `None` for a till with no registry.
    pub async fn probe(&self, ops: Option<&Ops>, wallet: &crate::wallet::Wallet, dir: &Path) -> (Option<bool>, bool) {
        if let Some((at, r, f)) = *self.last.lock().unwrap() {
            if at.elapsed() < PROBE_TTL {
                return (r, f);
            }
        }
        let reg = async {
            let o = ops?;
            Some(match o.client.cert_key(PROBE_TIMEOUT).await {
                Ok(key) => {
                    if let Some(k) = key {
                        if let Err(e) = crate::claim::write_cert_key(dir, &k) {
                            tracing::warn!("keeping the registry's certificate key: {e:#}");
                        }
                    }
                    true
                }
                Err(_) => false,
            })
        };
        let fed = async {
            matches!(tokio::time::timeout(PROBE_TIMEOUT, wallet.client.api().session_count()).await, Ok(Ok(_)))
        };
        let (r, f) = tokio::join!(reg, fed);
        *self.last.lock().unwrap() = Some((Instant::now(), r, f));
        (r, f)
    }

    /// Forget the last probe and record the registry down (a call to it just went unanswered).
    pub fn registry_down(&self) {
        let mut g = self.last.lock().unwrap();
        let fed = g.map_or(true, |(_, _, f)| f);
        *g = Some((Instant::now(), Some(false), fed));
    }

    /// Forget the last probe.
    pub fn reset(&self) {
        *self.last.lock().unwrap() = None;
    }
}

/// Everything a till needs for offline charges, its own and its phones'.
pub struct Offline {
    pub cfg: OfflineConfig,
    pub reach: Reach,
    pub queue: Arc<Mutex<Queue>>,
    pub dir: PathBuf,
    /// The phones' offline kits and their pool entries (`crate::pool`). Locked after `queue`,
    /// never before it.
    pub pool: Mutex<crate::pool::PoolLedger>,
    pub kits: crate::pool::KitConfig,
}

impl Offline {
    pub fn open(data_dir: &Path, cfg: OfflineConfig) -> Result<Arc<Self>> {
        Self::open_with(data_dir, cfg, crate::pool::KitConfig::default())
    }

    /// [`Offline::open`] with the phones' kit lifetimes. A ledger a crash left behind the queue is
    /// brought up to it.
    pub fn open_with(data_dir: &Path, cfg: OfflineConfig, kits: crate::pool::KitConfig) -> Result<Arc<Self>> {
        let queue = Queue::open(data_dir)?;
        let mut pool = crate::pool::PoolLedger::open(data_dir)?;
        if let Err(e) = crate::pool::reconcile(&queue, &mut pool) {
            // Safe to run on: an entry a queued charge paid to is refused to any other charge.
            tracing::warn!("offline pool: {e:#}");
        }
        Ok(Arc::new(Self {
            cfg,
            reach: Reach::default(),
            queue: Arc::new(Mutex::new(queue)),
            dir: data_dir.to_owned(),
            pool: Mutex::new(pool),
            kits,
        }))
    }

    pub fn stats(&self) -> Stats {
        self.queue.lock().unwrap().stats()
    }

    /// `/v1/status`'s `offline` object. `surplus_allowed`: the federation takes a surplus
    /// (`Wallet::surplus_allowed`), so offline charges carry a fee margin.
    pub fn status_json(&self, surplus_allowed: bool) -> Value {
        let s = self.stats();
        let last_margin = self.queue.lock().unwrap().file.charges.last().map(|c| c.fee_margin_msat);
        json!({
            "fee_margin": surplus_allowed && !self.cfg.fee_headroom.is_off(),
            "fee_headroom_base_msat": self.cfg.fee_headroom.base_msat,
            "fee_headroom_ppm": self.cfg.fee_headroom.ppm,
            "last_fee_margin_msat": last_margin,
            "queued": s.queued,
            "queued_msat": s.queued_msat,
            "double_spent": s.double_spent,
            "refused": s.refused,
            "rejected": s.rejected,
            "cap_msat": self.cfg.cap_msat,
            "cap_count": self.cfg.cap_count,
        })
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Take `price` from the card in `cw.card` with no registry or federation (§ 4.4), as the core's
/// [`fedimint_card_pos_core::charge::charge`] does on a phone: the till's own outputs, the PIN of
/// the job (else `CARD_PIN`), and change only into KEYED slots whose key this till's copy of the
/// card has never seen carry a note. On success the signed transaction is in the queue, its notes
/// in the spent list; the caller records the sale and answers.
pub async fn charge(cw: &mut CardWallet, off: &Offline, ops: &Ops, price: u64, client_ref: Option<&str>) -> Result<Taken> {
    let cert_key = crate::claim::read_cert_key(&off.dir)?;
    let decoders = mint_decoders(cw.wallet.mint_id);
    // A KEYED slot whose key the till's copy of the card knows to have carried a note is not a
    // slot for change: a load not yet written to the card, or a key whose note died (a rejected
    // online charge's change slot), which the registry refuses as change until it is renewed.
    let keyed_ok = |short_id: &str, slots: &[u8]| -> Result<Vec<u8>> {
        let local = StateStore::open(ops.card_path(short_id))?;
        Ok(slots.iter().copied().filter(|s| local.state.notes_on_current_key(*s).next().is_none()).collect())
    };
    let pin = match cw.pin.clone() {
        Some(p) => Pin::Verify(p),
        None => Pin::OpenWith(fedimint_card_host_card::pin_from_env()),
    };
    let env = Env {
        mint_id: cw.wallet.mint_id,
        tbs_pks: &cw.wallet.mint_cfg.tbs_pks,
        fee: &cw.wallet.mint_cfg.fee_consensus,
        surplus_allowed: cw.wallet.surplus_allowed(),
        decoders: &decoders,
        cert_key: cert_key.as_deref(),
        cfg: off.cfg,
        header: cw.headers.as_ref().and_then(|h| h.latest()),
        pin,
        till_outs: TillOuts::Fresh,
        keyed_ok: Some(&keyed_ok),
        now_ms: now_ms(),
        expires_at_ms: None,
    };
    let card = cw.card.as_mut();
    on_card(|| fedimint_card_pos_core::charge::charge(card, &off.queue, &env, price, client_ref))
}

// ------------------------------------------------------------------ the sender

/// What one pass of the sender did.
#[derive(Debug, Clone, Default)]
pub struct Sent {
    /// Reported to the registry (now open there, signed).
    pub reported: Vec<String>,
    /// Settled: the federation took it, the outputs are collected, the outcome is queued or sent.
    pub settled: Vec<String>,
    pub double_spent: Vec<String>,
    pub refused: Vec<String>,
    /// Reported, then refused by the federation (`ChargeState::Rejected`).
    pub rejected: Vec<String>,
    pub errors: Vec<String>,
}

/// Report every queued charge (only those of `short_id`, when given) to the registry and write
/// it into its card's file. Stops at the first unanswered call: the registry is down again.
pub async fn report(off: &Offline, ops: &Ops, short_id: Option<&str>, sent: &mut Sent) {
    let queued: Vec<OfflineCharge> = off
        .queue
        .lock()
        .unwrap()
        .file
        .charges
        .iter()
        .filter(|c| c.state == ChargeState::Queued && short_id.is_none_or(|s| s == c.short_id))
        .cloned()
        .collect();
    for c in queued {
        match ops.client.offline_charge(&c.short_id, &c.report_body()).await {
            Ok(OfflineAnswer::Recorded(_)) => {
                // Into the card's file first, then the queue: a crash between them repeats the
                // report, which the registry answers as before, and the write is idempotent.
                if let Err(e) = write_into_card_file(ops, &c) {
                    sent.errors.push(format!("{}: {e:#}", c.txid));
                    continue;
                }
                match off.queue.lock().unwrap().set(&c.txid, |x| x.state = ChargeState::Reported) {
                    Ok(()) => sent.reported.push(c.txid.clone()),
                    Err(e) => sent.errors.push(format!("{}: {e:#}", c.txid)),
                }
            }
            Ok(OfflineAnswer::DoubleSpend(conflicts)) => {
                tracing::warn!("offline charge {} on card {} is a double spend: {conflicts}", c.txid, c.short_id);
                let r = off.queue.lock().unwrap().set(&c.txid, |x| {
                    x.state = ChargeState::DoubleSpent;
                    x.detail = Some(format!(
                        "the registry: inputs already spent {conflicts}; the card's change slots are released at its next online tap"
                    ));
                });
                match r {
                    Ok(()) => sent.double_spent.push(c.txid.clone()),
                    Err(e) => sent.errors.push(format!("{}: {e:#}", c.txid)),
                }
            }
            Err(OpsError::Unreachable(m)) => {
                sent.errors.push(format!("{}: {m}", c.txid));
                off.reach.registry_down();
                return;
            }
            Err(e) => {
                tracing::warn!("offline charge {} refused by the registry: {e}", c.txid);
                let r = off.queue.lock().unwrap().set(&c.txid, |x| {
                    x.state = ChargeState::Refused;
                    x.detail = Some(e.to_string());
                });
                match r {
                    Ok(()) => sent.refused.push(c.txid.clone()),
                    Err(e) => sent.errors.push(format!("{}: {e:#}", c.txid)),
                }
            }
        }
    }
}

/// The reported charge in its card's file, as an open charge whose signed bytes the registry has:
/// the inputs, the change (pending, with blinding keys), the till's outputs, and the `PendingTx`.
/// Idempotent.
fn write_into_card_file(ops: &Ops, c: &OfflineCharge) -> Result<()> {
    let mut store = StateStore::open(ops.card_path(&c.short_id))?;
    let st = &mut store.state;
    for i in &c.inputs {
        if !st.notes.iter().any(|n| n.nonce == i.nonce) {
            st.notes.push(NoteRec {
                slot: i.slot,
                amount_msat: i.amount_msat,
                nonce: i.nonce.clone(),
                signature: Some(i.signature.clone()),
                status: NoteStatus::Live,
                blinding_key: None,
                txid: None,
                out_idx: None,
            });
        }
    }
    for ch in &c.change {
        if !st.notes.iter().any(|n| n.nonce == ch.nonce) {
            st.notes.push(NoteRec {
                slot: ch.slot,
                amount_msat: ch.amount_msat,
                nonce: ch.nonce.clone(),
                signature: None,
                status: NoteStatus::Pending,
                blinding_key: Some(ch.blinding_key.clone()),
                txid: Some(c.txid.clone()),
                out_idx: Some(ch.out_idx),
            });
        }
    }
    for h in &c.host {
        if !st.host_pending.iter().any(|x| x.secret_key == h.secret_key) {
            st.host_pending.push(h.clone());
        }
    }
    if !st.pending_txs.iter().any(|p| p.txid == c.txid) {
        st.pending_txs.push(PendingTx {
            txid: c.txid.clone(),
            tx_hex: c.signed_hex.clone(),
            // The registry has the signed bytes: any till may send them, so never abandon unsent.
            submitted: true,
            accepted: false,
            ops_unconfirmed: false,
            inputs: c.inputs.iter().map(|i| i.nonce.clone()).collect(),
            client_ref: c.client_ref.clone(),
            price_msat: c.price_msat,
            ts_unix_ms: c.ts_unix_ms,
            overpay_msat: c.overpay_msat,
            fees_msat: c.fees_msat,
            spent: c.inputs.iter().map(|i| (i.slot, i.amount_msat)).collect(),
            change: c.change.iter().map(|ch| (ch.slot, ch.amount_msat)).collect(),
        });
    }
    store.save()
}

/// Settle every reported charge: load its card from the registry, let `resolve_pending` send it
/// to the federation and collect its outputs, and deliver the outcome. A charge the federation
/// rejected is a double spend. Answers per charge what became of it; the caller records sales.
pub async fn settle(cw: &mut CardWallet, off: &Offline, ops: &Ops, sent: &mut Sent) {
    let reported: Vec<OfflineCharge> =
        off.queue.lock().unwrap().file.charges.iter().filter(|c| c.state == ChargeState::Reported).cloned().collect();
    let mut cards: Vec<String> = reported.iter().map(|c| c.short_id.clone()).collect();
    cards.sort();
    cards.dedup();
    for id in cards {
        let base = match ops.attach_id(cw, &id).await {
            Ok(b) => b,
            Err(e) => {
                sent.errors.push(format!("card {id}: {e:#}"));
                if crate::ops::unanswered(&e) {
                    off.reach.registry_down();
                    return;
                }
                continue;
            }
        };
        let abandoned_before = cw.abandoned.len();
        if let Err(e) = cw.resolve_pending(SEND_TIMEOUT).await {
            sent.errors.push(format!("card {id}: {e:#}"));
        }
        let abandoned: Vec<String> = cw.abandoned[abandoned_before..].to_vec();
        let still: Vec<String> = cw.store.state.pending_txs.iter().map(|p| p.txid.clone()).collect();
        Ops::finish(cw, base).await;
        for c in reported.iter().filter(|c| c.short_id == id) {
            if still.contains(&c.txid) {
                continue;
            }
            let r = if abandoned.contains(&c.txid) {
                sent.rejected.push(c.txid.clone());
                off.queue.lock().unwrap().set(&c.txid, |x| {
                    x.state = ChargeState::Rejected;
                    x.detail = Some(
                        "the federation rejected it: an input spent elsewhere, or a fee it no longer covers; the card's notes and \
                         change slots are released at its next online tap"
                            .into(),
                    );
                })
            } else {
                sent.settled.push(c.txid.clone());
                off.queue.lock().unwrap().commit(|f| f.charges.retain(|x| x.txid != c.txid))
            };
            if let Err(e) = r {
                sent.errors.push(format!("{}: {e:#}", c.txid));
            }
        }
    }
}
