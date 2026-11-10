//! The offline charge (docs/OFFLINE-PROTOCOL.md § 4.4, docs/POS-OFFLINE.md): one function for a
//! till whose registry or federation does not answer and for a POS phone with no network. The
//! card proves it is one the registry certified (`GET_CERT`, `ATTEST`), each note's federation
//! signature is checked here, the card signs (each slot once, ever), and the signed transaction
//! waits in the durable queue until the till sends it.
//!
//! Write order: the queue entry with every secret is saved (`signing`) before the card is told of
//! change (`SET_CHANGE`) or signs, and the signed bytes (`queued`) before the caller is answered.
//! Nothing the card signed is ever lost: an entry a pulled card left `signing` is finished at its
//! next tap from the card's own answers (a SPENT slot repeats its signature for the same hash),
//! and the tap that left it is answered `undecided`, never `no_card`, so the app keeps the sale.
//!
//! What differs between a till and a phone is in [`Env`]: the till makes its own outputs
//! ([`TillOuts::Fresh`]) and knows from its copy of the card which KEYED slots once carried a note
//! (`keyed_ok`); a phone takes the till's outputs from its offline kit's pool
//! ([`TillOuts::Pool`]), whose secrets stay on the till, and has an expiry. The card driver is
//! blocking: run this off any thread that must not wait on the card.

use std::str::FromStr as _;
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow, bail, ensure};
use fedimint_card_host_card::v3::{HEADER_LEN, SW_OVER_LIMIT, header_time};
use fedimint_card_host_card::{Card, Certificate, Opening, PIN_MAX, PIN_MIN, SlotState, StatusWord, open_pin_session_with, status_word};
use fedimint_core::core::{IntoDynInstance as _, ModuleInstanceId};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::CommonModuleInit as _;
use fedimint_core::module::registry::{ModuleDecoderRegistry, ModuleRegistry};
use fedimint_core::secp256k1::hashes::Hash as _;
use fedimint_core::secp256k1::rand::{self, Rng};
use fedimint_core::secp256k1::{Keypair, Message, PublicKey, SECP256K1, XOnlyPublicKey};
use fedimint_core::transaction::{Transaction, TransactionSignature};
use fedimint_core::{Amount, Tiered, TransactionId};
use fedimint_mint_common::config::FeeConsensus;
use fedimint_mint_common::{BlindNonce, MintCommonInit, MintInput, MintOutput, Note};
use tbs::{AggregatePublicKey, BlindedMessage, BlindingKey};

use crate::files::{HostKind, HostPending, LoadNote};
use crate::plan::{self, ALL_TIERS, MAX_CARD_NOTES, PlanError, Tiers};
use crate::queue::{ChargeState, OfflineCharge, OfflineConfig, OfflineError, OfflineInput, PoolRef, Queue, Stats};
use crate::{blind, nonce_for, short_id_of};

/// Largest price accepted (2^40 msat, about 11 BTC), as the till API.
pub const MAX_PRICE_MSAT: u64 = 1 << 40;
/// Longest `client_ref`, in ASCII characters.
pub const MAX_CLIENT_REF: usize = 128;

/// How the holder's PIN opens the card's session.
#[derive(Clone)]
pub enum Pin {
    /// The holder's PIN, typed at the till or the phone: only verified.
    Verify(Vec<u8>),
    /// A till's own PIN (`CARD_PIN`, the CLI and the tests): set first on a card with none.
    OpenWith(Vec<u8>),
}

/// A till output a phone may use: a pool entry of its offline kit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolEntry {
    pub amount_msat: u64,
    pub blinded: BlindedMessage,
    /// `blinded`, hex: how a charge names the entry.
    pub blinded_hex: String,
}

impl PoolEntry {
    pub fn new(amount_msat: u64, blinded: BlindedMessage) -> Self {
        Self { amount_msat, blinded, blinded_hex: blinded.consensus_encode_to_hex() }
    }
}

/// Where the till's outputs come from.
pub enum TillOuts<'a> {
    /// A till's own: a fresh key and blinding key per note, kept in the entry's `host`.
    Fresh,
    /// A phone's offline kit's pool, every entry it was given (the queue knows which are used).
    Pool(&'a [PoolEntry]),
}

