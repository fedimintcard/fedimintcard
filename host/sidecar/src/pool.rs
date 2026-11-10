//! The phones' offline kits and the charges they took (docs/POS-OFFLINE.md, docs/TILL-API.md):
//! the pool ledger `<data_dir>/offline_pool.json` (0600), the kit a paired phone fetches
//! (`GET /v1/offline/kit`), and its charges taken offline, ingested into this till's own offline
//! queue (`POST /v1/offline/charges`) as `queued`, for the sender to report and settle as the
//! till's own.
//!
//! A kit's pool holds blinded nonces only: the till keeps each one's spend key and blinding key
//! (`Issued`) until a charge that pays to it is ingested, when they move to the queue entry's
//! `host` (the ledger keeps `Used` and the txid). An entry is used once, by one phone, within its
//! kit's redeem window (expiry plus a grace); past it, its secrets are wiped (`Reclaimed`). Writes
//! go to the queue first, then the ledger: [`reconcile`] finishes a ledger that a crash left
//! behind.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use fedimint_card_pos_core::charge::mint_decoders;
use fedimint_card_pos_core::files::{HostKind, HostPending, write_private_json};
use fedimint_card_pos_core::kit::{KIT_MAX_BYTES, KIT_VERSION, Kit, PoolTier};
use fedimint_card_pos_core::plan;
use fedimint_card_pos_core::queue::{ChargeState, OfflineCharge, OfflineConfig, OfflineError, Queue};
use fedimint_card_pos_core::{blind, nonce_for};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::registry::{ModuleDecoderRegistry, ModuleRegistry};
use fedimint_core::secp256k1::hashes::Hash as _;
use fedimint_core::secp256k1::{Keypair, Message, PublicKey, SECP256K1, XOnlyPublicKey};
use fedimint_core::transaction::{Transaction, TransactionSignature};
use fedimint_core::{Amount, Tiered};
use fedimint_mint_common::config::FeeConsensus;
use fedimint_mint_common::{MintInput, MintOutput, Nonce};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tbs::{AggregatePublicKey, BlindingKey};

use crate::sales::{SaleRec, SalesStore, SlotMsat};

pub const POOL_FILE: &str = "offline_pool.json";
pub const POOL_VERSION: u32 = 1;
/// Pool entries per tier in a new kit (decision 2 of 2026-11-09): a charge takes a tier at most
/// once, and the count cap is 200.
pub const KIT_DEPTH: usize = 40;
/// A phone asking again within this long gets the kit it has, while every tier has
/// [`REDELIVER_MIN_LEFT`] entries left.
pub const REDELIVER_WITHIN_MS: u64 = 3_600_000;
pub const REDELIVER_MIN_LEFT: usize = 4;
/// Most kits a phone holds that have not expired; at the cap it gets the newest again.
pub const MAX_LIVE_KITS: usize = 3;
/// Most charges one `POST /v1/offline/charges` takes.
pub const MAX_INGEST: usize = 50;
const DAY_MS: u64 = 86_400_000;

/// How long a kit is good for, and how long after that its pool entries are still redeemed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KitConfig {
    pub ttl_ms: u64,
    pub grace_ms: u64,
}

impl Default for KitConfig {
    fn default() -> Self {
        Self { ttl_ms: 7 * DAY_MS, grace_ms: 30 * DAY_MS }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PoolFile {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub kits: Vec<KitRec>,
    #[serde(default)]
    pub entries: Vec<PoolRec>,
}

/// A kit issued to a phone. Kept after it is reclaimed (a tombstone).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitRec {
    pub kit_id: String,
    pub phone_id: String,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    /// Its entries are redeemed until then; then their secrets are wiped.
    pub redeem_until_ms: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reclaimed: bool,
}

/// One till note a kit's pool offers: its blinded nonce (hex) and what became of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolRec {
    pub blinded: String,
    pub amount_msat: u64,
    pub kit_id: String,
    pub state: EntryState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum EntryState {
    /// In a kit, its secrets here: the note's spend key and blinding key, hex.
    Issued { secret_key: String, blinding_key: String },
    /// A phone's charge that pays to it was ingested: its secrets are in that queue entry.
    Used { txid: String },
    /// Its kit's redeem window passed unused: secrets wiped.
    Reclaimed,
}

/// The ledger, saved like the queue (0600, fsync, rename, fsync of the directory); every change
/// goes through [`PoolLedger::commit`].
pub struct PoolLedger {
    path: PathBuf,
    pub file: PoolFile,
    fault: Option<usize>,
}

impl PoolLedger {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join(POOL_FILE);
        let file: PoolFile = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PoolFile { version: POOL_VERSION, ..Default::default() },
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        if file.version > POOL_VERSION {
            bail!("{} has version {}, this build understands {POOL_VERSION}", path.display(), file.version);
        }
        Ok(Self { path, file, fault: None })
    }

