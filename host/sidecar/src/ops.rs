//! The till on ops (docs/OPS-PLAN.md § 5–7, phase O3): the card side of the state lives in the
//! registry Worker, and the till reads it on every card job and writes it back through the
//! registry's routes.
//!
//! What stays on the till: its own outputs (`host_pending`, the merchant's note secrets), its
//! sales ledger and `cashouts.log`. What comes from ops: the card's slot keys, its notes and an
//! open charge. The local file is per card, `<data_dir>/cards/<short_id>.json` in the
//! `card_state.json` format, and holds the till's part plus a working copy of the card's.
//! [`Ops::attach`] delivers whatever the till still owes ops (`ops_outbox`), then replaces the
//! card's part with what ops answers.
//!
//! Write order, as in `glue.rs` `sign_and_submit`, moved to ops: `intent` (the unsigned
//! transaction; a txid does not cover signatures) before the card signs, `submitted` (the signed
//! bytes) before the till sends, `outcome` once the federation answered. A charge open in ops
//! without signed bytes never left its till and is recorded rejected. One with signed bytes may be
//! in the federation, so the next till resubmits those bytes (idempotent) and records what the
//! federation answers. A till never re-signs another till's charge: the holder at the second till
//! did not agree to it.
//!
//! Split keying (a 32-slot card registered with keys for slots 0-7 only): the registry lists the
//! card's never-keyed slots as `unkeyed_slots`, and nothing can be loaded into one until it has a
//! key on record. When a tap needs more free slots than the card has keyed (a load, or a charge
//! whose change needs room), the till keys the lowest unkeyed slots on the card while it is in the
//! field ([`key_slots`]: `GET_PUB`, and `GEN_KEY` with P2 = 00 on `6985`; no PIN) and records them
//! with `POST /cards/:id/slot-keys` before it goes on. It keys the shortfall plus [`KEY_AHEAD`].
//! Keys are never queued in `ops_outbox`: a tap that cannot record them in ops refuses the work
//! that needed them. A tap that keyed slots but died before the POST leaves keys on the card that
//! the next tap's `GET_PUB` finds and records; a POST whose answer was lost is replayed, which the
//! registry answers without change.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use fedimint_card_host_card::{Card, SW_NO_SPACE, SlotState, status_word};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::glue::{CardWallet, on_card};
use crate::state::{CardState, LoadNote, NoteRec, NoteStatus, OpsWrite, PendingTx, SlotRec, StateStore};

/// Every registry call is bounded; a till that cannot reach ops says so instead of hanging.
const OPS_TIMEOUT: Duration = Duration::from_secs(15);
/// Where the per-card files live inside the data directory.
pub const CARDS_DIR: &str = "cards";
/// Unkeyed slots a tap keys beyond what its load or change needs, so the next few taps on the
/// card need none. A slot's first `GEN_KEY` takes about 318 ms on a J3R180 (docs/CAP-BUILD.md),
/// so the margin adds at most about 1 s to the tap that keys.
pub const KEY_AHEAD: usize = 3;

/// Fresh keyed slots (keyed, never carried a note, KEYED on the card) an online tap keeps a 3.0
/// card at, so it can take change offline (docs/OFFLINE-PLAN.md § 10 "Spare slots offline"):
/// offline, change goes only to such slots, and no till can key one without the registry.
pub const SPARE_TARGET: usize = 12;
/// Most slots one tap keys for that reserve. A `GEN_KEY` takes about 320 ms on a J3R180.
pub const MAX_KEY_PER_TAP: usize = 2;
/// What the reserve's budget counts, per card command (J3R180 estimates: a `GEN_KEY` measured
/// at about 320 ms, a `RELEASE` checks a P-256 signature in about 75 ms, a `SET_NOTE` writes
/// 56 bytes).
const GEN_KEY_MS: u64 = 320;
const RELEASE_MS: u64 = 100;
const SET_NOTE_MS: u64 = 40;

/// The spare reserve's settings (`till serve --spare-target`, `--spare-keys-per-tap`). A target
/// or a per-tap cap of 0 turns it off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpareConfig {
    pub target: usize,
    pub max_per_tap: usize,
}

impl Default for SpareConfig {
    fn default() -> Self {
        Self { target: SPARE_TARGET, max_per_tap: MAX_KEY_PER_TAP }
    }
}

/// How many slots a tap keys for the spare reserve: none when the card has `fresh >= target`,
/// else the shortfall, but never more than the per-tap budget (`max_per_tap` keys' worth of card
/// time) leaves after the `released` releases and `written` notes the tap already sent.
pub fn spares_to_key(fresh: usize, cfg: SpareConfig, released: usize, written: usize) -> usize {
    if fresh >= cfg.target {
        return 0;
    }
    let budget = cfg.max_per_tap as u64 * GEN_KEY_MS;
    let busy = released as u64 * RELEASE_MS + written as u64 * SET_NOTE_MS;
    let n = (budget.saturating_sub(busy) / GEN_KEY_MS) as usize;
    n.min(cfg.target - fresh)
}

/// What [`CardWallet::card_spares`] did at a tap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpareReport {
    /// Fresh keyed slots the card had before.
    pub fresh: usize,
    /// SPENT slots re-keyed (queued to ops as renewals, as every renewal is).
    pub renewed: Vec<u8>,
    /// Never-keyed slots keyed and recorded in ops (`POST /slot-keys`).
    pub keyed: Vec<u8>,
}

/// `--ops-url` and `--ops-token`. The token is a till token (`till_…`) and is never printed.
#[derive(Clone)]
pub struct OpsConfig {
    pub url: String,
    pub token: String,
}

impl std::fmt::Debug for OpsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpsConfig").field("url", &self.url).field("token", &"<redacted>").finish()
    }
}

/// How a registry call failed. The till service maps these to its own error codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpsError {
    /// 409: the card changed under this till (stale version, an open charge, a resolved one).
    Changed(String),
    /// 404: no such card for this till's operator.
    UnknownCard(String),
    /// No answer, or a 5xx.
    Unreachable(String),
    /// Any other refusal (400, 401, 403).
    Refused(u16, String),
}

impl std::fmt::Display for OpsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpsError::Changed(m) => write!(f, "ops: the card changed: {m}"),
            OpsError::UnknownCard(m) => write!(f, "ops: {m}"),
            OpsError::Unreachable(m) => write!(f, "ops unreachable: {m}"),
            OpsError::Refused(s, m) => write!(f, "ops refused ({s}): {m}"),
        }
    }
}
impl std::error::Error for OpsError {}

/// The card's state as `GET /cards/:short_id/state` answers it.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteState {
    pub version: u64,
    /// Keyed slots only; see `unkeyed_slots`.
    pub slots: Vec<RemoteSlot>,
    /// Slots the card has with no key on record yet (split keying), ascending. Empty from a
    /// registry older than split keying.
    #[serde(default)]
    pub unkeyed_slots: Vec<u8>,
    pub notes: Vec<RemoteNote>,
    pub open_txs: Vec<RemoteTx>,
    /// The registry's releases for this card (docs/OFFLINE-PROTOCOL.md § 6). Empty from a registry
    /// older than releases.
    #[serde(default)]
    pub releases: Vec<RemoteRelease>,
}