/// Given a card's short id and its KEYED slots, the ones change may go to.
pub type KeyedFilter<'a> = &'a dyn Fn(&str, &[u8]) -> Result<Vec<u8>>;

/// What an offline charge works with besides the card and the queue.
pub struct Env<'a> {
    pub mint_id: ModuleInstanceId,
    /// The federation's key per note tier.
    pub tbs_pks: &'a Tiered<AggregatePublicKey>,
    pub fee: &'a FeeConsensus,
    /// The federation takes a surplus (core consensus 2.1 or later): the charge keeps a fee margin.
    pub surplus_allowed: bool,
    /// Decodes a queued transaction: [`mint_decoders`].
    pub decoders: &'a ModuleDecoderRegistry,
    /// The registry's certificate key, 65 bytes uncompressed, hex. `None`: no card can be checked.
    pub cert_key: Option<&'a str>,
    /// The caps on what waits to settle, and the fee headroom.
    pub cfg: OfflineConfig,
    /// The newest block header known, shown to a card whose clock is older.
    pub header: Option<[u8; HEADER_LEN]>,
    pub pin: Pin,
    pub till_outs: TillOuts<'a>,
    /// A till's: change may not go to a KEYED slot whose key once carried a note (a load not yet
    /// written to the card, or a dead change key the registry refuses). `None`: any KEYED slot.
    pub keyed_ok: Option<KeyedFilter<'a>>,
    pub now_ms: u64,
    /// A phone's offline kit expires: no charge from then on, by this clock or the card's.
    pub expires_at_ms: Option<u64>,
}

/// A decoder registry with only the mint module, at `mint_id`: all an offline charge holds.
pub fn mint_decoders(mint_id: ModuleInstanceId) -> ModuleDecoderRegistry {
    ModuleRegistry::from_iter([(mint_id, fedimint_mint_common::KIND, MintCommonInit::decoder())])
}

/// What a charge answers: the queued charge, and whether it was taken before (a repeat of the
/// same `client_ref`, or one a pulled card left signing and this tap finished).
#[derive(Debug, Clone)]
pub struct Taken {
    pub charge: OfflineCharge,
    pub replayed: bool,
}

fn lock(q: &Mutex<Queue>) -> MutexGuard<'_, Queue> {
    // A panic never leaves the queue half-changed (`Queue::commit`), so a poisoned lock is safe.
    q.lock().unwrap_or_else(|e| e.into_inner())
}