    /// Change the ledger with `f` and save it; what is in memory changes only once it is on disk.
    pub fn commit<R>(&mut self, f: impl FnOnce(&mut PoolFile) -> R) -> Result<R> {
        let mut next = self.file.clone();
        let r = f(&mut next);
        if let Some(n) = self.fault {
            if n == 0 {
                self.fault = None;
                bail!("writing {} failed (an injected fault)", self.path.display());
            }
            self.fault = Some(n - 1);
        }
        write_private_json(&self.path, &next)?;
        self.file = next;
        Ok(r)
    }

    /// Tests: fail the write after the next `n` (0: the next one), once.
    #[doc(hidden)]
    pub fn fail_write_after(&mut self, n: Option<usize>) {
        self.fault = n;
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn kit(&self, kit_id: &str) -> Option<&KitRec> {
        self.file.kits.iter().find(|k| k.kit_id == kit_id)
    }
}

/// A crash between the queue's write and the ledger's: every pool entry a queued charge pays to
/// is that charge's, and its secrets are in the queue.
pub fn reconcile(queue: &Queue, pool: &mut PoolLedger) -> Result<usize> {
    let owed: Vec<(String, String)> =
        queue.file.charges.iter().flat_map(|c| c.pool.iter().map(|p| (p.blinded.clone(), c.txid.clone()))).collect();
    let behind = pool.file.entries.iter().filter(|e| matches!(e.state, EntryState::Issued { .. }) && owed.iter().any(|(b, _)| *b == e.blinded)).count();
    if behind == 0 {
        return Ok(0);
    }
    pool.commit(|f| {
        for e in f.entries.iter_mut() {
            if let (EntryState::Issued { .. }, Some((_, txid))) = (&e.state, owed.iter().find(|(b, _)| *b == e.blinded)) {
                e.state = EntryState::Used { txid: txid.clone() };
            }
        }
    })?;
    tracing::warn!("offline pool: {behind} entr(ies) of queued phone charges were still issued; marked used");
    Ok(behind)
}

/// What a kit and an ingest need of the till's federation.
#[derive(Clone)]
pub struct TillKeys {
    pub federation_id: String,
    pub mint_id: ModuleInstanceId,
    pub tbs_pks: Tiered<AggregatePublicKey>,
    pub fee: FeeConsensus,
    pub surplus_allowed: bool,
}

impl TillKeys {
    pub fn of(w: &crate::wallet::Wallet) -> Self {
        Self {
            federation_id: w.federation_id().to_string(),
            mint_id: w.mint_id,
            tbs_pks: w.mint_cfg.tbs_pks.clone(),
            fee: w.mint_cfg.fee_consensus.clone(),
            surplus_allowed: w.surplus_allowed(),
        }
    }

    fn fee_msat(&self, a: u64) -> u64 {
        self.fee.fee(Amount::from_msats(a)).msats
    }