/// A release: the registry's signature that `txid` will never be accepted, for one slot.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteRelease {
    pub slot: u8,
    /// 32 bytes, hex.
    pub txid: String,
    /// DER ECDSA P-256, hex.
    pub sig: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteSlot {
    pub slot: u8,
    pub pubkey: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteNote {
    pub slot: u8,
    pub amount_msat: u64,
    pub nonce: String,
    pub signature: Option<String>,
    pub blinding_key: Option<String>,
    pub status: NoteStatus,
    pub txid: Option<String>,
    pub out_idx: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteTx {
    pub txid: String,
    pub till_id: u64,
    pub client_ref: String,
    /// The signed bytes, once the till that made the charge was about to send them.
    pub signed_hex: Option<String>,
    pub inputs: Vec<String>,
}

/// A registry client for one till token. Cheap to clone.
#[derive(Clone)]
pub struct OpsClient {
    http: reqwest::Client,
    url: String,
    token: String,
}

impl OpsClient {
    pub fn new(cfg: &OpsConfig) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(OPS_TIMEOUT).build().context("building the ops HTTP client")?;
        Ok(Self { http, url: cfg.url.trim_end_matches('/').to_owned(), token: cfg.token.clone() })
    }

    /// The registry's URL, without a trailing slash.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// A client with no token, for the holder routes (`GET /cards/:id/slots`, `POST …/load`).
    pub fn holder(url: &str) -> Result<Self> {
        Self::new(&OpsConfig { url: url.to_owned(), token: String::new() })
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Value, OpsError> {
        let mut req = self.http.request(method, format!("{}{path}", self.url));
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.map_err(|e| OpsError::Unreachable(format!("{path}: {e}")))?;
        let status = resp.status().as_u16();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return Ok(v);
        }
        let reason = v["reason"].as_str().unwrap_or("no reason given").to_owned();
        Err(match status {
            409 => OpsError::Changed(reason),
            404 => OpsError::UnknownCard(reason),
            s if s >= 500 => OpsError::Unreachable(format!("{path}: {s} {reason}")),
            s => OpsError::Refused(s, reason),
        })
    }

    fn version(v: &Value) -> Result<u64, OpsError> {
        v["version"].as_u64().ok_or_else(|| OpsError::Unreachable("the registry answered without a version".into()))
    }

    pub async fn state(&self, short_id: &str) -> Result<RemoteState, OpsError> {
        let v = self.call(reqwest::Method::GET, &format!("/cards/{short_id}/state"), None).await?;
        serde_json::from_value(v).map_err(|e| OpsError::Unreachable(format!("the card state did not parse: {e}")))
    }

    /// `POST /cards/:id/intent`. Answers the card's new version.
    pub async fn intent(&self, short_id: &str, body: &Value) -> Result<u64, OpsError> {
        Self::version(&self.call(reqwest::Method::POST, &format!("/cards/{short_id}/intent"), Some(body)).await?)
    }

    pub async fn submitted(&self, short_id: &str, txid: &str, tx_hex: &str) -> Result<u64, OpsError> {
        let body = json!({"txid": txid, "tx_hex": tx_hex});
        Self::version(&self.call(reqwest::Method::POST, &format!("/cards/{short_id}/submitted"), Some(&body)).await?)
    }

    pub async fn outcome(&self, short_id: &str, body: &Value) -> Result<u64, OpsError> {
        Self::version(&self.call(reqwest::Method::POST, &format!("/cards/{short_id}/outcome"), Some(body)).await?)
    }

    pub async fn renew_slot(&self, short_id: &str, slot: u8, body: &Value) -> Result<u64, OpsError> {
        Self::version(&self.call(reqwest::Method::PUT, &format!("/cards/{short_id}/slots/{slot}"), Some(body)).await?)
    }

    /// `GET /registry/cert-key` within `timeout`: whether the registry answered at all, and its
    /// certificate key (hex) when it has one. Also the till's reachability probe.
    pub async fn cert_key(&self, timeout: Duration) -> Result<Option<String>, OpsError> {
        let resp = self
            .http
            .get(format!("{}/registry/cert-key", self.url))
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| OpsError::Unreachable(format!("/registry/cert-key: {e}")))?;
        let status = resp.status().as_u16();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        match status {
            200 => Ok(v["key"].as_str().filter(|k| k.len() == 130).map(str::to_owned)),
            // The registry itself, without a certificate key (or an older one without the route).
            503 | 404 if v.get("reason").is_some() || v.get("ok").is_some() => Ok(None),
            s if s >= 500 => Err(OpsError::Unreachable(format!("/registry/cert-key: {s}"))),
            _ => Ok(None),
        }
    }

    /// `POST /cards/:id/offline-charges` (docs/OFFLINE-PROTOCOL.md § 3): an offline charge,
    /// reported after the fact.
    pub async fn offline_charge(&self, short_id: &str, body: &Value) -> Result<OfflineAnswer, OpsError> {
        let resp = self
            .http
            .post(format!("{}/cards/{short_id}/offline-charges", self.url))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|e| OpsError::Unreachable(format!("/offline-charges: {e}")))?;
        let status = resp.status().as_u16();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        let reason = v["reason"].as_str().unwrap_or("no reason given").to_owned();
        match status {
            200..=299 => Ok(OfflineAnswer::Recorded(Self::version(&v)?)),
            409 if reason == "double_spend" => Ok(OfflineAnswer::DoubleSpend(v["conflicts"].clone())),
            409 => Err(OpsError::Changed(reason)),
            404 => Err(OpsError::UnknownCard(reason)),
            s if s >= 500 => Err(OpsError::Unreachable(format!("/offline-charges: {s} {reason}"))),
            s => Err(OpsError::Refused(s, reason)),
        }
    }

    /// `POST /federations`: what a till read from the federation's config ([`FederationReport`]).
    pub async fn federation(&self, report: &FederationReport) -> Result<(), OpsError> {
        let mut body = json!({"federation_id": report.federation_id, "tbs_pks": report.tbs_pks});
        if let Some(name) = report.name.as_deref().filter(|n| !n.trim().is_empty()) {
            body["name"] = json!(name);
        }
        self.call(reqwest::Method::POST, "/federations", Some(&body)).await.map(|_| ())
    }

    /// `POST /cards/:id/slot-keys` (till): the first keys of never-keyed slots, all or nothing.
    /// The same keys again replay without change.
    pub async fn slot_keys(&self, short_id: &str, keys: &[(u8, String)]) -> Result<SlotKeysAnswer, OpsError> {
        let body = json!({"keys": keys.iter().map(|(slot, pubkey)| json!({"slot": slot, "pubkey": pubkey})).collect::<Vec<_>>()});
        let v = self.call(reqwest::Method::POST, &format!("/cards/{short_id}/slot-keys"), Some(&body)).await?;
        serde_json::from_value(v).map_err(|e| OpsError::Unreachable(format!("the slot-keys answer did not parse: {e}")))
    }

    /// `GET /cards/:id/slots` (holder): the card's free slot keys, its version and federation.
    pub async fn free_slots(&self, short_id: &str) -> Result<FreeSlots, OpsError> {
        let v = self.call(reqwest::Method::GET, &format!("/cards/{short_id}/slots"), None).await?;
        serde_json::from_value(v).map_err(|e| OpsError::Unreachable(format!("the free slots did not parse: {e}")))
    }

    /// `POST /cards/:id/load` (holder): the notes, pending, before the wallet submits.
    pub async fn load(&self, short_id: &str, body: &Value) -> Result<u64, OpsError> {
        Self::version(&self.call(reqwest::Method::POST, &format!("/cards/{short_id}/load"), Some(body)).await?)
    }

    /// `POST /cards/:id/load/outcome` (holder): the signatures, or `{accepted: false}`.
    pub async fn load_outcome(&self, short_id: &str, body: &Value) -> Result<u64, OpsError> {
        Self::version(&self.call(reqwest::Method::POST, &format!("/cards/{short_id}/load/outcome"), Some(body)).await?)
    }
}