/// Take `price` from `card` offline: the card, its notes and its signatures, with nothing but
/// `env`. On success the signed transaction is in `queue` and its notes in the spent list. The
/// order is chosen so a charge that cannot succeed spends no PIN try:
///
/// 1. the price, `client_ref` and PIN's shape;
/// 2. a charge already queued under this `client_ref` is that charge (`conflict` for another price);
/// 3. a phone's kit expiry, then the count cap (not for a retap of a sale this device holds
///    `signing`: finishing that charge adds none);
/// 4. a 3.0 card, its certificate and attestation, its clock (and the kit's expiry by it), the PIN;
/// 5. charges this card left `signing` are finished: a repeat of one is that charge, and another
///    card for one is `undecided`; one that can never be finished is `abandoned` and frees its
///    `client_ref` and the notes the card never signed for it;
/// 6. the card's notes, the plan, the value cap, the card's day, then `signing`, `SET_CHANGE` and
///    `SIGN`, and `queued`. A card taken away once it signed an input leaves the charge `signing`,
///    and the tap is answered `undecided` (step 5 finishes it at the card's next tap), not `no_card`.
pub fn charge(card: &mut dyn Card, queue: &Mutex<Queue>, env: &Env, price: u64, client_ref: Option<&str>) -> Result<Taken> {
    check_request(price, client_ref, &env.pin)?;
    if let Some(r) = client_ref {
        let q = lock(queue);
        if let Some(c) = q.file.charges.iter().find(|c| c.client_ref.as_deref() == Some(r) && c.price_msat != price) {
            bail!(OfflineError::Conflict(format!("client_ref {r:?} was used for a sale of {} msat, not {price} msat", c.price_msat)));
        }
        if let Some(c) = q.file.charges.iter().find(|c| c.client_ref.as_deref() == Some(r) && c.state != ChargeState::Signing) {
            return Ok(Taken { charge: c.clone(), replayed: true });
        }
    }
    if let Some(exp) = env.expires_at_ms {
        if env.now_ms >= exp {
            bail!(OfflineError::KitExpired(format!(
                "the offline kit expired at {exp} (now {}); the phone needs a new one from the till",
                env.now_ms
            )));
        }
    }
    {
        let q = lock(queue);
        let s = q.stats();
        // A retap of a sale this device holds `signing` adds no charge: it finishes that one
        // (counted already), or abandons it and takes the sale again, the count as it was.
        if !holds_signing(&q, client_ref) && s.queued + 1 > env.cfg.cap_count {
            bail!(cap_reached(&s, &env.cfg));
        }
    }

    let v = card.applet_version()?;
    if !v.sign_once() {
        bail!(OfflineError::Unsupported(format!(
            "applet {v} can sign a note twice, so it is taken only online; the registry or the federation is unreachable"
        )));
    }
    let Some(cert_key) = env.cert_key else {
        bail!(OfflineError::NotReady(
            "no certificate key from the registry yet (a till fetches one while online, a phone gets it in its offline kit), so no card can be checked offline".into()
        ));
    };
    let slot0 = attest_card(card, cert_key)?;
    let short_id = short_id_of(&slot0);
    // The clock first: a header a day later starts the card's new day.
    let card_now = if env.header.is_some() || env.expires_at_ms.is_some() { sync_clock(card, env.header) } else { None };
    if let (Some(exp), Some(now)) = (env.expires_at_ms, card_now) {
        if u64::from(now).saturating_mul(1_000) >= exp {
            bail!(OfflineError::KitExpired(format!(
                "the offline kit expired at {exp}, and the card's clock says {now} s; the phone needs a new kit from the till"
            )));
        }
    }
    // The PIN, before anything on the card changes: a wrong one leaves the card as it was.
    open_pin(card, &env.pin)?;

    // A retap of a sale whose card was taken away mid-SIGN is that sale, finished from the card's
    // stored signatures, never a second transaction. Another card for it waits for the first.
    let resumed = match resume_signing(card, queue, env, &short_id) {
        Ok(r) => r,
        Err(e) => return Err(undecided_if_held(e, queue, client_ref)),
    };
    if let Some(r) = client_ref {
        if let Some(c) = resumed.into_iter().find(|c| c.client_ref.as_deref() == Some(r)) {
            return Ok(Taken { charge: c, replayed: true });
        }
        if let Some(c) = lock(queue).file.charges.iter().find(|c| c.state == ChargeState::Signing && c.client_ref.as_deref() == Some(r)) {
            bail!(OfflineError::Undecided(format!(
                "an earlier attempt with client_ref {r:?} is waiting for card {} (it was taken away while it signed); tap that card again, nothing was charged again",
                c.short_id
            )));
        }
    }
    take(card, queue, env, price, client_ref, &short_id)
}

fn check_request(price: u64, client_ref: Option<&str>, pin: &Pin) -> Result<()> {
    let bad = |m: String| -> Result<()> { Err(OfflineError::BadRequest(m).into()) };
    if price == 0 || price > MAX_PRICE_MSAT {
        return bad(format!("price_msat must be between 1 and {MAX_PRICE_MSAT}"));
    }
    if client_ref.is_some_and(|c| c.is_empty() || c.len() > MAX_CLIENT_REF || !c.is_ascii()) {
        return bad(format!("client_ref must be 1 to {MAX_CLIENT_REF} ASCII characters"));
    }
    let shaped = match pin {
        Pin::Verify(p) => (PIN_MIN..=PIN_MAX).contains(&p.len()) && p.iter().all(u8::is_ascii_digit),
        Pin::OpenWith(p) => (PIN_MIN..=PIN_MAX).contains(&p.len()),
    };
    if !shaped {
        return bad(format!("pin must be {PIN_MIN} to {PIN_MAX} digits"));
    }
    Ok(())
}

fn cap_reached(s: &Stats, cfg: &OfflineConfig) -> OfflineError {
    OfflineError::Cap(format!(
        "{} offline charge(s) worth {} msat wait to settle here; the caps are {} and {} msat",
        s.queued, s.queued_msat, cfg.cap_count, cfg.cap_msat
    ))
}