    /// The tiers a kit's pool covers: from the smallest note the planner gives the till
    /// (`plan::lowest_till_note`) to the largest at or below the phone's value cap.
    pub fn pool_tiers(&self, cap_msat: u64) -> Vec<u64> {
        let fee = |a: u64| self.fee_msat(a);
        let lowest = plan::lowest_till_note(&fee, self.fee.min_economical_denomination().msats);
        self.tbs_pks.iter().map(|(a, _)| a.msats).filter(|t| t.is_power_of_two() && *t >= lowest && *t <= cap_msat).collect()
    }
}

// ------------------------------------------------------------------ kits

/// The kit for `phone_id` (`GET /v1/offline/kit`): the one it has, while it is young and full
/// enough (or the phone is at its cap of live kits), else a new one, written to the ledger before
/// it is answered. Each call reclaims kits past their redeem window. `NotReady` without the
/// registry's certificate key.
pub fn issue_kit(
    pool: &Mutex<PoolLedger>,
    keys: &TillKeys,
    cfg: &OfflineConfig,
    kits: &KitConfig,
    cert_key: Option<String>,
    header: Option<[u8; 80]>,
    phone_id: &str,
    now: u64,
) -> Result<Kit> {
    let Some(cert_key) = cert_key else {
        bail!(OfflineError::NotReady(
            "this till has no certificate key from the registry yet (it fetches one while online), so it cannot issue an offline kit".into()
        ));
    };
    let mut pool = pool.lock().unwrap();
    reclaim(&mut pool, now)?;
    let live: Vec<&KitRec> = pool.file.kits.iter().filter(|k| k.phone_id == phone_id && !k.reclaimed && k.expires_at_ms > now).collect();
    let newest = live.iter().max_by_key(|k| k.issued_at_ms).copied();
    let again = newest.filter(|k| live.len() >= MAX_LIVE_KITS || (now.saturating_sub(k.issued_at_ms) < REDELIVER_WITHIN_MS && fewest_left(&pool, k) >= REDELIVER_MIN_LEFT));
    let rec = match again {
        Some(k) => k.clone(),
        None => mint_kit(&mut pool, keys, cfg, kits, &cert_key, header, phone_id, now)?,
    };
    let kit = kit_of(&pool, &rec, keys, cfg, &cert_key, header);
    let len = serde_json::to_vec(&kit)?.len();
    if len > KIT_MAX_BYTES {
        bail!("the offline kit would be {len} bytes, more than {KIT_MAX_BYTES}");
    }
    Ok(kit)
}

/// Entries of `kit` still issued, in its emptiest tier.
fn fewest_left(pool: &PoolLedger, kit: &KitRec) -> usize {
    let mut per: std::collections::BTreeMap<u64, usize> = Default::default();
    for e in pool.file.entries.iter().filter(|e| e.kit_id == kit.kit_id) {
        let n = per.entry(e.amount_msat).or_default();
        if matches!(e.state, EntryState::Issued { .. }) {
            *n += 1;
        }
    }
    per.values().copied().min().unwrap_or(0)
}

/// The kit `rec` as a phone gets it: its entries still issued, and the till's keys, fees, caps
/// and header as they are now.
fn kit_of(pool: &PoolLedger, rec: &KitRec, keys: &TillKeys, cfg: &OfflineConfig, cert_key: &str, header: Option<[u8; 80]>) -> Kit {
    let mut tiers: Vec<PoolTier> = vec![];
    for e in pool.file.entries.iter().filter(|e| e.kit_id == rec.kit_id) {
        if !matches!(e.state, EntryState::Issued { .. }) {
            continue;
        }
        match tiers.iter_mut().find(|t| t.amount_msat == e.amount_msat) {
            Some(t) => t.blinded.push(e.blinded.clone()),
            None => tiers.push(PoolTier { amount_msat: e.amount_msat, blinded: vec![e.blinded.clone()] }),
        }
    }
    tiers.sort_by_key(|t| t.amount_msat);
    Kit {
        version: KIT_VERSION,
        kit_id: rec.kit_id.clone(),
        phone_id: rec.phone_id.clone(),
        federation_id: keys.federation_id.clone(),
        mint_instance_id: keys.mint_id,
        tbs_pks: keys.tbs_pks.iter().map(|(a, pk)| (a.msats.to_string(), pk.consensus_encode_to_hex())).collect(),
        fee_consensus: keys.fee.consensus_encode_to_hex(),
        fee_headroom: cfg.fee_headroom,
        surplus_allowed: keys.surplus_allowed,
        cert_key: cert_key.to_owned(),
        cap_msat: cfg.cap_msat,
        cap_count: cfg.cap_count,
        issued_at_ms: rec.issued_at_ms,
        expires_at_ms: rec.expires_at_ms,
        block_header: header.map(hex::encode),
        pool: tiers,
    }
}

/// A new kit: [`KIT_DEPTH`] entries per tier, fewer if the kit would pass [`KIT_MAX_BYTES`].
#[allow(clippy::too_many_arguments)]
fn mint_kit(
    pool: &mut PoolLedger,
    keys: &TillKeys,
    cfg: &OfflineConfig,
    kits: &KitConfig,
    cert_key: &str,
    header: Option<[u8; 80]>,
    phone_id: &str,
    now: u64,
) -> Result<KitRec> {
    let rec = KitRec {
        kit_id: hex::encode(rand::random::<[u8; 16]>()),
        phone_id: phone_id.to_owned(),
        issued_at_ms: now,
        expires_at_ms: now + kits.ttl_ms,
        redeem_until_ms: now + kits.ttl_ms + kits.grace_ms,
        reclaimed: false,
    };
    let tiers = keys.pool_tiers(cfg.cap_msat);
    if tiers.is_empty() {
        bail!("the federation has no note tier a kit's pool could hold");
    }
    // The kit's size: what it is with an empty pool, a tier's brackets, and 99 bytes an entry (its
    // 96 hex characters, quotes and a comma).
    let mut bare = kit_of(pool, &rec, keys, cfg, cert_key, header);
    bare.pool = tiers.iter().map(|t| PoolTier { amount_msat: *t, blinded: vec![] }).collect();
    let base = serde_json::to_vec(&bare)?.len();
    let room = KIT_MAX_BYTES.saturating_sub(base + 1_024) / (tiers.len() * 99);
    let depth = KIT_DEPTH.min(room);
    if depth < KIT_DEPTH {
        tracing::warn!("offline kit: {} tiers fit {depth} entries each under {KIT_MAX_BYTES} bytes, not {KIT_DEPTH}", tiers.len());
    }
    if depth == 0 {
        bail!("an offline kit of {} tiers does not fit in {KIT_MAX_BYTES} bytes", tiers.len());
    }
    let mut entries = Vec::with_capacity(tiers.len() * depth);
    for t in &tiers {
        for _ in 0..depth {
            let kp = Keypair::new(SECP256K1, &mut rand::thread_rng());
            let bk = BlindingKey::random();
            entries.push(PoolRec {
                blinded: blind(Nonce(kp.public_key()), bk).consensus_encode_to_hex(),
                amount_msat: *t,
                kit_id: rec.kit_id.clone(),
                state: EntryState::Issued { secret_key: hex::encode(kp.secret_bytes()), blinding_key: bk.consensus_encode_to_hex() },
            });
        }
    }
    pool.commit(|f| {
        f.kits.push(rec.clone());
        f.entries.extend(entries);
    })?;
    Ok(rec)
}

/// Kits past their redeem window: their entries never used are wiped (`Reclaimed`).
fn reclaim(pool: &mut PoolLedger, now: u64) -> Result<()> {
    let due: HashSet<String> = pool.file.kits.iter().filter(|k| !k.reclaimed && k.redeem_until_ms <= now).map(|k| k.kit_id.clone()).collect();
    if due.is_empty() {
        return Ok(());
    }
    pool.commit(|f| {
        for k in f.kits.iter_mut().filter(|k| due.contains(&k.kit_id)) {
            k.reclaimed = true;
        }
        for e in f.entries.iter_mut().filter(|e| due.contains(&e.kit_id) && matches!(e.state, EntryState::Issued { .. })) {
            e.state = EntryState::Reclaimed;
        }
    })
}

// ------------------------------------------------------------------ ingest

/// What became of one charge a phone sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ingested {
    pub txid: String,
    /// `queued` (now the till's), `already` (it had it), or `refused`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// A refusal for now (the till could not write it): the phone sends it again later.
    pub retry: bool,
}