/// What `POST /cards/:id/offline-charges` answered.
#[derive(Debug, Clone, PartialEq)]
pub enum OfflineAnswer {
    /// Every input was live: recorded as an open, signed charge.
    Recorded(u64),
    /// Some input was not live (`409 double_spend`): the registry's `conflicts`.
    DoubleSpend(Value),
}

/// `GET /cards/:id/slots` as the registry answers it.
#[derive(Debug, Clone, Deserialize)]
pub struct FreeSlots {
    pub version: u64,
    pub federation_id: String,
    /// Keyed slots whose key never carried a note.
    pub free: Vec<RemoteSlot>,
    /// Slots with no key on record yet, ascending: a till keys them at a tap ([`key_slots`]).
    #[serde(default)]
    pub unkeyed_slots: Vec<u8>,
}

/// `POST /cards/:id/slot-keys` as the registry answers it.
#[derive(Debug, Clone, Deserialize)]
pub struct SlotKeysAnswer {
    /// The card's version after the keys; the next intent or load is made against it.
    pub version: u64,
    #[serde(default)]
    pub added: Vec<u8>,
    #[serde(default)]
    pub replayed: bool,
    #[serde(default)]
    pub unkeyed_slots: Vec<u8>,
}

/// How many unkeyed slots a tap keys when it needs `need` free slots and has `free` keyed ones:
/// none when they fit, else the shortfall plus [`KEY_AHEAD`], and never more than `unkeyed`.
pub fn slots_to_key(need: usize, free: usize, unkeyed: usize) -> usize {
    if need <= free {
        return 0;
    }
    (need - free + KEY_AHEAD).min(unkeyed)
}

/// What [`key_slots`] keyed: each slot's key as the card holds it, and the registry's answer.
#[derive(Debug, Clone)]
pub struct KeyedSlots {
    pub keys: Vec<(u8, String)>,
    pub answer: SlotKeysAnswer,
}

/// Give `slots` (never keyed in ops) keys on the card in the field, and record them in ops in one
/// `POST /slot-keys`. Per slot: `GET_PUB`; on `6985` (empty) `GEN_KEY` with P2 = 00, which needs
/// no PIN; a key already there is one an earlier tap made and could not record, and is used as
/// it is. The card's private keys never leave it, so any key in an unkeyed slot is the card's own.
///
/// Fails without recording anything if the card refuses (`6A84`: out of memory for the slot's
/// key) or ops does not answer; the keys stay on the card for the next tap to record.
pub async fn key_slots(card: &mut dyn Card, client: &OpsClient, short_id: &str, slots: &[u8]) -> Result<KeyedSlots> {
    let mut keys = vec![];
    for &slot in slots {
        let pk = match on_card(|| card.get_or_gen(slot)) {
            Ok(pk) => pk,
            Err(e) if status_word(&e) == Some(SW_NO_SPACE) => {
                return Err(e).with_context(|| format!("the card has no memory left for slot {slot}'s key (6A84)"));
            }
            Err(e) => return Err(e).with_context(|| format!("keying slot {slot} on the card")),
        };
        keys.push((slot, pk.to_string()));
    }
    if keys.is_empty() {
        bail!("no slot to key");
    }
    let answer = client.slot_keys(short_id, &keys).await.context("recording the card's new slot keys in ops")?;
    Ok(KeyedSlots { keys, answer })
}

/// What a till tells ops about its federation (`POST /federations`): the name, for the
/// provisioner to show, and the mint's aggregate key per note tier, which ops checks every note's
/// signature against before it is live (OPS-PLAN O4).
#[derive(Debug, Clone)]
pub struct FederationReport {
    pub federation_id: String,
    pub name: Option<String>,
    /// Tier in msat (decimal) to its `tbs::AggregatePublicKey`, 96 bytes compressed G2, hex.
    pub tbs_pks: std::collections::BTreeMap<String, String>,
}

impl FederationReport {
    /// Read from the wallet's copy of the federation's client config.
    pub async fn of(wallet: &crate::wallet::Wallet) -> Self {
        use fedimint_core::encoding::Encodable as _;
        let name = wallet.client.config().await.global.meta.get("federation_name").cloned();
        let tbs_pks = wallet
            .mint_cfg
            .tbs_pks
            .iter()
            .map(|(amount, pk)| (amount.msats.to_string(), pk.consensus_encode_to_hex()))
            .collect();
        Self { federation_id: wallet.federation_id().to_string(), name, tbs_pks }
    }
}

/// The card a job is working on, and the version of its state this till last saw.
#[derive(Clone)]
pub struct OpsLink {
    pub client: OpsClient,
    pub short_id: String,
    pub version: u64,
    /// Slot keys as ops last answered them.
    pub remote_slots: Vec<(u8, String)>,
    /// Slots with no key in ops yet (split keying), as ops last answered them.
    pub unkeyed: Vec<u8>,
    /// Charges open in ops as last read, plus any this job posted an intent for. Only these get an
    /// outcome: one another till already recorded needs nothing from this one.
    pub open: Vec<String>,
    /// The card's releases as ops last answered them ([`CardWallet::card_releases`]).
    pub releases: Vec<RemoteRelease>,
}

// Moved to the core the POS phones share (docs/POS-OFFLINE.md); kept at the old path.
pub use fedimint_card_pos_core::short_id_of;

/// The till's side of ops: the client and where the per-card files live.
#[derive(Clone)]
pub struct Ops {
    pub client: OpsClient,
    cards_dir: PathBuf,
}