/// The card is one the registry certified (§ 2): its certificate under `cert_key_hex`, for this
/// card's slot-0 key, and a fresh attestation under the certified key. Answers slot 0's key.
pub fn attest_card(card: &mut dyn Card, cert_key_hex: &str) -> Result<XOnlyPublicKey> {
    let not_genuine = |m: String| anyhow::Error::new(OfflineError::NotGenuine(m));
    let cert_key = hex::decode(cert_key_hex).context("the kept certificate key is not hex")?;
    let slot0 = card.get_pub(0).context("reading the card's slot 0 key")?;
    let raw = match card.get_cert() {
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
    let der = match card.attest(&challenge) {
        Ok(d) => d,
        Err(e) if status_word(&e).is_some() => return Err(not_genuine(format!("the card would not attest ({e:#})"))),
        Err(e) => return Err(e),
    };
    cert.verify_attest(&challenge, &der).map_err(|e| not_genuine(format!("{e:#}")))?;
    Ok(slot0)
}

/// Show a card whose clock is older the header (`SET_HEADER`). Answers the card's clock, or
/// `None` when the card did not say; never fails the charge.
fn sync_clock(card: &mut dyn Card, header: Option<[u8; HEADER_LEN]>) -> Option<u32> {
    let r: Result<u32> = (|| {
        let st = card.get_state()?;
        match header {
            Some(h) if st.now < header_time(&h) => card.set_header(&h),
            _ => Ok(st.now),
        }
    })();
    r.map_err(|e| tracing::warn!("the card's clock: {e:#}")).ok()
}

fn open_pin(card: &mut dyn Card, pin: &Pin) -> Result<()> {
    match pin {
        Pin::Verify(p) => card.verify_pin(p).context("verifying the holder's PIN"),
        Pin::OpenWith(p) => open_pin_session_with(card, p).context("opening the PIN session"),
    }
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

/// The pool entries no charge has taken, and the tiers they hold.
fn usable_pool(q: &Queue, outs: &TillOuts) -> (Vec<PoolEntry>, Tiers) {
    let TillOuts::Pool(all) = outs else { return (vec![], ALL_TIERS) };
    let used: std::collections::HashSet<&str> = q
        .file
        .charges
        .iter()
        .flat_map(|c| c.pool.iter().map(|p| p.blinded.as_str()))
        .chain(q.file.pool_used.iter().map(String::as_str))
        .collect();
    let free: Vec<PoolEntry> = all.iter().filter(|e| !used.contains(e.blinded_hex.as_str())).cloned().collect();
    let tiers = free.iter().filter(|e| e.amount_msat.is_power_of_two()).fold(0, |t, e| t | e.amount_msat);
    (free, tiers)
}

/// Whether `q` holds a charge for `client_ref` that a card left `signing`.
fn holds_signing(q: &Queue, client_ref: Option<&str>) -> bool {
    client_ref.is_some_and(|r| q.file.charges.iter().any(|c| c.state == ChargeState::Signing && c.client_ref.as_deref() == Some(r)))
}

/// A failure that leaves this sale's charge `signing` here (the card left, or stopped answering,
/// once it had signed an input) is `undecided`, not the failure itself: only that card's next tap
/// on this device finishes the charge, so the app keeps the sale and its `client_ref` and asks for
/// the same card. Answered as `no_card`, which says nothing was left behind, the apps rang the
/// sale again under a new `client_ref`, and the retap paid it twice: the charge it finished and a
/// new one (ENG-842's third review). Any other failure is itself.
fn undecided_if_held(e: anyhow::Error, queue: &Mutex<Queue>, client_ref: Option<&str>) -> anyhow::Error {
    let held = client_ref.and_then(|r| {
        lock(queue).file.charges.iter().find(|c| c.state == ChargeState::Signing && c.client_ref.as_deref() == Some(r)).map(|c| c.short_id.clone())
    });
    match held {
        Some(short_id) => anyhow::Error::new(OfflineError::Undecided(format!(
            "card {short_id} was taken away while it signed this sale ({e:#}); only its next tap here can finish the sale, so tap that card again: nothing was charged twice"
        ))),
        None => e,
    }
}

/// Step 6: plan from the card's notes and take the charge.
fn take(card: &mut dyn Card, queue: &Mutex<Queue>, env: &Env, price: u64, client_ref: Option<&str>, short_id: &str) -> Result<Taken> {
    // The card's notes, each checked against the federation's key for its amount.
    let st = card.get_state()?;
    let spent: Vec<String> = lock(queue).file.spent.clone();
    let mut notes = vec![];
    let mut keyed = vec![];
    let mut invalid = 0usize;
    for (i, s) in st.states.iter().enumerate() {
        let slot = i as u8;
        match s {
            SlotState::Note => {
                let info = card.get_slot(slot)?;
                let Some(key) = info.xonly else { continue };
                let n = CardNote { slot, key, amount_msat: info.amount_msat, sig48: info.sig48 };
                if spent.contains(&n.nonce_hex()) {
                    tracing::warn!("slot {slot}: a note already taken offline here; skipped");
                    continue;
                }
                let Ok(pk) = env.tbs_pks.tier(&Amount::from_msats(n.amount_msat)) else {
                    tracing::warn!("slot {slot}: {} msat is not a denomination; skipped", n.amount_msat);
                    invalid += 1;
                    continue;
                };
                let good = sig_of(&n.sig48).is_ok_and(|signature| Note { nonce: nonce_for(key), signature }.verify(*pk));
                if !good {
                    tracing::warn!("slot {slot}: the note's federation signature does not verify; skipped");
                    invalid += 1;
                    continue;
                }
                notes.push(n);
            }
            SlotState::Keyed => keyed.push(slot),
            _ => {}
        }
    }
    let keyed = match env.keyed_ok {
        Some(f) => f(short_id, &keyed)?,
        None => keyed,
    };

    // The plan, from the card's own notes, change into KEYED slots, the till's notes in the tiers
    // its outputs can have. On a federation that takes a surplus, with a fee margin
    // (OFFLINE-PLAN.md § 10): the charge is sent days later, perhaps under higher fees. Older
    // federations refuse any surplus: an exact balance.
    let fee_cfg = env.fee.clone();
    let fee = move |a: u64| fee_cfg.fee(Amount::from_msats(a)).msats;
    let amounts: Vec<u64> = notes.iter().map(|n| n.amount_msat).collect();
    let min_denom = env.fee.min_economical_denomination().msats;
    let (pool, tiers) = usable_pool(&lock(queue), &env.till_outs);
    let planned = if env.surplus_allowed {
        plan::solve_with_margin_in(&amounts, price, keyed.len(), &fee, min_denom, env.cfg.fee_headroom, tiers)
    } else {
        plan::solve_in(&amounts, price, keyed.len(), &fee, min_denom, tiers)
    };
    let plan = match planned {
        Ok(p) => p,
        Err(e) => return Err(no_plan(e, &amounts, price, keyed.len(), &fee, min_denom, tiers, invalid)),
    };
    let inputs: Vec<CardNote> = plan.inputs.iter().map(|&i| notes[i].clone()).collect();
    let till_msat: u64 = plan.till.iter().sum();

    // The value cap (the count was checked before the card), then the card's day.
    {
        let s = lock(queue).stats();
        if s.queued_msat + till_msat > env.cfg.cap_msat {
            bail!(cap_reached(&s, &env.cfg));
        }
    }
    let input_amounts: Vec<u64> = inputs.iter().map(|n| n.amount_msat).collect();
    if !st.limit_allows(&input_amounts) {
        return Err(anyhow::Error::new(StatusWord(SW_OVER_LIMIT)).context("the card's daily offline limit does not take this charge; nothing was signed"));
    }

    // Outputs, blinded as the online flow blinds them: the till's first, then the change.
    let mut outs: Vec<(u64, BlindedMessage)> = vec![];
    let mut host = vec![];
    let mut pool_refs = vec![];
    match env.till_outs {
        TillOuts::Fresh => {
            for a in &plan.till {
                let kp = Keypair::new(SECP256K1, &mut rand::thread_rng());
                let key = BlindingKey::random();
                outs.push((*a, blind(fedimint_mint_common::Nonce(kp.public_key()), key)));
                host.push((*a, kp, key));
            }
        }
        TillOuts::Pool(_) => {
            let mut free = pool;
            for a in &plan.till {
                let i = free.iter().position(|e| e.amount_msat == *a).with_context(|| format!("the pool has no {a} msat note left"))?;
                let e = free.swap_remove(i);
                pool_refs.push(PoolRef { amount_msat: *a, blinded: e.blinded_hex.clone(), out_idx: outs.len() as u64 });
                outs.push((*a, e.blinded));
            }
        }
    }
    let mut change = vec![];
    for (a, slot) in plan.change.iter().zip(keyed.iter()) {
        let info = card.get_slot(*slot)?;
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
                Ok(MintInput::new_v0(Amount::from_msats(n.amount_msat), Note { nonce: nonce_for(n.key), signature: sig_of(&n.sig48)? }).into_dyn(env.mint_id))
            })
            .collect::<Result<_>>()?,
        outputs: outs.iter().map(|(a, b)| MintOutput::new_v0(Amount::from_msats(*a), BlindNonce(*b)).into_dyn(env.mint_id)).collect(),
        nonce: rand::thread_rng().r#gen(),
        signatures: TransactionSignature::NaiveMultisig(vec![]),
    };
    let txid = tx.tx_hash();
    let n_till = plan.till.len();
    let fees_msat = inputs.iter().map(|n| fee(n.amount_msat)).sum::<u64>() + plan::fee_sum(&plan.till, &fee) + plan::fee_sum(&plan.change, &fee);
    // What the card signs pays the outputs, their fees and the margin, to the msat.
    ensure!(
        input_amounts.iter().sum::<u64>() == till_msat + plan.change.iter().sum::<u64>() + fees_msat + plan.surplus,
        "the offline plan does not balance"
    );
    let entry = OfflineCharge {
        txid: txid.to_string(),
        short_id: short_id.to_owned(),
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
        ts_unix_ms: env.now_ms,
        detail: None,
        pool: pool_refs,
        synced: false,
    };
    // Every secret on disk, and the notes in the spent list, before the card hears of it. A
    // phone keeps the latest time it has seen, so a clock set back cannot revive its kit.
    lock(queue).commit(|f| {
        f.charges.push(entry.clone());
        f.spent.extend(entry.inputs.iter().map(|i| i.nonce.clone()));
        if env.expires_at_ms.is_some() {
            f.high_water_ms = f.high_water_ms.max(env.now_ms);
        }
    })?;
    // The card keeps the change's opening, then signs. Any failure before a slot is spent leaves
    // nothing to send: the entry and its spent marks go again.
    let digest = txid.to_byte_array();
    let signed: Result<Vec<_>> = (|| {
        for (i, (slot, a, _, bk)) in change.iter().enumerate() {
            let opening = Opening {
                txid: digest,
                out_idx: (n_till + i) as u16,
                blinding_key: bk.consensus_encode_to_vec().try_into().map_err(|_| anyhow!("a blinding key is 32 bytes"))?,
            };
            card.set_change(*slot, *a, &opening).with_context(|| format!("SET_CHANGE on slot {slot}"))?;
        }
        sign_inputs(card, &inputs.iter().map(|n| (n.slot, n.key)).collect::<Vec<_>>(), digest)
    })();
    let sigs = match signed {
        Ok(s) => s,
        Err(e) => {
            settle_failed_signing(card, queue, &entry);
            return Err(undecided_if_held(e, queue, client_ref));
        }
    };
    tx.signatures = TransactionSignature::NaiveMultisig(sigs);
    // The signed bytes, durably, before the answer.
    let signed_hex = tx.consensus_encode_to_hex();
    let charge = lock(queue).commit(|f| {
        let c = f.charges.iter_mut().find(|c| c.txid == entry.txid)?;
        c.state = ChargeState::Queued;
        c.signed_hex = Some(signed_hex);
        c.signed_at = env.now_ms.max(1);
        Some(c.clone())
    })?;
    Ok(Taken { charge: charge.context("the charge left the queue while it was signed")?, replayed: false })
}