impl Ingested {
    fn ok(txid: &str, status: &'static str) -> Self {
        Self { txid: txid.to_owned(), status, code: None, detail: None, retry: false }
    }

    fn refused(txid: &str, code: &'static str, detail: String, retry: bool) -> Self {
        Self { txid: txid.to_owned(), status: "refused", code: Some(code), detail: Some(detail), retry }
    }
}

/// A charge a phone sent, as its signed bytes say.
struct Checked {
    unsigned_hex: String,
    till_msat: u64,
    fees_msat: u64,
    fee_margin_msat: u64,
}

/// Take `charges` from phone `phone_id` into the till's queue (`POST /v1/offline/charges`).
/// Each is checked from its signed bytes alone: no federation, no registry. A charge whose txid
/// the queue or the ledger has is `already`; one that checks is queued with the till's secrets for
/// its pool outputs, then its pool entries are marked used, then its sale is recorded.
pub fn ingest(
    queue: &Mutex<Queue>,
    pool: &Mutex<PoolLedger>,
    keys: &TillKeys,
    sales: &Mutex<SalesStore>,
    phone_id: &str,
    charges: Vec<Value>,
    now: u64,
) -> Vec<Ingested> {
    let decoders = mint_decoders(keys.mint_id);
    charges
        .into_iter()
        .map(|v| {
            let hint = v.get("txid").and_then(Value::as_str).unwrap_or_default().to_owned();
            match serde_json::from_value::<OfflineCharge>(v) {
                Ok(c) => ingest_one(queue, pool, keys, &decoders, sales, phone_id, c, now),
                Err(e) => Ingested::refused(&hint, "bad_charge", format!("not an offline charge: {e}"), false),
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn ingest_one(
    queue: &Mutex<Queue>,
    pool: &Mutex<PoolLedger>,
    keys: &TillKeys,
    decoders: &ModuleDecoderRegistry,
    sales: &Mutex<SalesStore>,
    phone_id: &str,
    c: OfflineCharge,
    now: u64,
) -> Ingested {
    let checked = match check(&c, keys, decoders) {
        Ok(x) => x,
        Err((code, detail)) => return Ingested::refused(&c.txid, code, detail, false),
    };
    // The queue's lock, then the ledger's: the order every writer of both keeps.
    let mut q = queue.lock().unwrap();
    let mut pl = pool.lock().unwrap();
    let mine = |state: &EntryState| matches!(state, EntryState::Used { txid } if *txid == c.txid);
    let had = q.file.charges.iter().any(|x| x.txid == c.txid)
        || c.pool.iter().any(|p| pl.file.entries.iter().any(|e| e.blinded == p.blinded && mine(&e.state)));
    if had {
        let entry = q.file.charges.iter().find(|x| x.txid == c.txid).cloned();
        drop(pl);
        drop(q);
        // A repeat after an answer that was lost: the sale, if it was not recorded.
        record_sale(sales, entry.as_ref().unwrap_or(&c), phone_id);
        return Ingested::ok(&c.txid, "already");
    }
    let mut host = vec![];
    for p in &c.pool {
        let Some(e) = pl.file.entries.iter().find(|e| e.blinded == p.blinded) else {
            return Ingested::refused(&c.txid, "pool_entry_unknown", format!("pool entry {} was never issued by this till", p.blinded), false);
        };
        let Some(kit) = pl.kit(&e.kit_id).filter(|k| k.phone_id == phone_id) else {
            return Ingested::refused(&c.txid, "pool_entry_unknown", format!("pool entry {} was not issued to phone {phone_id}", p.blinded), false);
        };
        if e.amount_msat != p.amount_msat {
            return Ingested::refused(&c.txid, "bad_charge", format!("pool entry {} is {} msat, not {}", p.blinded, e.amount_msat, p.amount_msat), false);
        }
        let reused = q.file.charges.iter().any(|x| x.txid != c.txid && x.pool.iter().any(|y| y.blinded == p.blinded));
        match &e.state {
            EntryState::Used { .. } => {
                return Ingested::refused(&c.txid, "pool_entry_reused", format!("pool entry {} paid another charge", p.blinded), false);
            }
            _ if reused => {
                return Ingested::refused(&c.txid, "pool_entry_reused", format!("pool entry {} paid another charge", p.blinded), false);
            }
            EntryState::Issued { secret_key, blinding_key } if kit.redeem_until_ms > now => host.push(HostPending {
                amount_msat: p.amount_msat,
                secret_key: secret_key.clone(),
                blinding_key: blinding_key.clone(),
                kind: HostKind::Till,
                group: c.txid.clone(),
                out_idx: Some(p.out_idx),
            }),
            EntryState::Issued { .. } | EntryState::Reclaimed => {
                return Ingested::refused(&c.txid, "pool_entry_expired", format!("pool entry {}'s kit {} is past its redeem window", p.blinded, kit.kit_id), false);
            }
        }
    }
    let entry = OfflineCharge {
        state: ChargeState::Queued,
        unsigned_hex: checked.unsigned_hex,
        signed_at: if c.signed_at > 0 { c.signed_at } else { now },
        host,
        till_msat: checked.till_msat,
        overpay_msat: checked.till_msat - c.price_msat,
        fees_msat: checked.fees_msat,
        fee_margin_msat: checked.fee_margin_msat,
        detail: None,
        synced: false,
        ..c
    };
    let nonces: Vec<String> = entry.inputs.iter().map(|i| i.nonce.clone()).collect();
    if let Some(n) = nonces.iter().find(|n| q.file.spent.contains(n)) {
        tracing::warn!("phone charge {}: note {n} was already taken offline here; the registry decides which is a double spend", entry.txid);
    }
    let r = q.commit(|f| {
        f.charges.push(entry.clone());
        for n in &nonces {
            if !f.spent.contains(n) {
                f.spent.push(n.clone());
            }
        }
    });
    if let Err(e) = r {
        return Ingested::refused(&entry.txid, "internal", format!("{e:#}"), true);
    }
    let blinded: Vec<&str> = entry.pool.iter().map(|p| p.blinded.as_str()).collect();
    let r = pl.commit(|f| {
        for e in f.entries.iter_mut().filter(|e| blinded.contains(&e.blinded.as_str())) {
            e.state = EntryState::Used { txid: entry.txid.clone() };
        }
    });
    if let Err(e) = r {
        // The queue has the charge and its secrets: the ledger catches up when it is next opened.
        tracing::warn!("phone charge {} queued, its pool entries not yet marked used: {e:#}", entry.txid);
    }
    drop(pl);
    drop(q);
    record_sale(sales, &entry, phone_id);
    Ingested::ok(&entry.txid, "queued")
}

/// The sale of a phone's charge, `offline` as the till's own; a `client_ref` the ledger has under
/// another transaction is dropped (with a warning) rather than the sale.
fn record_sale(sales: &Mutex<SalesStore>, c: &OfflineCharge, phone_id: &str) {
    let rec = SaleRec {
        id: 0,
        ts_unix_ms: c.ts_unix_ms,
        price_msat: c.price_msat,
        received_msat: 0,
        overpaid_msat: c.overpay_msat,
        fees_msat: c.fees_msat,
        txid: c.txid.clone(),
        inputs: c.inputs.iter().map(|i| SlotMsat { slot: i.slot, msat: i.amount_msat }).collect(),
        change: c.change.iter().map(|ch| SlotMsat { slot: ch.slot, msat: ch.amount_msat }).collect(),
        collection_pending: true,
        client_ref: c.client_ref.clone(),
        duration_ms: 0,
        warnings: vec![format!("taken offline by phone {phone_id}")],
        offline: true,
        double_spent: false,
        rejected: false,
    };
    let mut s = sales.lock().unwrap();
    if let Err(e) = s.append(rec.clone()) {
        tracing::warn!("phone charge {}: {e:#}; recorded without its client_ref", c.txid);
        if let Err(e) = s.append(SaleRec { client_ref: None, ..rec }) {
            tracing::warn!("phone charge {}: sale not recorded: {e:#}", c.txid);
        }
    }
}

type Refusal = (&'static str, String);

/// The charge as its signed bytes say, checked against everything it declares: the txid, its
/// inputs (mint notes of this till's federation, each with its card's signature over the txid),
/// and each output either declared change (`blind(nonce, blinding key)`) or a pool entry, at its
/// index. A fee the inputs no longer cover is logged, not refused: the money is taken already.
fn check(c: &OfflineCharge, keys: &TillKeys, decoders: &ModuleDecoderRegistry) -> Result<Checked, Refusal> {
    let bad = |m: String| ("bad_charge", m);
    let signed = c.signed_hex.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| bad("no signed transaction".into()))?;
    let mut tx = Transaction::consensus_decode_hex(signed, decoders).map_err(|e| bad(format!("the signed transaction does not decode: {e}")))?;
    let txid = tx.tx_hash();
    if txid.to_string() != c.txid {
        return Err(bad(format!("the signed transaction is {txid}, not {}", c.txid)));
    }
    if c.short_id.len() != 8 || !c.short_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad("short_id is not 8 hex characters".into()));
    }
    if c.client_ref.as_deref().is_some_and(|r| r.is_empty() || r.len() > fedimint_card_pos_core::charge::MAX_CLIENT_REF || !r.is_ascii()) {
        return Err(bad("client_ref is not 1 to 128 ASCII characters".into()));
    }
    let TransactionSignature::NaiveMultisig(sigs) = &tx.signatures else { return Err(bad("not signed by the card".into())) };
    if tx.inputs.is_empty() || sigs.len() != tx.inputs.len() || c.inputs.len() != tx.inputs.len() {
        return Err(bad(format!("{} inputs, {} signatures, {} declared", tx.inputs.len(), sigs.len(), c.inputs.len())));
    }
    let reg = ModuleRegistry::default();
    let msg = Message::from_digest(txid.to_byte_array());
    let mut in_sum = 0u64;
    let mut fees = 0u64;
    for ((input, sig), declared) in tx.inputs.iter().zip(sigs).zip(&c.inputs) {
        let note = input
            .as_any()
            .downcast_ref::<MintInput>()
            .filter(|_| input.module_instance_id() == keys.mint_id)
            .and_then(MintInput::maybe_v0_ref)
            .ok_or_else(|| bad("an input that is not a note of this till's mint".into()))?;
        let nonce_hex = hex::encode(note.note.nonce.0.serialize());
        if note.amount.msats != declared.amount_msat || nonce_hex != declared.nonce || note.note.signature.consensus_encode_to_hex() != declared.signature {
            return Err(bad(format!("input {nonce_hex} is not what the charge declares")));
        }
        let pk = keys.tbs_pks.tier(&note.amount).map_err(|_| ("bad_note", format!("{} msat is not a denomination of this federation", note.amount.msats)))?;
        if !note.note.verify(*pk) {
            return Err(("bad_note", format!("note {nonce_hex}'s federation signature does not verify")));
        }
        let key = XOnlyPublicKey::from(note.note.nonce.0);
        if SECP256K1.verify_schnorr(sig, &msg, &key).is_err() {
            return Err(("bad_signature", format!("the card's signature for note {nonce_hex} does not verify")));
        }
        in_sum += note.amount.msats;
        fees += keys.fee_msat(note.amount.msats);
    }
    if c.pool.is_empty() || tx.outputs.len() != c.change.len() + c.pool.len() {
        return Err(bad(format!("{} outputs, {} change and {} pool declared", tx.outputs.len(), c.change.len(), c.pool.len())));
    }
    let mut out_sum = 0u64;
    let mut till_msat = 0u64;
    for (i, output) in tx.outputs.iter().enumerate() {
        let out = output
            .as_any()
            .downcast_ref::<MintOutput>()
            .filter(|_| output.module_instance_id() == keys.mint_id)
            .and_then(MintOutput::maybe_v0_ref)
            .ok_or_else(|| bad(format!("output {i} is not a note of this till's mint")))?;
        let i = i as u64;
        let amount = out.amount.msats;
        let ok = if let Some(ch) = c.change.iter().find(|ch| ch.out_idx == i) {
            let nonce = PublicKey::from_str(&ch.nonce).map(Nonce).map_err(|_| bad(format!("change {i}'s nonce")))?;
            let bk = BlindingKey::consensus_decode_hex(&ch.blinding_key, &reg).map_err(|_| bad(format!("change {i}'s blinding key")))?;
            ch.amount_msat == amount && blind(nonce, bk) == out.blind_nonce.0 && nonce_for(XOnlyPublicKey::from(nonce.0)) == nonce
        } else if let Some(p) = c.pool.iter().find(|p| p.out_idx == i) {
            till_msat += amount;
            p.amount_msat == amount && p.blinded == out.blind_nonce.0.consensus_encode_to_hex()
        } else {
            false
        };
        if !ok {
            return Err(bad(format!("output {i} is neither the change nor the pool entry the charge declares")));
        }
        out_sum += amount;
        fees += keys.fee_msat(amount);
    }
    if c.price_msat == 0 || c.price_msat > till_msat {
        return Err(bad(format!("a price of {} msat for {till_msat} msat to the till", c.price_msat)));
    }
    if in_sum < out_sum + fees {
        tracing::warn!(
            "phone charge {}: its inputs ({in_sum} msat) no longer cover its outputs and today's fees ({} msat); the federation refuses it unless they fall",
            c.txid,
            out_sum + fees
        );
    }
    tx.signatures = TransactionSignature::NaiveMultisig(vec![]);
    Ok(Checked { unsigned_hex: tx.consensus_encode_to_hex(), till_msat, fees_msat: fees, fee_margin_msat: in_sum.saturating_sub(out_sum + fees) })
}