impl Ops {
    pub fn new(cfg: &OpsConfig, data_dir: &Path) -> Result<Self> {
        Ok(Self { client: OpsClient::new(cfg)?, cards_dir: data_dir.join(CARDS_DIR) })
    }

    pub fn card_path(&self, short_id: &str) -> PathBuf {
        self.cards_dir.join(format!("{short_id}.json"))
    }

    /// Read the card's short id (`GET_PUB` slot 0), switch `cw` to that card's local file and
    /// load it from ops. Returns the store `cw` had, for [`finish`](Self::finish); on an error
    /// `cw` is left as it was.
    pub async fn attach(&self, cw: &mut CardWallet) -> Result<StateStore> {
        let pk = on_card(|| cw.card.get_pub(0)).context("reading the card's slot 0 key")?;
        let base = self.attach_id(cw, &short_id_of(&pk)).await?;
        // A 3.0 card: what the registry released (before planning, so this tap can use it), its
        // clock, any live note it lacks (a load or change finished elsewhere), then its spare
        // reserve.
        let released = cw.card_releases();
        let written = cw.card_tick();
        match cw.card_spares(released, written).await {
            Ok(r) if !r.renewed.is_empty() || !r.keyed.is_empty() => {
                tracing::info!("card {}: spare reserve from {} fresh slot(s): renewed {:?}, keyed {:?}", short_id_of(&pk), r.fresh, r.renewed, r.keyed);
            }
            Ok(_) => {}
            // The holder's PIN was refused: the charge would be refused the same way when it
            // signs, and verifying again there would spend a second try.
            Err(e) if pin_refused(&e) => {
                Self::finish(cw, base).await;
                return Err(e);
            }
            Err(e) => tracing::warn!("card {}: spare reserve not topped up this tap: {e:#}", short_id_of(&pk)),
        }
        Ok(base)
    }

    /// [`attach`](Self::attach) for a card named by its short id, without reading the card.
    pub async fn attach_id(&self, cw: &mut CardWallet, short_id: &str) -> Result<StateStore> {
        let card = StateStore::open(self.card_path(short_id))?;
        let base = std::mem::replace(&mut cw.store, card);
        cw.ops = Some(OpsLink { client: self.client.clone(), short_id: short_id.to_owned(), version: 0, remote_slots: vec![], unkeyed: vec![], open: vec![], releases: vec![] });
        match cw.ops_load().await {
            Ok(()) => Ok(base),
            Err(e) => {
                cw.ops = None;
                cw.store = base;
                Err(e)
            }
        }
    }

    /// End a job [`attach`](Self::attach) started: deliver the writes it queued (a failure stays
    /// queued in the card's file), then give `cw` its own store back.
    pub async fn finish(cw: &mut CardWallet, base: StateStore) {
        cw.ops_sync().await;
        cw.ops = None;
        cw.store = base;
    }

    /// The card whose local file has an unresolved charge, or an unrecorded sale, for
    /// `client_ref`. Lets a repeated `client_ref` be settled while no card is in the field.
    pub fn card_for_ref(&self, client_ref: &str) -> Option<String> {
        self.card_files().into_iter().find_map(|(id, st)| {
            let hit = st.pending_txs.iter().any(|p| p.client_ref.as_deref() == Some(client_ref))
                || st.tombstones.iter().any(|t| t.client_ref == client_ref);
            hit.then_some(id)
        })
    }

    /// `client_ref`s of unresolved charges across every card this till has seen.
    pub fn pending_refs(&self) -> Vec<String> {
        self.card_files()
            .into_iter()
            .flat_map(|(_, st)| st.pending_txs.into_iter().filter_map(|p| p.client_ref))
            .collect()
    }

    fn card_files(&self) -> Vec<(String, CardState)> {
        let Ok(dir) = std::fs::read_dir(&self.cards_dir) else { return vec![] };
        dir.filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let id = name.strip_suffix(".json")?.to_owned();
                Some((id, StateStore::open(e.path()).ok()?.state))
            })
            .collect()
    }
}

/// What a delivery attempt of one outbox entry came to.
enum Sent {
    /// The registry has it.
    Done,
    /// Not ready to send yet (a load whose notes are still pending); kept.
    Later,
    /// The registry refused it for good; dropped.
    Dropped(String),
}

impl CardWallet {
    /// GET the card's state, and remember its version and slot keys.
    async fn ops_fetch(&mut self) -> Result<RemoteState> {
        let link = self.ops.as_mut().context("no ops link")?;
        let remote = link.client.state(&link.short_id).await?;
        link.version = remote.version;
        link.remote_slots = remote.slots.iter().map(|s| (s.slot, s.pubkey.clone())).collect();
        link.unkeyed = remote.unkeyed_slots.clone();
        link.open = remote.open_txs.iter().map(|t| t.txid.clone()).collect();
        link.releases = remote.releases.clone();
        Ok(remote)
    }

    /// Send a 3.0 card the registry's releases (docs/OFFLINE-PROTOCOL.md § 6) that still fit it:
    /// a slot SPENT on that txid, or CHANGE owed by it. A released input slot is NOTE again (ops
    /// recorded its note live with the rejection) and a released change slot is a free KEYED one
    /// (ops never issued its note), so the working copy from ops already reads them so and the
    /// card is brought in line with it. Answers how many were released; never fails the tap: a
    /// release that does not take is logged and comes again with the card's next state.
    pub fn card_releases(&mut self) -> usize {
        let Some((releases, short_id)) = self.ops.as_ref().map(|l| (l.releases.clone(), l.short_id.clone())) else { return 0 };
        if releases.is_empty() || !self.card_sign_once() {
            return 0;
        }
        let st = match on_card(|| self.card.get_state()) {
            Ok(st) => st,
            Err(e) => {
                tracing::warn!("releases for card {short_id}: GET_STATE: {e:#}");
                return 0;
            }
        };
        if !st.registry_key {
            tracing::warn!("card {short_id} has no registry key (set at provisioning); {} release(s) not sent", releases.len());
            return 0;
        }
        let mut n = 0;
        for r in releases {
            let (Some(txid), Ok(sig)) = (hex::decode(&r.txid).ok().and_then(|t| <[u8; 32]>::try_from(t).ok()), hex::decode(&r.sig)) else {
                tracing::warn!("release for slot {} of card {short_id}: not hex", r.slot);
                continue;
            };
            if !matches!(st.states.get(r.slot as usize), Some(SlotState::Spent | SlotState::Change)) {
                continue;
            }
            let fits = match on_card(|| self.card.get_slot(r.slot)) {
                Ok(info) => match info.state {
                    SlotState::Spent => info.spent_hash == txid,
                    SlotState::Change => info.opening.txid == txid,
                    _ => false,
                },
                Err(e) => {
                    tracing::warn!("release for slot {} of card {short_id}: GET_SLOT: {e:#}", r.slot);
                    continue;
                }
            };
            if !fits {
                continue;
            }
            match on_card(|| self.card.release(r.slot, &txid, &sig)) {
                Ok(now) => {
                    tracing::info!("card {short_id}: slot {} released from {} ({now:?})", r.slot, r.txid);
                    n += 1;
                }
                Err(e) => tracing::warn!("card {short_id}: RELEASE of slot {} for {}: {e:#}", r.slot, r.txid),
            }
        }
        n
    }