/// Why there is no plan, in the vocabulary of the till API: a pool short of a tier, a card short
/// of room for change (each with prices it pays), notes that do not check, or the planner's own.
#[allow(clippy::too_many_arguments)]
fn no_plan(e: PlanError, amounts: &[u64], price: u64, free: usize, fee: plan::FeeFn, min_denom: u64, tiers: Tiers, invalid: usize) -> anyhow::Error {
    let pays = |free: usize, tiers: Tiers| plan::solve_in(amounts, price, free, fee, min_denom, tiers).is_ok();
    if invalid == 0 && matches!(e, PlanError::NoExactPlan { .. }) && pays(MAX_CARD_NOTES, ALL_TIERS) {
        let (below, above) = plan::exact_prices_near_in(amounts, price, free, fee, min_denom, 1_000, tiers);
        let sats = |m: u64| if m % 1_000 == 0 { format!("{} sats", m / 1_000) } else { format!("{:.3} sats", m as f64 / 1_000.0) };
        let tip = match (below, above) {
            (None, None) => "try another amount".to_owned(),
            (b, a) => format!("it can pay {} exactly", [b, a].into_iter().flatten().map(sats).collect::<Vec<_>>().join(" or ")),
        };
        // The pool is short when the card's slots would do with every tier, or no number of slots
        // does with the tiers the pool has left.
        if tiers != ALL_TIERS && (pays(free, ALL_TIERS) || !pays(MAX_CARD_NOTES, tiers)) {
            return OfflineError::PoolExhausted {
                detail: format!("{} needs till notes the phone's offline kit has run out of; {tip}", sats(price)),
                price_msat: price,
                below_msat: below,
                above_msat: above,
            }
            .into();
        }
        // Change is what the card has no room for offline (a fresh keyed slot per change note,
        // and nothing offline can key one without the registry): say so, with prices it pays.
        return OfflineError::NoChangeRoom {
            detail: format!("{} needs change the card has no room for while offline ({free} free slot(s) for change); {tip}", sats(price)),
            price_msat: price,
            below_msat: below,
            above_msat: above,
        }
        .into();
    }
    if invalid > 0 {
        return OfflineError::NoteInvalid(format!(
            "{invalid} of the card's notes do not check against the federation's keys, and the rest do not pay {price} msat ({e}); nothing was signed"
        ))
        .into();
    }
    e.into()
}

