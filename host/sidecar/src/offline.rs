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

use anyhow::{Context, Result, anyhow, bail, ensure};
use fedimint_card_host_card::{Certificate, Opening, SlotState, status_word};
use fedimint_core::core::IntoDynInstance as _;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::registry::ModuleRegistry;
use fedimint_core::secp256k1::hashes::Hash as _;
use fedimint_core::secp256k1::rand::{self, Rng};
use fedimint_core::secp256k1::{Keypair, Message, SECP256K1, XOnlyPublicKey};
use fedimint_core::transaction::{Transaction, TransactionSignature};
use fedimint_core::{Amount, TransactionId};
use fedimint_mint_common::{BlindNonce, MintInput, MintOutput, Note};
use serde_json::{Value, json};
use tbs::BlindingKey;

use crate::glue::{CardWallet, nonce_for, on_card};
use crate::ops::{OfflineAnswer, Ops, OpsError, short_id_of};
use crate::shares::blind;
use crate::state::{HostKind, HostPending, LoadNote, NoteRec, NoteStatus, PendingTx, StateStore};

// Moved to the core the POS phones share (docs/POS-OFFLINE.md); kept at the old paths.
pub use fedimint_card_pos_core::queue::{
    ChargeState, DEFAULT_CAP_COUNT, DEFAULT_CAP_MSAT, OfflineCharge, OfflineConfig, OfflineError, OfflineInput, QUEUE_FILE, Queue, QueueFile,
    Stats,
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

/// Everything a till needs for offline charges.
pub struct Offline {
    pub cfg: OfflineConfig,
    pub reach: Reach,
    pub queue: Arc<Mutex<Queue>>,
    pub dir: PathBuf,
}

impl Offline {
    pub fn open(data_dir: &Path, cfg: OfflineConfig) -> Result<Arc<Self>> {
        Ok(Arc::new(Self { cfg, reach: Reach::default(), queue: Arc::new(Mutex::new(Queue::open(data_dir)?)), dir: data_dir.to_owned() }))
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

fn sig_of(sig48: &[u8; 48]) -> Result<tbs::Signature> {
    tbs::Signature::consensus_decode_hex(&hex::encode(sig48), &ModuleRegistry::default()).map_err(|_| anyhow!("not a signature"))
}

/// A note the card holds, checked offline.
#[derive(Debug, Clone)]
struct CardNote {
    slot: u8,
    key: XOnlyPublicKey,
    amount_msat: u64,
    sig48: [u8; 48],
}

impl CardNote {
    fn nonce_hex(&self) -> String {
        hex::encode(nonce_for(self.key).0.serialize())
    }
}

/// The card is one the registry certified (§ 2): its certificate under the kept certificate key,
/// for this card's slot-0 key, and a fresh attestation under the certified key.
pub fn attest_card(card: &mut dyn fedimint_card_host_card::Card, cert_key_hex: &str) -> Result<XOnlyPublicKey> {
    let not_genuine = |m: String| anyhow::Error::new(OfflineError::NotGenuine(m));
    let cert_key = hex::decode(cert_key_hex).context("the kept certificate key is not hex")?;
    let slot0 = on_card(|| card.get_pub(0)).context("reading the card's slot 0 key")?;
    let raw = match on_card(|| card.get_cert()) {
        Ok(c) => c,
        Err(e) if status_word(&e).is_some() => return Err(not_genuine(format!("the card has no certificate ({e:#})"))),
        Err(e) => return Err(e),
    };
    let cert = Certificate::parse(&raw).map_err(|e| not_genuine(format!("{e:#}")))?;
    cert.verify(&cert_key).map_err(|e| not_genuine(format!("{e:#}")))?;
    if cert.slot0 != slot0.serialize() {
        return Err(not_genuine("the certificate is for another card".into()));
    }
    let challenge: [u8; 32] = rand::thread_rng().r#gen();
    let der = match on_card(|| card.attest(&challenge)) {
        Ok(d) => d,
        Err(e) if status_word(&e).is_some() => return Err(not_genuine(format!("the card would not attest ({e:#})"))),
        Err(e) => return Err(e),
    };
    cert.verify_attest(&challenge, &der).map_err(|e| not_genuine(format!("{e:#}")))?;
    Ok(slot0)
}

/// What an offline charge returns to the till service.
#[derive(Debug, Clone)]
pub struct Taken {
    pub charge: OfflineCharge,
}

/// Take `price` from the card in `cw.card` with no registry or federation (§ 4.4). The holder's
/// PIN is `cw.pin`. On success the signed transaction is in the queue, its nonces in the spent
/// list; the caller records the sale and answers.
pub async fn charge(cw: &mut CardWallet, off: &Offline, ops: &Ops, price: u64, client_ref: Option<&str>) -> Result<Taken> {
    let _ = ops;
    // 1. A 3.0 card only.
    let v = on_card(|| cw.card.applet_version())?;
    if !v.sign_once() {
        bail!(OfflineError::Unsupported(format!(
            "applet {v} can sign a note twice, so this till takes it only online; the registry or the federation is unreachable"
        )));
    }
    // 2. A card the registry certified.
    let Some(cert_key) = crate::claim::read_cert_key(&off.dir)? else {
        bail!(OfflineError::NotReady(
            "this till has no certificate key from the registry yet (it fetches one while online), so it cannot check a card offline".into()
        ));
    };
    let slot0 = attest_card(cw.card.as_mut(), &cert_key)?;
    let short_id = short_id_of(&slot0);
    // The clock first: a header a day later starts the card's new day.
    cw.sync_card_clock();

    // 3. The PIN, before anything on the card changes: a wrong one leaves the card as it was.
    cw.verify_pin_session()?;
    resume_signing(cw, off, &short_id)?;

    // 4. The card's notes, each checked against the federation's key for its amount.
    let st = on_card(|| cw.card.get_state())?;
    let spent: Vec<String> = off.queue.lock().unwrap().file.spent.clone();
    let mut notes = vec![];
    let mut keyed = vec![];
    for (i, s) in st.states.iter().enumerate() {
        let slot = i as u8;
        match s {
            SlotState::Note => {
                let info = on_card(|| cw.card.get_slot(slot))?;
                let Some(key) = info.xonly else { continue };
                let n = CardNote { slot, key, amount_msat: info.amount_msat, sig48: info.sig48 };
                if spent.contains(&n.nonce_hex()) {
                    tracing::warn!("slot {slot}: a note this till already took offline; skipped");
                    continue;
                }
                let Ok(pk) = cw.wallet.mint_cfg.tbs_pks.tier(&Amount::from_msats(n.amount_msat)) else {
                    tracing::warn!("slot {slot}: {} msat is not a denomination; skipped", n.amount_msat);
                    continue;
                };
                let good = sig_of(&n.sig48).is_ok_and(|signature| Note { nonce: nonce_for(key), signature }.verify(*pk));
                if !good {
                    tracing::warn!("slot {slot}: the note's federation signature does not verify; skipped");
                    continue;
                }
                notes.push(n);
            }
            SlotState::Keyed => keyed.push(slot),
            _ => {}
        }
    }
    // A KEYED slot whose key the till's copy of the card knows to have carried a note is not a
    // slot for change: a load not yet written to the card, or a key whose note died (a rejected
    // online charge's change slot), which the registry refuses as change until it is renewed.
    let local = StateStore::open(ops.card_path(&short_id))?;
    keyed.retain(|s| local.state.notes_on_current_key(*s).next().is_none());

    // 5. Plan from the card's own notes; change into KEYED slots. On a federation that takes a
    //    surplus, with a fee margin (OFFLINE-PLAN.md § 10): the charge is sent days later, perhaps
    //    under higher fees. Older federations refuse any surplus: an exact balance.
    let fee_cfg = cw.wallet.mint_cfg.fee_consensus.clone();
    let fee = move |a: u64| fee_cfg.fee(Amount::from_msats(a)).msats;
    let amounts: Vec<u64> = notes.iter().map(|n| n.amount_msat).collect();
    let min_denom = cw.wallet.min_denom();
    let planned = if cw.wallet.surplus_allowed() {
        crate::plan::solve_with_margin(&amounts, price, keyed.len(), &fee, min_denom, off.cfg.fee_headroom)
    } else {
        crate::plan::solve(&amounts, price, keyed.len(), &fee, min_denom)
    };
    let plan = match planned {
        Ok(p) => p,
        // Change is what the card has no room for offline (a fresh keyed slot per change note,
        // and no till can key one without the registry): say so, with prices it can pay.
        Err(crate::plan::PlanError::NoExactPlan { .. })
            if crate::plan::solve(&amounts, price, crate::plan::MAX_CARD_NOTES, &fee, min_denom).is_ok() =>
        {
            let (below, above) = crate::plan::exact_prices_near(&amounts, price, keyed.len(), &fee, min_denom, 1_000);
            let sats = |m: u64| if m % 1_000 == 0 { format!("{} sats", m / 1_000) } else { format!("{:.3} sats", m as f64 / 1_000.0) };
            let tip = match (below, above) {
                (None, None) => "try another amount".to_owned(),
                (b, a) => format!("it can pay {} exactly", [b, a].into_iter().flatten().map(sats).collect::<Vec<_>>().join(" or ")),
            };
            bail!(OfflineError::NoChangeRoom {
                detail: format!(
                    "{} needs change the card has no room for while the till is offline ({} free slot(s) for change); {tip}",
                    sats(price),
                    keyed.len()
                ),
                below_msat: below,
                above_msat: above,
            });
        }
        Err(e) => return Err(e.into()),
    };
    let inputs: Vec<CardNote> = plan.inputs.iter().map(|&i| notes[i].clone()).collect();
    let till_msat: u64 = plan.till.iter().sum();

    // 6. The till's caps, then the card's day.
    {
        let s = off.stats();
        if s.queued + 1 > off.cfg.cap_count || s.queued_msat + till_msat > off.cfg.cap_msat {
            bail!(OfflineError::Cap(format!(
                "this till holds {} offline charge(s) worth {} msat unsettled; its caps are {} and {} msat",
                s.queued, s.queued_msat, off.cfg.cap_count, off.cfg.cap_msat
            )));
        }
    }
    let input_amounts: Vec<u64> = inputs.iter().map(|n| n.amount_msat).collect();
    if !st.limit_allows(&input_amounts) {
        return Err(anyhow::Error::new(fedimint_card_host_card::StatusWord(fedimint_card_host_card::v3::SW_OVER_LIMIT))
            .context("the card's daily offline limit does not take this charge; nothing was signed"));
    }

    // 7. Outputs, blinded as the online flow blinds them: the till's first, then the change.
    let mint_id = cw.wallet.mint_id;
    let mut outs: Vec<(u64, tbs::BlindedMessage)> = vec![];
    let mut host = vec![];
    for a in &plan.till {
        let kp = Keypair::new(SECP256K1, &mut rand::thread_rng());
        let key = BlindingKey::random();
        outs.push((*a, blind(fedimint_mint_common::Nonce(kp.public_key()), key)));
        host.push((*a, kp, key));
    }
    let mut change = vec![];
    for (a, slot) in plan.change.iter().zip(keyed.iter()) {
        let info = on_card(|| cw.card.get_slot(*slot))?;
        let key = info.xonly.context("a keyed slot without a key")?;
        let nonce = nonce_for(key);
        let bk = BlindingKey::random();
        change.push((*slot, *a, nonce, bk));
        outs.push((*a, blind(nonce, bk)));
    }
    ensure!(change.len() == plan.change.len(), "the planner used more slots than are keyed");
    let mut tx = Transaction {
        inputs: inputs
            .iter()
            .map(|n| -> Result<_> {
                Ok(MintInput::new_v0(Amount::from_msats(n.amount_msat), Note { nonce: nonce_for(n.key), signature: sig_of(&n.sig48)? }).into_dyn(mint_id))
            })
            .collect::<Result<_>>()?,
        outputs: outs.iter().map(|(a, b)| MintOutput::new_v0(Amount::from_msats(*a), BlindNonce(*b)).into_dyn(mint_id)).collect(),
        nonce: rand::thread_rng().r#gen(),
        signatures: TransactionSignature::NaiveMultisig(vec![]),
    };
    let txid = tx.tx_hash();
    let n_till = plan.till.len();
    let fees_msat = inputs.iter().map(|n| fee(n.amount_msat)).sum::<u64>() + crate::plan::fee_sum(&plan.till, &fee) + crate::plan::fee_sum(&plan.change, &fee);
    // What the card signs pays the outputs, their fees and the margin, to the msat.
    ensure!(
        input_amounts.iter().sum::<u64>() == till_msat + plan.change.iter().sum::<u64>() + fees_msat + plan.surplus,
        "the offline plan does not balance"
    );
    let entry = OfflineCharge {
        txid: txid.to_string(),
        short_id: short_id.clone(),
        client_ref: client_ref.map(str::to_owned),
        state: ChargeState::Signing,
        unsigned_hex: tx.consensus_encode_to_hex(),
        signed_hex: None,
        signed_at: 0,
        inputs: inputs
            .iter()
            .map(|n| OfflineInput { slot: n.slot, amount_msat: n.amount_msat, nonce: n.nonce_hex(), signature: hex::encode(n.sig48) })
            .collect(),
        change: change
            .iter()
            .enumerate()
            .map(|(i, (slot, a, nonce, bk))| LoadNote {
                slot: *slot,
                amount_msat: *a,
                nonce: hex::encode(nonce.0.serialize()),
                blinding_key: bk.consensus_encode_to_hex(),
                out_idx: (n_till + i) as u64,
            })
            .collect(),
        host: host
            .iter()
            .enumerate()
            .map(|(i, (a, kp, bk))| HostPending {
                amount_msat: *a,
                secret_key: hex::encode(kp.secret_bytes()),
                blinding_key: bk.consensus_encode_to_hex(),
                kind: HostKind::Till,
                group: txid.to_string(),
                out_idx: Some(i as u64),
            })
            .collect(),
        price_msat: price,
        till_msat,
        overpay_msat: plan.overpay,
        fees_msat,
        fee_margin_msat: plan.surplus,
        ts_unix_ms: now_ms(),
        detail: None,
    };
    // 8. Every secret on disk, and the notes in the spent list, before the card hears of it.
    {
        let mut q = off.queue.lock().unwrap();
        q.file.charges.push(entry.clone());
        q.file.spent.extend(entry.inputs.iter().map(|i| i.nonce.clone()));
        q.save()?;
    }
    // 9. The card keeps the change's opening, then signs. Any failure before a slot is spent
    //    leaves nothing to send: the entry and its spent marks go again.
    let digest = txid.to_byte_array();
    let signed: Result<Vec<_>> = (|| {
        for (i, (slot, a, _, bk)) in change.iter().enumerate() {
            let opening = Opening {
                txid: digest,
                out_idx: (n_till + i) as u16,
                blinding_key: bk.consensus_encode_to_vec().try_into().map_err(|_| anyhow!("a blinding key is 32 bytes"))?,
            };
            on_card(|| cw.card.set_change(*slot, *a, &opening)).with_context(|| format!("SET_CHANGE on slot {slot}"))?;
        }
        sign_inputs(cw, &inputs.iter().map(|n| (n.slot, n.key)).collect::<Vec<_>>(), digest)
    })();
    let sigs = match signed {
        Ok(s) => s,
        Err(e) => {
            settle_failed_signing(cw, off, &entry);
            return Err(e);
        }
    };
    tx.signatures = TransactionSignature::NaiveMultisig(sigs);
    // 10. The signed bytes, durably, before the answer.
    let signed_hex = tx.consensus_encode_to_hex();
    let signed_at = now_ms();
    off.queue.lock().unwrap().set(&entry.txid, |c| {
        c.state = ChargeState::Queued;
        c.signed_hex = Some(signed_hex.clone());
        c.signed_at = signed_at;
    })?;
    let charge = off.queue.lock().unwrap().file.charges.iter().find(|c| c.txid == entry.txid).cloned().expect("just saved");
    Ok(Taken { charge })
}

/// SIGN each `(slot, key)` over `digest` and check each signature on the host.
fn sign_inputs(cw: &mut CardWallet, keys: &[(u8, XOnlyPublicKey)], digest: [u8; 32]) -> Result<Vec<fedimint_core::secp256k1::schnorr::Signature>> {
    let msg = Message::from_digest(digest);
    let mut sigs = vec![];
    for (slot, key) in keys {
        let sig = on_card(|| cw.card.sign(*slot, &digest)).with_context(|| format!("card signature, slot {slot}"))?;
        SECP256K1.verify_schnorr(&sig, &msg, key).context("card signature does not verify on the host")?;
        sigs.push(sig);
    }
    Ok(sigs)
}

/// Signing failed: if no input slot is spent for this transaction, nothing can ever be sent, so
/// the entry and its spent marks go. If one is (or the card cannot say), the entry stays
/// `signing` for [`resume_signing`].
fn settle_failed_signing(cw: &mut CardWallet, off: &Offline, entry: &OfflineCharge) {
    let digest = TransactionId::from_str_hex(&entry.txid).map(|t| t.to_byte_array());
    let mut any_spent = false;
    for i in &entry.inputs {
        let want = digest.as_ref().ok().copied();
        match on_card(|| cw.card.get_slot(i.slot)) {
            Ok(info) if info.state == SlotState::Spent && Some(info.spent_hash) == want => any_spent = true,
            Ok(_) => {}
            Err(_) => any_spent = true,
        }
    }
    if !any_spent {
        let mut q = off.queue.lock().unwrap();
        q.file.charges.retain(|c| c.txid != entry.txid);
        let nonces: Vec<String> = entry.inputs.iter().map(|i| i.nonce.clone()).collect();
        q.file.spent.retain(|n| !nonces.contains(n));
        if let Err(e) = q.save() {
            tracing::warn!("offline queue: {e:#}");
        }
        if !entry.change.is_empty() {
            tracing::warn!("offline charge {} not signed; its change slots stay owed change that will never come", entry.txid);
        }
    }
}

/// Finish entries a crash left `signing` for this card: the card answers a spent slot's stored
/// signature for the same hash, so the transaction is completed and queued. An entry none of whose
/// slots signed it is dropped. Needs the PIN session.
fn resume_signing(cw: &mut CardWallet, off: &Offline, short_id: &str) -> Result<()> {
    let left: Vec<OfflineCharge> = off
        .queue
        .lock()
        .unwrap()
        .file
        .charges
        .iter()
        .filter(|c| c.state == ChargeState::Signing && c.short_id == short_id)
        .cloned()
        .collect();
    for e in left {
        let txid = TransactionId::from_str_hex(&e.txid)?;
        let digest = txid.to_byte_array();
        let mut keys = vec![];
        let mut spent = 0;
        for i in &e.inputs {
            let info = on_card(|| cw.card.get_slot(i.slot))?;
            if info.state == SlotState::Spent && info.spent_hash == digest {
                spent += 1;
            }
            keys.push((i.slot, info.xonly.context("an empty input slot")?));
        }
        if spent == 0 {
            settle_failed_signing(cw, off, &e);
            continue;
        }
        let sigs = sign_inputs(cw, &keys, digest)?;
        let mut tx = Transaction::consensus_decode_hex(&e.unsigned_hex, cw.wallet.client.decoders())
            .map_err(|x| anyhow!("decoding a queued transaction: {x}"))?;
        tx.signatures = TransactionSignature::NaiveMultisig(sigs);
        let hex_tx = tx.consensus_encode_to_hex();
        off.queue.lock().unwrap().set(&e.txid, |c| {
            c.state = ChargeState::Queued;
            c.signed_hex = Some(hex_tx);
            c.signed_at = now_ms();
        })?;
        tracing::info!("offline charge {} finished from the card's stored signatures", e.txid);
    }
    Ok(())
}

trait FromStrHex: Sized {
    fn from_str_hex(s: &str) -> Result<Self>;
}

impl FromStrHex for TransactionId {
    fn from_str_hex(s: &str) -> Result<Self> {
        use std::str::FromStr as _;
        TransactionId::from_str(s).map_err(|_| anyhow!("bad txid {s}"))
    }
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
                let mut q = off.queue.lock().unwrap();
                q.file.charges.retain(|x| x.txid != c.txid);
                q.save()
            };
            if let Err(e) = r {
                sent.errors.push(format!("{}: {e:#}", c.txid));
            }
        }
    }
}