    /// Deliver what this till owes ops, then replace the card side of the local file with what
    /// ops holds. Fails if ops cannot be reached: a stale copy is never charged against.
    pub async fn ops_load(&mut self) -> Result<()> {
        let mut remote = self.ops_fetch().await?;
        if !self.store.state.ops_outbox.is_empty() {
            self.ops_push().await?;
            remote = self.ops_fetch().await?;
        }
        merge(&mut self.store.state, &remote);
        self.store.save()
    }

    /// Deliver the outbox at the end of a job. Failures stay queued for the next job with this
    /// card and are only logged.
    pub async fn ops_sync(&mut self) {
        if self.ops.is_none() {
            return;
        }
        if let Err(e) = self.ops_push().await {
            tracing::warn!("ops: writes for this card are queued for its next job: {e:#}");
        }
    }

    /// Deliver the outbox in order. A transport failure stops it (and is returned); an entry the
    /// registry refuses for good is dropped with a warning.
    pub async fn ops_push(&mut self) -> Result<()> {
        if self.ops.is_none() {
            return Ok(());
        }
        let mut i = 0;
        while i < self.store.state.ops_outbox.len() {
            let w = self.store.state.ops_outbox[i].clone();
            match self.ops_send(&w).await {
                Ok(Sent::Later) => i += 1,
                Ok(Sent::Done) => {
                    self.store.state.ops_outbox.remove(i);
                    self.store.save()?;
                }
                Ok(Sent::Dropped(why)) => {
                    tracing::warn!("ops: dropping a write the registry will not take: {why}");
                    self.store.state.ops_outbox.remove(i);
                    self.store.save()?;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    async fn ops_send(&mut self, w: &OpsWrite) -> Result<Sent, OpsError> {
        let link = self.ops.clone().expect("checked by the caller");
        let id = link.short_id.as_str();
        let r = match w {
            OpsWrite::Outcome { txid, accepted, change } => {
                let change: Vec<Value> = change.iter().map(|(n, s)| json!({"nonce": n, "signature": s})).collect();
                let body = json!({"txid": txid, "accepted": accepted, "change": change});
                match link.client.outcome(id, &body).await {
                    Ok(v) => Ok(v),
                    // No such charge (its intent never reached ops) or resolved otherwise by
                    // another till: ops is right, and the next load brings this file in line.
                    Err(OpsError::Changed(m)) | Err(OpsError::UnknownCard(m)) => {
                        return Ok(Sent::Dropped(format!("outcome of {txid}: {m}")));
                    }
                    Err(e) => Err(e),
                }
            }
            OpsWrite::Slot { slot, old_pubkey, pubkey } => {
                let body = json!({"version": link.version, "old_pubkey": old_pubkey, "pubkey": pubkey});
                match link.client.renew_slot(id, *slot, &body).await {
                    Ok(v) => Ok(v),
                    Err(OpsError::Changed(m)) => {
                        // Kept while ops still holds the old key (the spent note's outcome may not
                        // have landed yet); dropped once ops holds another one.
                        let remote = link.remote_slots.iter().find(|(s, _)| s == slot).map(|(_, k)| k.as_str());
                        if remote == Some(old_pubkey.as_str()) {
                            tracing::warn!("ops: renewal of slot {slot} not taken yet: {m}");
                            return Ok(Sent::Later);
                        }
                        return Ok(Sent::Dropped(format!("renewal of slot {slot}: {m}")));
                    }
                    Err(e) => Err(e),
                }
            }
            OpsWrite::Load { txid, client_ref, notes } => {
                let local: Vec<&NoteRec> = notes
                    .iter()
                    .filter_map(|l| self.store.state.notes.iter().find(|n| n.nonce == l.nonce))
                    .collect();
                if local.len() != notes.len() || local.iter().any(|n| n.status == NoteStatus::Dead) {
                    return Ok(Sent::Dropped(format!("load {txid} never became money")));
                }
                if local.iter().any(|n| n.status != NoteStatus::Live || n.signature.is_none()) {
                    return Ok(Sent::Later);
                }
                let change: Vec<Value> = notes
                    .iter()
                    .map(|l: &LoadNote| {
                        json!({"slot": l.slot, "amount_msat": l.amount_msat, "nonce": l.nonce,
                               "blinding_key": l.blinding_key, "out_idx": l.out_idx})
                    })
                    .collect();
                let intent = json!({"version": link.version, "txid": txid, "client_ref": client_ref,
                                    "inputs": [], "change": change});
                match link.client.intent(id, &intent).await {
                    Ok(v) => self.ops.as_mut().expect("set").version = v,
                    Err(OpsError::Changed(m)) => {
                        tracing::warn!("ops: load {txid} not taken yet: {m}");
                        return Ok(Sent::Later);
                    }
                    Err(e) => return Err(e),
                }
                let sigs: Vec<Value> = local
                    .iter()
                    .map(|n| json!({"nonce": n.nonce, "signature": n.signature.as_deref().unwrap_or_default()}))
                    .collect();
                link.client.outcome(id, &json!({"txid": txid, "accepted": true, "change": sigs})).await
            }
        };
        let v = r?;
        self.ops.as_mut().expect("set").version = v;
        Ok(Sent::Done)
    }

    /// Queue `w`, replacing an earlier entry for the same transaction or slot.
    pub(crate) fn ops_enqueue(&mut self, w: OpsWrite) {
        let Some(link) = &self.ops else { return };
        if let OpsWrite::Outcome { txid, .. } = &w {
            if !link.open.contains(txid) {
                return;
            }
        }
        let same = |a: &OpsWrite| match (a, &w) {
            (OpsWrite::Outcome { txid: x, .. }, OpsWrite::Outcome { txid: y, .. }) => x == y,
            (OpsWrite::Load { txid: x, .. }, OpsWrite::Load { txid: y, .. }) => x == y,
            _ => false,
        };
        self.store.state.ops_outbox.retain(|a| !same(a));
        self.store.state.ops_outbox.push(w);
    }

    /// Record in ops, before the card signs, the charge `txid`: the unsigned transaction, the
    /// card notes it spends, and the card change notes with their blinding keys.
    pub(crate) async fn ops_intent(&mut self, txid: &str, client_ref: Option<&str>, tx_hex: &str, inputs: &[String], change: Vec<Value>) -> Result<()> {
        let Some(link) = self.ops.as_mut() else { return Ok(()) };
        // Open from here on as far as this till knows: if the answer is lost, the rejection that
        // follows still goes to ops.
        link.open.push(txid.to_owned());
        let link = link.clone();
        let body = json!({
            "version": link.version,
            "txid": txid,
            // The registry needs one; a charge without the app's ref is named by its txid.
            "client_ref": client_ref.unwrap_or(txid),
            "tx_hex": tx_hex,
            "inputs": inputs,
            "change": change,
        });
        let v = link.client.intent(&link.short_id, &body).await.context("recording the charge in ops before the card signs")?;
        self.ops.as_mut().expect("set").version = v;
        Ok(())
    }

    /// Hand ops the signed bytes. Only after this may the till send them.
    pub(crate) async fn ops_submitted(&mut self, txid: &str, tx_hex: &str) -> Result<()> {
        let Some(link) = self.ops.clone() else { return Ok(()) };
        let v = link.client.submitted(&link.short_id, txid, tx_hex).await.context("handing ops the signed charge")?;
        self.ops.as_mut().expect("set").version = v;
        Ok(())
    }

    /// The charge `txid` is decided accepted: once every card change note of it is live, queue the
    /// outcome with their signatures.
    pub(crate) fn ops_accepted(&mut self, txid: &str) {
        if self.ops.is_none() {
            return;
        }
        let st = &self.store.state;
        // The charge's own inputs are not its outputs, though the registry lists an offline
        // charge's inputs as pending under its txid.
        let inputs: Vec<&String> = st.pending_txs.iter().filter(|p| p.txid == txid).flat_map(|p| &p.inputs).collect();
        let mine = st.notes.iter().filter(|n| n.txid.as_deref() == Some(txid) && !inputs.contains(&&n.nonce));
        if mine.clone().any(|n| n.status == NoteStatus::Pending) {
            return;
        }
        let change = mine
            .filter(|n| n.status == NoteStatus::Live)
            .filter_map(|n| Some((n.nonce.clone(), n.signature.clone()?)))
            .collect();
        self.ops_enqueue(OpsWrite::Outcome { txid: txid.to_owned(), accepted: true, change });
    }

    /// Keep a 3.0 card's spare reserve (docs/OFFLINE-PLAN.md § 10): when it has fewer than
    /// `self.spares.target` fresh keyed slots, key at most [`spares_to_key`] more in this tap.
    /// SPENT slots whose notes are settled (spent or dead in ops, confirmed spent by the
    /// federation, no charge open on them) are renewed first ([`overwrite_slot`], queued to ops
    /// like every renewal); they need the holder's PIN, so only a job that carries it renews.
    /// Then never-keyed slots, recorded in ops as split keying does ([`key_slots`]). Off ops, on a
    /// 2.x card, or with the reserve full it does nothing. The caller logs any error and goes on,
    /// except a refused PIN.
    ///
    /// [`overwrite_slot`]: CardWallet::overwrite_slot
    pub async fn card_spares(&mut self, released: usize, written: usize) -> Result<SpareReport> {
        let mut rep = SpareReport::default();
        let Some(link) = self.ops.clone() else { return Ok(rep) };
        if self.spares.target == 0 || self.spares.max_per_tap == 0 || !self.card_sign_once() {
            return Ok(rep);
        }
        let st = on_card(|| self.card.get_state()).context("GET_STATE")?;
        let on_card_is = |slot: u8, want: &[SlotState]| st.states.get(slot as usize).is_some_and(|s| want.contains(s));
        rep.fresh = self.store.state.free_slots().into_iter().filter(|s| on_card_is(*s, &[SlotState::Keyed])).count();
        let n = spares_to_key(rep.fresh, self.spares, released, written);
        if n == 0 {
            return Ok(rep);
        }

        // 1. Settled SPENT slots: their keys are burned, so renewing one costs the card nothing.
        if self.pin.is_some() {
            let open: Vec<&String> = self.store.state.pending_txs.iter().flat_map(|p| &p.inputs).collect();
            let spent: Vec<u8> = self
                .store
                .state
                .overwritable_slots()
                .into_iter()
                .filter(|s| self.may_renew(*s) && on_card_is(*s, &[SlotState::Spent, SlotState::Keyed]))
                .filter(|s| self.store.state.notes_on_current_key(*s).all(|n| !open.contains(&&n.nonce)))
                .collect();
            if !spent.is_empty() {
                self.verify_pin_session()?;
                for slot in spent {
                    if rep.renewed.len() >= n {
                        break;
                    }
                    match self.renewable(slot).await {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(e) => {
                            tracing::warn!("spare reserve: slot {slot}: {e:#}");
                            continue;
                        }
                    }
                    match self.overwrite_slot(slot) {
                        Ok(_) => rep.renewed.push(slot),
                        Err(e) => tracing::warn!("spare reserve: could not renew slot {slot}: {e:#}"),
                    }
                }
            }
        }

        // 2. Never-keyed slots.
        let more = n - rep.renewed.len();
        if more > 0 && !link.unkeyed.is_empty() {
            let want: Vec<u8> = link.unkeyed.iter().take(more).copied().collect();
            self.ops_key_unkeyed(&want).await?;
            rep.keyed = want;
        }
        Ok(rep)
    }

    /// Key `want` (never keyed in ops) on the card and record them in ops ([`key_slots`]), then
    /// take the keys into the link and the card's file.
    async fn ops_key_unkeyed(&mut self, want: &[u8]) -> Result<()> {
        let link = self.ops.clone().context("no ops link")?;
        let keyed = key_slots(self.card.as_mut(), &link.client, &link.short_id, want).await?;
        let l = self.ops.as_mut().expect("set");
        l.version = keyed.answer.version;
        l.unkeyed = keyed.answer.unkeyed_slots.clone();
        for (slot, pk) in &keyed.keys {
            l.remote_slots.retain(|(s, _)| s != slot);
            l.remote_slots.push((*slot, pk.clone()));
            self.store.state.set_slot_pubkey(*slot, pk.clone());
        }
        self.store.save()?;
        tracing::info!("ops: keyed slots {want:?} of card {} (version {})", link.short_id, keyed.answer.version);
        Ok(())
    }

    /// Unkeyed slots this tap may key: those ops reported, none off ops.
    pub(crate) fn keyable_slots(&self) -> usize {
        self.ops.as_ref().map_or(0, |l| l.unkeyed.len())
    }

    /// Make sure the card has `need` free keyed slots, keying unkeyed ones if it has too few
    /// ([`slots_to_key`], [`key_slots`]). Returns the slots keyed. Off ops, or with enough free
    /// slots, it does nothing; with too few even after keying it keys nothing and leaves the
    /// refusal to the caller. Ops not answering is an error: the work that needed the slots is
    /// refused rather than the keys queued.
    pub(crate) async fn ops_make_room(&mut self, need: usize) -> Result<Vec<u8>> {
        let Some(link) = self.ops.clone() else { return Ok(vec![]) };
        let free = self.store.state.free_slots().len();
        let n = slots_to_key(need, free, link.unkeyed.len());
        if n == 0 || free + n < need {
            return Ok(vec![]);
        }
        let want = link.unkeyed[..n].to_vec();
        self.ops_key_unkeyed(&want).await?;
        Ok(want)
    }

    /// Whether this card's slot may be re-keyed. On ops, slot 0's key names the card (its short
    /// id), so it is never renewed: a renewed slot 0 would make the card unknown to every till.
    pub(crate) fn may_renew(&self, slot: u8) -> bool {
        self.ops.is_none() || slot != 0
    }
}

/// Replace the card side of `st` with `remote`, keeping what only this till knows: its own
/// outputs, its unsent writes and the history of keys ops no longer reports.
pub fn merge(st: &mut CardState, remote: &RemoteState) {
    // Slots: ops's keys, with this till's renewals that ops has not taken yet laid over them.
    st.slots = remote.slots.iter().map(|s| SlotRec { slot: s.slot, pubkey: s.pubkey.clone(), unknown: false }).collect();
    st.slots.sort_by_key(|s| s.slot);
    for w in &st.ops_outbox {
        if let OpsWrite::Slot { slot, old_pubkey, pubkey } = w {
            if let Some(s) = st.slots.iter_mut().find(|s| s.slot == *slot && s.pubkey == *old_pubkey) {
                s.pubkey = pubkey.clone();
            }
        }
    }

    // Notes: ops's, plus spent and dead history it does not report, plus the notes of a load
    // this till has not recorded in ops yet.
    let theirs: HashSet<&str> = remote.notes.iter().map(|n| n.nonce.as_str()).collect();
    let loading: HashSet<String> = st
        .ops_outbox
        .iter()
        .filter_map(|w| match w {
            OpsWrite::Load { notes, .. } => Some(notes.iter().map(|n| n.nonce.clone())),
            _ => None,
        })
        .flatten()
        .collect();
    let kept: Vec<NoteRec> = st
        .notes
        .drain(..)
        .filter(|n| !theirs.contains(n.nonce.as_str()))
        .filter(|n| matches!(n.status, NoteStatus::Spent | NoteStatus::Dead) || loading.contains(&n.nonce))
        .collect();
    st.notes = remote
        .notes
        .iter()
        .map(|n| NoteRec {
            slot: n.slot,
            amount_msat: n.amount_msat,
            nonce: n.nonce.clone(),
            signature: n.signature.clone(),
            status: n.status,
            blinding_key: n.blinding_key.clone(),
            txid: n.txid.clone(),
            out_idx: n.out_idx,
        })
        .chain(kept)
        .collect();

    // Open charges: each becomes a `PendingTx`, so `resolve_pending` finishes it. Without signed
    // bytes it is abandoned (recorded rejected); with them it is resubmitted and recorded as the
    // federation answers. A load (no inputs) is its own till's to record.
    for o in remote.open_txs.iter().filter(|o| !o.inputs.is_empty()) {
        match st.pending_txs.iter_mut().find(|p| p.txid == o.txid) {
            Some(p) => {
                // Ops has the signed bytes, so anyone may have sent them: never abandon it.
                if let (false, Some(signed)) = (p.submitted, &o.signed_hex) {
                    p.submitted = true;
                    p.ops_unconfirmed = false;
                    p.tx_hex = Some(signed.clone());
                }
            }
            None => st.pending_txs.push(PendingTx {
                txid: o.txid.clone(),
                tx_hex: o.signed_hex.clone(),
                submitted: o.signed_hex.is_some(),
                accepted: false,
                inputs: o.inputs.clone(),
                // Another till's charge (or this till's, from a lost file): never a sale here.
                client_ref: None,
                ..Default::default()
            }),
        }
    }
}

/// Whether `err` is the card refusing the holder's PIN (`63 CX`, or blocked).
pub fn pin_refused(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|c| c.downcast_ref::<fedimint_card_host_card::StatusWord>())
        .any(|s| matches!(s.0, 0x63C0..=0x63CF | fedimint_card_host_card::SW_PIN_BLOCKED))
}

/// Whether ops gave no definitive answer (unreachable, timed out, 5xx): the write may or may not
/// have landed.
pub fn unanswered(err: &anyhow::Error) -> bool {
    matches!(ops_error(err), Some(OpsError::Unreachable(_)))
}

/// The ops error in `err`'s cause chain, if any.
pub fn ops_error(err: &anyhow::Error) -> Option<&OpsError> {
    err.chain().find_map(|c| c.downcast_ref::<OpsError>())
}

/// Report the federation's name and mint keys to ops at `serve` startup. A failure is a warning
/// only, but until some till of the operator has reported the keys, ops refuses every outcome
/// that makes a note live.
pub async fn report_federation(client: &OpsClient, report: &FederationReport) {
    if let Err(e) = client.federation(report).await {
        tracing::warn!("ops: could not report the federation: {e}");
        eprintln!("warning: could not report the federation's name and mint keys to ops: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fedimint_core::secp256k1::XOnlyPublicKey;

    fn note(nonce: &str, slot: u8, status: NoteStatus) -> NoteRec {
        NoteRec { slot, amount_msat: 1024, nonce: nonce.into(), signature: None, status, blinding_key: None, txid: None, out_idx: None }
    }

    fn remote(notes: Vec<RemoteNote>, open: Vec<RemoteTx>) -> RemoteState {
        RemoteState {
            version: 3,
            slots: (0..2).map(|s| RemoteSlot { slot: s, pubkey: format!("{s:02x}").repeat(32) }).collect(),
            unkeyed_slots: vec![],
            notes,
            open_txs: open,
            releases: vec![],
        }
    }

    #[test]
    fn the_spare_reserve_keys_the_shortfall_within_the_taps_budget() {
        let cfg = SpareConfig::default();
        assert_eq!((cfg.target, cfg.max_per_tap), (12, 2));
        assert_eq!(spares_to_key(12, cfg, 0, 0), 0, "full");
        assert_eq!(spares_to_key(15, cfg, 0, 0), 0);
        assert_eq!(spares_to_key(11, cfg, 0, 0), 1, "the shortfall");
        assert_eq!(spares_to_key(0, cfg, 0, 0), 2, "never more than the cap");
        // A tap busy with a release or a few notes has room for one key; with more, none.
        assert_eq!(spares_to_key(0, cfg, 1, 0), 1);
        assert_eq!(spares_to_key(0, cfg, 0, 3), 1);
        assert_eq!(spares_to_key(0, cfg, 4, 0), 0);
        assert_eq!(spares_to_key(0, cfg, 0, 16), 0);
        assert_eq!(spares_to_key(0, SpareConfig { target: 0, max_per_tap: 2 }, 0, 0), 0, "off");
        assert_eq!(spares_to_key(0, SpareConfig { target: 6, max_per_tap: 0 }, 0, 0), 0, "off");
    }

    #[test]
    fn unkeyed_slots_parse_and_default_to_none_from_an_older_registry() {
        let old: RemoteState = serde_json::from_value(json!({"version": 1, "slots": [], "notes": [], "open_txs": []})).unwrap();
        assert!(old.unkeyed_slots.is_empty());
        let new: RemoteState =
            serde_json::from_value(json!({"version": 1, "slot_count": 32, "slots": [], "unkeyed_slots": [8, 9], "notes": [], "open_txs": []}))
                .unwrap();
        assert_eq!(new.unkeyed_slots, vec![8, 9]);
        let free: FreeSlots = serde_json::from_value(json!({"version": 1, "federation_id": "f", "free": []})).unwrap();
        assert!(free.unkeyed_slots.is_empty());
    }

    #[test]
    fn releases_parse_and_default_to_none_from_an_older_registry() {
        let old: RemoteState = serde_json::from_value(json!({"version": 1, "slots": [], "notes": [], "open_txs": []})).unwrap();
        assert!(old.releases.is_empty());
        let new: RemoteState = serde_json::from_value(
            json!({"version": 1, "slots": [], "notes": [], "open_txs": [], "releases": [{"slot": 3, "txid": "ab", "sig": "30"}]}),
        )
        .unwrap();
        assert_eq!((new.releases[0].slot, new.releases[0].txid.as_str(), new.releases[0].sig.as_str()), (3, "ab", "30"));
    }

    #[test]
    fn merge_takes_the_card_from_ops_and_keeps_the_tills_part() {
        let mut st = CardState::default();
        st.notes.push(note("02aa", 0, NoteStatus::Live)); // ops says spent now: gone from live
        st.notes.push(note("02bb", 1, NoteStatus::Spent)); // history ops does not report
        st.notes.push(note("02cc", 1, NoteStatus::Pending)); // stale pending: dropped
        st.ops_outbox.push(OpsWrite::Slot { slot: 1, old_pubkey: "01".repeat(32), pubkey: "09".repeat(32) });
        let r = remote(
            vec![RemoteNote {
                slot: 0,
                amount_msat: 1024,
                nonce: "02aa".into(),
                signature: None,
                blinding_key: None,
                status: NoteStatus::Spent,
                txid: None,
                out_idx: None,
            }],
            vec![
                RemoteTx { txid: "t1".into(), till_id: 2, client_ref: "r".into(), signed_hex: Some("ff".into()), inputs: vec!["02dd".into()] },
                RemoteTx { txid: "t2".into(), till_id: 2, client_ref: "r".into(), signed_hex: None, inputs: vec!["02ee".into()] },
                RemoteTx { txid: "load".into(), till_id: 2, client_ref: "r".into(), signed_hex: None, inputs: vec![] },
            ],
        );
        merge(&mut st, &r);
        assert_eq!(st.slot_pubkey(0), Some("00".repeat(32).as_str()));
        assert_eq!(st.slot_pubkey(1), Some("09".repeat(32).as_str()), "an unsent renewal stays laid over");
        let nonces: Vec<(&str, NoteStatus)> = st.notes.iter().map(|n| (n.nonce.as_str(), n.status)).collect();
        assert_eq!(nonces, vec![("02aa", NoteStatus::Spent), ("02bb", NoteStatus::Spent)]);
        assert_eq!(st.pending_txs.len(), 2, "a load is not a charge to resolve");
        let t1 = &st.pending_txs[0];
        assert!(t1.submitted && t1.tx_hex.as_deref() == Some("ff") && t1.client_ref.is_none());
        assert!(!st.pending_txs[1].submitted && st.pending_txs[1].tx_hex.is_none());
    }

    #[test]
    fn signed_bytes_in_ops_make_a_local_charge_submitted() {
        let mut st = CardState::default();
        st.pending_txs.push(PendingTx { txid: "t".into(), client_ref: Some("mine".into()), ..Default::default() });
        merge(&mut st, &remote(vec![], vec![RemoteTx { txid: "t".into(), till_id: 1, client_ref: "mine".into(), signed_hex: Some("aa".into()), inputs: vec!["02".into()] }]));
        assert_eq!(st.pending_txs.len(), 1);
        assert!(st.pending_txs[0].submitted);
        assert_eq!(st.pending_txs[0].client_ref.as_deref(), Some("mine"), "this till's own sale stays its own");
    }

    /// The registry checks `blinding_key` as 32 bytes and `signature` as 48: the consensus
    /// encodings this crate stores (`glue.rs` `enc`) have exactly those lengths, no length prefix.
    #[test]
    fn stored_encodings_match_the_registry_checks() {
        use fedimint_core::encoding::Encodable;
        assert_eq!(tbs::BlindingKey::random().consensus_encode_to_hex().len(), 64);
        // Any G1 point encodes like a note's signature does.
        let sig = tbs::Signature(tbs::Message::from_bytes(b"note").0);
        assert_eq!(sig.consensus_encode_to_hex().len(), 96);
    }

    #[test]
    fn signed_bytes_in_ops_settle_an_unanswered_submitted() {
        let mut st = CardState::default();
        st.pending_txs.push(PendingTx { txid: "t".into(), tx_hex: Some("aa".into()), ops_unconfirmed: true, ..Default::default() });
        merge(&mut st, &remote(vec![], vec![RemoteTx { txid: "t".into(), till_id: 1, client_ref: "r".into(), signed_hex: Some("aa".into()), inputs: vec!["02".into()] }]));
        assert!(st.pending_txs[0].submitted && !st.pending_txs[0].ops_unconfirmed);
    }

    #[test]
    fn the_token_is_never_debug_printed() {
        let c = OpsConfig { url: "http://x".into(), token: "till_secret".into() };
        assert!(!format!("{c:?}").contains("till_secret"));
    }

    #[test]
    fn short_id_is_the_last_four_bytes_of_slot_0() {
        let pk: XOnlyPublicKey = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798".parse().unwrap();
        assert_eq!(short_id_of(&pk), "16f81798");
    }

    #[test]
    fn missing_load_notes_drop_from_merge_only_once_recorded() {
        let mut st = CardState::default();
        st.notes.push(note("02ab", 2, NoteStatus::Live));
        st.ops_outbox.push(OpsWrite::Load {
            txid: "t".into(),
            client_ref: "c".into(),
            notes: vec![LoadNote { slot: 2, amount_msat: 1024, nonce: "02ab".into(), blinding_key: "00".repeat(32), out_idx: 0 }],
        });
        merge(&mut st, &remote(vec![], vec![]));
        assert_eq!(st.notes.len(), 1, "a load ops has not taken yet keeps its notes");
        st.ops_outbox.clear();
        merge(&mut st, &remote(vec![], vec![]));
        assert!(st.notes.is_empty());
    }
}