/// SIGN each `(slot, key)` over `digest` and check each signature here.
fn sign_inputs(card: &mut dyn Card, keys: &[(u8, XOnlyPublicKey)], digest: [u8; 32]) -> Result<Vec<fedimint_core::secp256k1::schnorr::Signature>> {
    let msg = Message::from_digest(digest);
    let mut sigs = vec![];
    for (slot, key) in keys {
        let sig = card.sign(*slot, &digest).with_context(|| format!("card signature, slot {slot}"))?;
        SECP256K1.verify_schnorr(&sig, &msg, key).context("card signature does not verify on the host")?;
        sigs.push(sig);
    }
    Ok(sigs)
}

/// Signing failed: if no input slot is spent for this transaction, nothing can ever be sent, so
/// the entry and its spent marks go (its pool entries are never used again). If one is (or the
/// card cannot say), the entry stays `signing` for [`resume_signing`].
fn settle_failed_signing(card: &mut dyn Card, queue: &Mutex<Queue>, entry: &OfflineCharge) {
    let digest = TransactionId::from_str(&entry.txid).ok().map(|t| t.to_byte_array());
    let mut any_spent = false;
    for i in &entry.inputs {
        match card.get_slot(i.slot) {
            Ok(info) if info.state == SlotState::Spent && Some(info.spent_hash) == digest => any_spent = true,
            Ok(_) => {}
            Err(_) => any_spent = true,
        }
    }
    if any_spent {
        return;
    }
    let nonces: Vec<String> = entry.inputs.iter().map(|i| i.nonce.clone()).collect();
    let r = lock(queue).commit(|f| {
        f.charges.retain(|c| c.txid != entry.txid);
        f.spent.retain(|n| !nonces.contains(n));
        f.pool_used.extend(entry.pool.iter().map(|p| p.blinded.clone()));
    });
    if let Err(e) = r {
        tracing::warn!("offline queue: {e:#}");
    }
    if !entry.change.is_empty() {
        tracing::warn!("offline charge {} not signed; its change slots stay owed change that will never come", entry.txid);
    }
}

/// Finish the entries a pulled card left `signing`: the card answers a spent slot's stored
/// signature for the same hash, so the transaction is completed and queued. An entry none of
/// whose slots signed it is dropped. One that can never be finished, because an input slot is
/// neither spent for it nor still holding its note (spent by another transaction since, or
/// re-keyed), is [`ChargeState::Abandoned`] and the charge goes on; its inputs the card never
/// signed still hold their notes, which leave the spent list and pay again. Needs the PIN
/// session. Answers the entries finished.
fn resume_signing(card: &mut dyn Card, queue: &Mutex<Queue>, env: &Env, short_id: &str) -> Result<Vec<OfflineCharge>> {
    let left: Vec<OfflineCharge> =
        lock(queue).file.charges.iter().filter(|c| c.state == ChargeState::Signing && c.short_id == short_id).cloned().collect();
    let mut done = vec![];
    for e in left {
        let txid = TransactionId::from_str(&e.txid).map_err(|_| anyhow!("bad txid {}", e.txid))?;
        let digest = txid.to_byte_array();
        let mut keys = vec![];
        let mut spent = 0;
        let mut gone = None;
        // The inputs not signed yet: their notes are still on the card.
        let mut unsigned = vec![];
        // Every input slot is read before any is signed: a transaction that cannot be finished
        // must not spend another note.
        for i in &e.inputs {
            let key = input_key(i)?;
            let info = card.get_slot(i.slot)?;
            let ours = info.xonly == Some(key);
            match info.state {
                // Signed for this transaction: SIGN answers its stored signature again.
                SlotState::Spent if ours && info.spent_hash == digest => spent += 1,
                // Not signed yet: SIGN spends the note for this transaction.
                SlotState::Note if ours && info.amount_msat == i.amount_msat => unsigned.push(i.nonce.clone()),
                s => {
                    gone.get_or_insert_with(|| match (ours, s) {
                        (false, _) => format!("input slot {} was re-keyed", i.slot),
                        (true, SlotState::Spent) => format!("input slot {} was spent by another transaction", i.slot),
                        (true, s) => format!("input slot {} is {s:?}, not the note it held", i.slot),
                    });
                }
            }
            keys.push((i.slot, key));
        }
        if spent == 0 {
            settle_failed_signing(card, queue, &e);
            continue;
        }
        if let Some(why) = gone {
            abandon(queue, &e, &why, &unsigned)?;
            continue;
        }
        let sigs = sign_inputs(card, &keys, digest)?;
        let mut tx = Transaction::consensus_decode_hex(&e.unsigned_hex, env.decoders).map_err(|x| anyhow!("decoding a queued transaction: {x}"))?;
        tx.signatures = TransactionSignature::NaiveMultisig(sigs);
        let hex_tx = tx.consensus_encode_to_hex();
        let c = lock(queue).commit(|f| {
            let c = f.charges.iter_mut().find(|c| c.txid == e.txid)?;
            c.state = ChargeState::Queued;
            c.signed_hex = Some(hex_tx);
            c.signed_at = env.now_ms.max(1);
            Some(c.clone())
        })?;
        tracing::info!("offline charge {} finished from the card's stored signatures", e.txid);
        done.extend(c);
    }
    Ok(done)
}

/// The key of a queued input: its nonce is `02 ‖ x-only key`.
fn input_key(i: &OfflineInput) -> Result<XOnlyPublicKey> {
    PublicKey::from_str(&i.nonce).map(XOnlyPublicKey::from).map_err(|_| anyhow!("a queued input's nonce {} is not a key", i.nonce))
}

/// An entry left `signing` that can never be sent: kept for the operator as `abandoned`, its
/// `client_ref` freed (a retap of the sale is a new charge: this one was never paid), its pool
/// entries never taken again. The card's slots spent for it stay spent; its `unsigned` inputs
/// (nonces), whose notes the card still holds, leave the spent list in the same write, so they pay
/// again here.
fn abandon(queue: &Mutex<Queue>, e: &OfflineCharge, why: &str, unsigned: &[String]) -> Result<()> {
    let sale = e.client_ref.as_deref().map(|r| format!(" for client_ref {r:?}")).unwrap_or_default();
    let detail = format!(
        "{why} while this charge waited for the card's next tap, so it can never be sent; nothing was charged{sale}, and the notes the card signed for it stay spent on the card"
    );
    tracing::warn!("offline charge {} on card {} abandoned: {detail}", e.txid, e.short_id);
    lock(queue).commit(|f| {
        if let Some(c) = f.charges.iter_mut().find(|c| c.txid == e.txid) {
            c.state = ChargeState::Abandoned;
            c.client_ref = None;
            c.detail = Some(detail);
            f.pool_used.extend(std::mem::take(&mut c.pool).into_iter().map(|p| p.blinded));
            f.spent.retain(|n| !unsigned.contains(n));
        }
    })
}
