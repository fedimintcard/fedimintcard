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
use fedimint_card_host_card::{Card, SW_NO_SPACE, SlotState, is_change_slot, status_word};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::glue::{CardWallet, on_card};
use crate::state::{
    CardState, Held, HeldKind, LoadNote, NoteRec, NoteStatus, OpsWrite, PendingTx, SlotRec, StateStore, nonce_matches_pubkey,
};

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
/// What the reserve's budget counts, per card command (J3R180: a `GEN_KEY` measured at about
/// 316 ms, a `RELEASE` checks a P-256 signature in about 75 ms, a `SET_NOTE` measured at 136 ms).
const GEN_KEY_MS: u64 = 320;
const RELEASE_MS: u64 = 100;
const SET_NOTE_MS: u64 = 136;
/// Change slots (24 to 31) a tap keys while fewer than this many are KEYED and free, whatever
/// the tap already wrote: a card's first tap leaves it able to take change offline.
pub const MIN_CHANGE_SPARES: usize = 2;

/// Most slots one tap reads (`GET_SLOT`) to tell whether they are stranded
/// ([`CardWallet::card_strands`]); the rest are held on doubt and read at a later tap.
pub const MAX_STRAND_CHECKS: usize = 8;
/// One bound on the federation's answers about the due spent notes a tap reports.
const STRAND_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
/// Most rows one `POST /stranded` takes: every slot of the largest card.
const MAX_STRANDED_ROWS: usize = 32;

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

/// Whether a tap that has already keyed or renewed `taken` for the reserve takes `slot` too:
/// any slot while fewer than `n` ([`spares_to_key`]) are taken, but a slot below 24 only if what
/// is left of `n` still leaves room for the `floor` change slots (24 to 31) the tap owes; a
/// change slot also while fewer than `floor` change slots are taken.
pub fn spare_may_take(slot: u8, taken: &[u8], n: usize, floor: usize) -> bool {
    let change = taken.iter().filter(|s| is_change_slot(**s)).count();
    if is_change_slot(slot) { taken.len() < n || change < floor } else { taken.len() + floor.saturating_sub(change) < n }
}

/// `slots` with the change slots (24 to 31) first, each part lowest first: the order the reserve
/// keys and renews in.
fn change_slots_first(mut slots: Vec<u8>) -> Vec<u8> {
    slots.sort_by_key(|s| (!is_change_slot(*s), *s));
    slots
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
    #[serde(default)]
    pub federation_id: Option<String>,
    #[serde(default)]
    pub mint_version: crate::wallet::MintVersion,
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
    /// The card's stranded slots (docs/OFFLINE-PROTOCOL.md § 7). Empty from a registry older than
    /// them.
    #[serde(default)]
    pub stranded: Vec<RemoteStranded>,
}

impl RemoteState {
    fn belongs_to(&self, federation: &str, mint: crate::wallet::MintVersion) -> bool {
        self.mint_version == mint
            && match &self.federation_id {
                Some(id) => id == federation,
                // Old 4.0 registries answered v1 card state without its federation id.
                None => mint == crate::wallet::MintVersion::V1,
            }
    }
}

/// A slot the registry holds as stranded: a till reported it SPENT, or owed CHANGE, for `txid`,
/// a transaction the registry never saw.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteStranded {
    pub slot: u8,
    /// `spent` or `change` ([`HeldKind::as_str`]); kept as text, so a kind this till does not know
    /// matches nothing rather than failing the card's state.
    pub kind: String,
    /// 32 bytes, hex.
    pub txid: String,
    /// The note's amount for a spent slot; none for a change slot.
    #[serde(default)]
    pub amount_msat: Option<u64>,
    /// When the registry first saw it, and when it may release it (ms, by its clock).
    #[serde(default)]
    pub since: u64,
    #[serde(default)]
    pub release_after: u64,
    /// Past the grace: a report with the federation's word may release it now.
    #[serde(default)]
    pub due: bool,
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
    /// The registry's signature over the note (DER hex), once it signed it. Absent from a
    /// registry older than note signatures.
    #[serde(default)]
    pub reg_sig: Option<String>,
}

/// A note the registry signed, as an accepted outcome's answer lists it (`notes[]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SignedNote {
    pub slot: u8,
    /// The note's nonce, hex, as the till recorded it.
    pub nonce: String,
    /// DER ECDSA P-256, hex.
    pub reg_sig: String,
}

/// What an outcome call answered: the card's new version, and the notes the registry signed in
/// it (none from a registry older than note signatures, or one that could not sign yet).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutcomeAnswer {
    pub version: u64,
    pub notes: Vec<SignedNote>,
}

impl OutcomeAnswer {
    fn of(v: &Value) -> Result<Self, OpsError> {
        let version = OpsClient::version(v)?;
        // Entries without a signature are skipped: such a note waits for a later answer.
        #[derive(Deserialize)]
        struct Maybe {
            slot: u8,
            nonce: String,
            reg_sig: Option<String>,
        }
        // So is an entry that does not parse: the outcome is recorded all the same, and only the
        // version decides whether the answer stands.
        let notes = match v.get("notes") {
            None | Some(Value::Null) => vec![],
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|e| match serde_json::from_value::<Maybe>(e.clone()) {
                    Ok(m) => Some(m),
                    Err(err) => {
                        tracing::warn!("ops: an entry of the outcome's notes did not parse and is skipped: {err}");
                        None
                    }
                })
                .filter_map(|m| Some(SignedNote { slot: m.slot, nonce: m.nonce, reg_sig: m.reg_sig? }))
                .collect(),
            Some(_) => {
                tracing::warn!("ops: the outcome's notes are not a list and are skipped");
                vec![]
            }
        };
        Ok(Self { version, notes })
    }
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
    registered_v2: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    http: reqwest::Client,
    url: String,
    token: String,
}

impl OpsClient {
    pub fn new(cfg: &OpsConfig) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(OPS_TIMEOUT).build().context("building the ops HTTP client")?;
        Ok(Self { registered_v2: Default::default(), http, url: cfg.url.trim_end_matches('/').to_owned(), token: cfg.token.clone() })
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

    /// `POST /cards/:id/outcome`: the new version and, when accepted, the notes the registry signed.
    pub async fn outcome(&self, short_id: &str, body: &Value) -> Result<OutcomeAnswer, OpsError> {
        OutcomeAnswer::of(&self.call(reqwest::Method::POST, &format!("/cards/{short_id}/outcome"), Some(body)).await?)
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

    /// `POST /cards/:id/stranded` (docs/OFFLINE-PROTOCOL.md § 7): the slots a tap held
    /// ([`CardWallet::ops_report_strands`]). A registry older than strandings answers 404.
    pub async fn stranded(&self, short_id: &str, body: &Value) -> Result<Value, OpsError> {
        self.call(reqwest::Method::POST, &format!("/cards/{short_id}/stranded"), Some(body)).await
    }

    /// `POST /federations`: what a till read from the federation's config ([`FederationReport`]).
    pub async fn federation(&self, report: &FederationReport) -> Result<(), OpsError> {
        let mut body = json!({"federation_id": report.federation_id, "tbs_pks": report.tbs_pks});
        if let Some(name) = report.name.as_deref().filter(|n| !n.trim().is_empty()) {
            body["name"] = json!(name);
        }
        let path = match report.mint_version {
            crate::wallet::MintVersion::V1 => "/federations",
            crate::wallet::MintVersion::V2 => {
                body["mint_version"] = json!(report.mint_version);
                "/federations/mintv2"
            }
        };
        let answer = self.call(reqwest::Method::POST, path, Some(&body)).await?;
        if report.mint_version == crate::wallet::MintVersion::V2 {
            if answer["ok"].as_bool() != Some(true)
                || answer["mint_version"].as_str() != Some("v2")
                || answer["federation_id"].as_str() != Some(&report.federation_id)
            {
                return Err(OpsError::Refused(422, "the registry did not confirm this federation’s mint v2 keys".into()));
            }
            *self.registered_v2.lock().unwrap() = Some(report.federation_id.clone());
        }
        Ok(())
    }

    /// `POST /cards/:id/slot-keys` (till): the first keys of never-keyed slots, all or nothing.
    /// The same keys again replay without change.
    pub async fn slot_keys(&self, short_id: &str, keys: &[(u8, String)]) -> Result<SlotKeysAnswer, OpsError> {
        let body = json!({"keys": keys.iter().map(|(slot, pubkey)| json!({"slot": slot, "pubkey": pubkey})).collect::<Vec<_>>()});
        let v = self.call(reqwest::Method::POST, &format!("/cards/{short_id}/slot-keys"), Some(&body)).await?;
        serde_json::from_value(v).map_err(|e| OpsError::Unreachable(format!("the slot-keys answer did not parse: {e}")))
    }

    /// `GET /cards/:id/slots` (holder): the card's free slot keys, its version and federation,
    /// with every slot from 24 up left out ([`FreeSlots::for_loads`]).
    pub async fn free_slots(&self, short_id: &str) -> Result<FreeSlots, OpsError> {
        let v = self.call(reqwest::Method::GET, &format!("/cards/{short_id}/slots"), None).await?;
        serde_json::from_value::<FreeSlots>(v)
            .map(FreeSlots::for_loads)
            .map_err(|e| OpsError::Unreachable(format!("the free slots did not parse: {e}")))
    }

    /// `POST /cards/:id/load` (holder): the notes, pending, before the wallet submits.
    pub async fn load(&self, short_id: &str, body: &Value) -> Result<u64, OpsError> {
        Self::version(&self.call(reqwest::Method::POST, &format!("/cards/{short_id}/load"), Some(body)).await?)
    }

    /// `POST /cards/:id/load/outcome` (holder): the signatures, or `{accepted: false}`.
    pub async fn load_outcome(&self, short_id: &str, body: &Value) -> Result<OutcomeAnswer, OpsError> {
        OutcomeAnswer::of(&self.call(reqwest::Method::POST, &format!("/cards/{short_id}/load/outcome"), Some(body)).await?)
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
    #[serde(default)]
    pub mint_version: crate::wallet::MintVersion,
    pub version: u64,
    pub federation_id: String,
    /// Keyed slots whose key never carried a note.
    pub free: Vec<RemoteSlot>,
    /// Slots with no key on record yet, ascending: a till keys them at a tap ([`key_slots`]).
    #[serde(default)]
    pub unkeyed_slots: Vec<u8>,
}

impl FreeSlots {
    /// Only the slots a load may take: slots 24 and up hold the change of offline sales and
    /// nothing else, so they are dropped from both lists, whatever the registry listed.
    pub fn for_loads(mut self) -> Self {
        self.free.retain(|s| !is_change_slot(s.slot));
        self.unkeyed_slots.retain(|s| !is_change_slot(*s));
        self
    }
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
    pub mint_version: crate::wallet::MintVersion,
    pub federation_id: String,
    pub name: Option<String>,
    /// Tier in msat (decimal) to its `tbs::AggregatePublicKey`, 96 bytes compressed G2, hex.
    pub tbs_pks: std::collections::BTreeMap<String, String>,
}

impl FederationReport {
    /// Read from the wallet's copy of the federation's client config.
    pub async fn of(wallet: &crate::wallet::Wallet) -> Self {
        use fedimint_core::encoding::Encodable as _;
        let name = wallet.federation_name().await;
        let tbs_pks = wallet.mint_cfg.tbs_pks.iter().map(|(amount, pk)| (amount.msats.to_string(), pk.consensus_encode_to_hex())).collect();
        Self { mint_version: wallet.mint_version, federation_id: wallet.federation_id().to_string(), name, tbs_pks }
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
    /// The card's stranded slots as ops last answered them ([`CardWallet::card_strands`]).
    pub stranded: Vec<RemoteStranded>,
    /// The registry's certificate key this till keeps (hex SEC1, `till-ops.json`): the key a 4.0
    /// card checks the registry's note signatures under. `None` when the till has none yet.
    pub cert_key: Option<String>,
    /// Dead change slots this job releases and re-keys once the PIN is checked
    /// ([`CardWallet::card_dead_change`]), as [`CardWallet::card_releases`] found them.
    pub dead_change: Vec<DeadChange>,
}

/// A change slot ([`CardState::dead_change_slot`]) left for [`CardWallet::card_dead_change`]:
/// its release (txid, signature) when the card reads it CHANGE, `None` when it already reads
/// KEYED on the burned key (a tap cut between the two steps).
///
/// [`CardState::dead_change_slot`]: crate::state::CardState::dead_change_slot
#[derive(Clone, Debug)]
pub struct DeadChange {
    pub slot: u8,
    pub release: Option<([u8; 32], Vec<u8>)>,
}

// Moved to the core the POS phones share (docs/POS-OFFLINE.md); kept at the old path.
pub use fedimint_card_pos_core::short_id_of;

/// The till's side of ops: the client and where the per-card files live.
#[derive(Clone)]
pub struct Ops {
    pub client: OpsClient,
    cards_dir: PathBuf,
}

/// At most this many transactions a tap asks the registry again for the note signatures it has
/// not given ([`CardWallet::ops_ask_reg_sigs`]): a slow registry costs a tap a bounded wait.
pub const MAX_SIG_ASKS_PER_TAP: usize = 4;

/// The wait before a transaction is asked again after its second fruitless asking in a row; it
/// doubles with each one after, up to [`SIG_ASK_MOST_WAIT_MS`].
pub const SIG_ASK_FIRST_WAIT_MS: u64 = 60_000;
/// The longest wait between two askings of one transaction: a day.
pub const SIG_ASK_MOST_WAIT_MS: u64 = 86_400_000;
/// A holder's load is asked at the holder's route, which spends the card's hourly load budget:
/// at most once in this long for a card, whatever the transaction.
pub const LOAD_ASK_EVERY_MS: u64 = 3_600_000;

/// How long a transaction waits before it is asked again after `misses` fruitless askings in a
/// row: none after the first (the next tap asks once more, as a key may be back by then), then
/// [`SIG_ASK_FIRST_WAIT_MS`], doubling up to [`SIG_ASK_MOST_WAIT_MS`].
pub fn sig_ask_backoff_ms(misses: u32) -> u64 {
    if misses < 2 {
        return 0;
    }
    SIG_ASK_FIRST_WAIT_MS.saturating_mul(1u64 << (misses - 2).min(20)).min(SIG_ASK_MOST_WAIT_MS)
}

/// A note still waiting for the registry's signature: live, with the federation's, without the
/// registry's.
fn waits_for_reg_sig(n: &NoteRec) -> bool {
    n.status == NoteStatus::Live && n.signature.is_some() && n.reg_sig.is_none()
}

/// The transactions a tap asks the registry again for note signatures, least recently asked
/// first, at most [`MAX_SIG_ASKS_PER_TAP`]. Each has a live note with the federation's signature
/// and not the registry's, is not open in ops (`open`) and owes no outcome or load in the outbox,
/// so a repeat never decides anything, and is past its back-off. A holder's load (asked at the
/// load route) is left out until the card's hourly load-route limit allows an asking, and at most
/// one is in a tap, so waiting loads never take the places of the till's own charges. Forgets the
/// askings of transactions that have no such note left.
pub(crate) fn sig_asks_due(st: &mut CardState, open: &[String], now_ms: u64) -> Vec<String> {
    let mut waiting: Vec<String> = vec![];
    for n in st.notes.iter().filter(|n| waits_for_reg_sig(n)) {
        if let Some(t) = &n.txid
            && !waiting.contains(t)
        {
            waiting.push(t.clone());
        }
    }
    st.sig_asks.retain(|t, _| waiting.contains(t));
    let owed =
        |t: &str| st.ops_outbox.iter().any(|w| matches!(w, OpsWrite::Outcome { txid, .. } | OpsWrite::Load { txid, .. } if txid == t));
    let load_route_free = now_ms >= st.load_ask_ms.saturating_add(LOAD_ASK_EVERY_MS);
    let mut due: Vec<(u64, bool, String)> = waiting
        .into_iter()
        .filter(|t| !open.contains(t) && !owed(t))
        .filter_map(|t| match st.sig_asks.get(&t) {
            None => Some((0, false, t)),
            Some(a) if a.load && !load_route_free => None,
            Some(a) if now_ms >= a.last_ms.saturating_add(sig_ask_backoff_ms(a.misses)) => Some((a.last_ms, a.load, t)),
            Some(_) => None,
        })
        .collect();
    due.sort_by_key(|(last, _, _)| *last);
    let mut loads = 0;
    due.retain(|(_, load, _)| {
        loads += usize::from(*load);
        !*load || loads == 1
    });
    due.into_iter().take(MAX_SIG_ASKS_PER_TAP).map(|(_, _, t)| t).collect()
}

/// The change a repeat of `txid`'s accepted outcome lists: only its notes still waiting for the
/// registry's signature. The registry takes a repeat whose notes are among those it recorded
/// (`cards.ts repeatMatches`); every note with the txid would not do, as the registry files an
/// offline charge's inputs under it too, and a spent change note of an online charge keeps it.
pub(crate) fn repeat_change(st: &CardState, txid: &str) -> Vec<Value> {
    st.notes
        .iter()
        .filter(|n| n.txid.as_deref() == Some(txid) && waits_for_reg_sig(n))
        .filter_map(|n| n.signature.as_ref().map(|s| json!({"nonce": n.nonce, "signature": s})))
        .collect()
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
        // A 4.x card that fails under the kept certificate key: the registry may have rotated it.
        self.check_cert_key(cw).await;
        // Live notes the registry has not signed yet (its key was off, or its signing failed):
        // asked for again now, so this tap can write them to the card.
        let asked = cw.ops_ask_reg_sigs().await;
        if asked > 0 {
            tracing::info!("card {}: {asked} note signature(s) from the registry on a repeat", short_id_of(&pk));
        }
        // A 3.0 card: what the registry released (before planning, so this tap can use it), the
        // slots nothing explains (held out of this job's plans, reported as it ends), its clock,
        // any live note it lacks (a load or change finished elsewhere), then its spare reserve.
        let released = cw.card_releases();
        cw.card_strands(self.cards_dir.parent());
        let written = cw.card_tick();
        match cw.card_spares(released, written).await {
            Ok(r) if !r.renewed.is_empty() || !r.keyed.is_empty() => {
                tracing::info!(
                    "card {}: spare reserve from {} fresh slot(s): renewed {:?}, keyed {:?}",
                    short_id_of(&pk),
                    r.fresh,
                    r.renewed,
                    r.keyed
                );
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
        // Dead change slots wait for the PIN (`card_dead_change`): a job that carries it checks it
        // here when nothing above did, and a refused PIN ends the tap as above.
        if cw.pin.is_some()
            && cw.ops.as_ref().is_some_and(|l| !l.dead_change.is_empty())
            && let Err(e) = cw.verify_pin_session()
        {
            if pin_refused(&e) {
                Self::finish(cw, base).await;
                return Err(e);
            }
            tracing::warn!("card {}: dead change slots not renewed this tap: {e:#}", short_id_of(&pk));
        }
        Ok(base)
    }

    /// The registry's certificate key the sealed PIN checks a 4.x card under: the one this till
    /// keeps (`cert_key` in `till-ops.json`), else asked of the registry now (`GET
    /// /registry/cert-key`, a public route: any session, the till's included, may call it) and
    /// kept. The reachability probe ([`crate::offline::Reach::probe`]) refreshes the kept copy
    /// each time it runs. `None` when the registry has none or does not answer: the PIN is then
    /// refused unsent ([`crate::glue::NoCertKey`]), never sealed to an unchecked key.
    pub async fn kept_cert_key(&self) -> Option<String> {
        let dir = self.cards_dir.parent()?;
        if let Ok(Some(k)) = crate::claim::read_cert_key(dir) {
            return Some(k);
        }
        match self.client.cert_key(crate::offline::PROBE_TIMEOUT).await {
            Ok(Some(k)) => {
                if let Err(e) = crate::claim::write_cert_key(dir, &k) {
                    tracing::warn!("keeping the registry's certificate key: {e:#}");
                }
                Some(k)
            }
            Ok(None) => {
                tracing::warn!("the registry has no certificate key: no PIN goes to a 4.0 card until it has one");
                None
            }
            Err(e) => {
                tracing::warn!("the registry's certificate key could not be fetched ({e}): no PIN goes to a 4.0 card this tap");
                None
            }
        }
    }

    /// Settle which certificate key this tap checks a 4.x card under. The key in use is
    /// `cw.cert_key`, else the ops link's, else [`kept_cert_key`](Self::kept_cert_key). When the
    /// card in the field fails [`trust_card`](fedimint_card_host_card::trust_card) under it (or the
    /// kept key is not hex), the registry this till is configured with is asked once for its key
    /// (`GET /registry/cert-key`, the same [`OpsClient`] as every other call; no other URL is
    /// asked); a key that differs is kept (`till-ops.json`, the job, the link) and the card is
    /// checked again when its PIN is opened. The same key, no key, or no answer: the old key
    /// stays, and a card that still fails is refused `card_not_genuine` (403) with no PIN sent.
    /// Runs once a tap (`cw.cert_checked`, cleared in [`finish`](Self::finish)); a card that
    /// passes, or a card older than 4.0, costs no fetch.
    pub async fn check_cert_key(&self, cw: &mut CardWallet) {
        if cw.cert_key.is_none() && cw.ops.as_ref().and_then(|l| l.cert_key.as_ref()).is_none() {
            cw.cert_key = self.kept_cert_key().await;
        }
        if cw.cert_checked {
            return;
        }
        let Some(old) = cw.cert_key.clone().or_else(|| cw.ops.as_ref().and_then(|l| l.cert_key.clone())) else { return };
        if !on_card(|| cw.card.applet_version()).is_ok_and(|v| v.is_current()) {
            return;
        }
        cw.cert_checked = true;
        if hex::decode(&old).is_ok() {
            match cw.trusted_card() {
                Err(e) if e.chain().any(|c| c.downcast_ref::<fedimint_card_host_card::UntrustedCard>().is_some()) => {}
                _ => return,
            }
        }
        match self.client.cert_key(crate::offline::PROBE_TIMEOUT).await {
            Ok(Some(k)) if k != old => {
                tracing::warn!("the registry's certificate key changed: this till keeps the new one");
                if let Some(dir) = self.cards_dir.parent()
                    && let Err(e) = crate::claim::write_cert_key(dir, &k)
                {
                    tracing::warn!("keeping the registry's certificate key: {e:#}");
                }
                if let Some(l) = cw.ops.as_mut() {
                    l.cert_key = Some(k.clone());
                }
                cw.cert_key = Some(k);
            }
            Ok(_) => tracing::warn!("a 4.0 card does not verify under the registry's certificate key: no PIN goes to it"),
            Err(e) => tracing::warn!("the registry's certificate key could not be fetched ({e}): the till keeps the key it has"),
        }
    }

    pub(crate) async fn confirm_mint(&self, wallet: &crate::wallet::Wallet) -> Result<()> {
        if wallet.mint_version == crate::wallet::MintVersion::V2
            && self.client.registered_v2.lock().unwrap().as_ref() != Some(&wallet.federation_id().to_string())
        {
            self.client
                .federation(&FederationReport::of(wallet).await)
                .await
                .context("the registry must accept this federation’s mint v2 keys before a card can be used")?;
        }
        Ok(())
    }

    /// [`attach`](Self::attach) for a card named by its short id, without reading the card.
    pub async fn attach_id(&self, cw: &mut CardWallet, short_id: &str) -> Result<StateStore> {
        self.confirm_mint(&cw.wallet).await?;
        let card = StateStore::open(self.card_path(short_id))?;
        let base = std::mem::replace(&mut cw.store, card);
        cw.ops = Some(OpsLink {
            client: self.client.clone(),
            short_id: short_id.to_owned(),
            version: 0,
            remote_slots: vec![],
            unkeyed: vec![],
            open: vec![],
            releases: vec![],
            stranded: vec![],
            cert_key: self.kept_cert_key().await,
            dead_change: vec![],
        });
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
    /// queued in the card's file), report the slots it held ([`CardWallet::ops_report_strands`]),
    /// then give `cw` its own store back.
    pub async fn finish(cw: &mut CardWallet, base: StateStore) {
        cw.cert_checked = false;
        cw.ops_sync().await;
        cw.ops_report_strands().await;
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
        self.card_files().into_iter().flat_map(|(_, st)| st.pending_txs.into_iter().filter_map(|p| p.client_ref)).collect()
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
        if !remote.belongs_to(&self.wallet.federation_id().to_string(), self.wallet.mint_version) {
            return Err(OpsError::Refused(409, "the card belongs to a different federation or mint".into()).into());
        }
        link.version = remote.version;
        link.remote_slots = remote.slots.iter().map(|s| (s.slot, s.pubkey.clone())).collect();
        link.unkeyed = remote.unkeyed_slots.clone();
        link.open = remote.open_txs.iter().map(|t| t.txid.clone()).collect();
        link.releases = remote.releases.clone();
        link.stranded = remote.stranded.clone();
        Ok(remote)
    }

    /// Send a 3.0 card the registry's releases (docs/OFFLINE-PROTOCOL.md § 6) that still fit it:
    /// a slot SPENT on that txid, or CHANGE owed by it. A released input slot is NOTE again (ops
    /// recorded its note live with the rejection) and a released change slot is a free KEYED one
    /// (ops never issued its note), so the working copy from ops already reads them so and the
    /// card is brought in line with it. Answers how many were released; never fails the tap: a
    /// release that does not take is logged and comes again with the card's next state.
    ///
    /// A CHANGE slot whose notes on its current key are all dead in the card's file
    /// ([`CardState::dead_change_slot`]) is not released here: it is listed in the link's
    /// `dead_change` with its release, as is such a slot the card already reads KEYED, for
    /// [`card_dead_change`](Self::card_dead_change) to release and re-key once the PIN is checked.
    /// Until then the slot reads CHANGE and no phone takes it.
    ///
    /// [`CardState::dead_change_slot`]: crate::state::CardState::dead_change_slot
    pub fn card_releases(&mut self) -> usize {
        let Some((releases, short_id)) = self.ops.as_ref().map(|l| (l.releases.clone(), l.short_id.clone())) else { return 0 };
        let dead: Vec<u8> =
            self.store.state.slots.iter().map(|s| s.slot).filter(|s| self.store.state.dead_change_slot(*s) && self.may_renew(*s)).collect();
        if (releases.is_empty() && dead.is_empty()) || !self.card_sign_once().unwrap_or(false) {
            return 0;
        }
        let st = match on_card(|| self.card.get_state()) {
            Ok(st) => st,
            Err(e) => {
                tracing::warn!("releases for card {short_id}: GET_STATE: {e:#}");
                return 0;
            }
        };
        let mut unit: Vec<DeadChange> = dead
            .iter()
            .filter(|s| st.states.get(**s as usize) == Some(&SlotState::Keyed))
            .map(|s| DeadChange { slot: *s, release: None })
            .collect();
        let n = self.card_send_releases(&short_id, releases, &st, &dead, &mut unit);
        if !unit.is_empty() {
            tracing::info!("card {short_id}: dead change slot(s) {:?} wait for the PIN", unit.iter().map(|d| d.slot).collect::<Vec<_>>());
        }
        if let Some(l) = self.ops.as_mut() {
            l.dead_change = unit;
        }
        n
    }

    /// [`card_releases`](Self::card_releases) with the card's state read: each release that fits
    /// is sent, except a dead change slot's, which goes to `unit`.
    fn card_send_releases(
        &mut self,
        short_id: &str,
        releases: Vec<RemoteRelease>,
        st: &fedimint_card_host_card::CardStatus,
        dead: &[u8],
        unit: &mut Vec<DeadChange>,
    ) -> usize {
        if releases.is_empty() {
            return 0;
        }
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
            if st.states.get(r.slot as usize) == Some(&SlotState::Change) && dead.contains(&r.slot) {
                unit.push(DeadChange { slot: r.slot, release: Some((txid, sig)) });
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

    /// Release and re-key, as one unit, the dead change slots [`card_releases`](Self::card_releases)
    /// listed: RELEASE (when the card reads CHANGE), then [`overwrite_slot`] at once. Run once per
    /// job, by [`verify_pin_session`] when it first answers Ok: re-keying needs the PIN session.
    /// Outside the spare budget; a dead note needs no federation call. A tap cut between the two
    /// steps leaves the slot KEYED on its burned key, and the next tap with the PIN re-keys it.
    /// Never fails the job: a step that does not take is logged and comes again.
    ///
    /// [`overwrite_slot`]: CardWallet::overwrite_slot
    /// [`verify_pin_session`]: CardWallet::verify_pin_session
    pub fn card_dead_change(&mut self) {
        let Some(link) = self.ops.as_mut() else { return };
        let unit = std::mem::take(&mut link.dead_change);
        let short_id = link.short_id.clone();
        for d in unit {
            if let Some((txid, sig)) = &d.release
                && let Err(e) = on_card(|| self.card.release(d.slot, txid, sig))
            {
                tracing::warn!("card {short_id}: RELEASE of dead change slot {}: {e:#}", d.slot);
                continue;
            }
            match self.overwrite_slot(d.slot) {
                Ok(_) => tracing::info!("card {short_id}: dead change slot {} released and re-keyed", d.slot),
                Err(e) => tracing::warn!("card {short_id}: re-keying dead change slot {}: {e:#}", d.slot),
            }
        }
    }

    /// Hold back, for this job, what the registry's copy of a 3.0 card cannot explain (ENG-867):
    /// a SPENT slot whose note it calls live, or a CHANGE slot whose key never carried a note, for
    /// a transaction it never saw (a POS phone's offline sales, lost with the phone). Unheld, the
    /// planner picks such a note and the card refuses to sign it again, and a charge's change can
    /// be planned into such a slot. A slot is explained, and left alone, when its note is an input
    /// of an open charge, or its transaction is one this till or the registry has: an open charge,
    /// a live or pending note's transaction, or a charge in this till's offline queue under
    /// `data_dir` for the card that it has neither abandoned nor had refused by the registry
    /// (`queued_txids`).
    ///
    /// A slot the registry lists as stranded, as the same kind, is held as listed: a SPENT slot
    /// cannot sign again and a CHANGE slot cannot reopen. Up to [`MAX_STRAND_CHECKS`] others are
    /// read (`GET_SLOT`) for their transaction; one past that, or whose read fails, is held on
    /// doubt and read at a later tap. [`Ops::finish`] reports the holds
    /// ([`ops_report_strands`](Self::ops_report_strands)). Card reads only; never fails the tap.
    pub fn card_strands(&mut self, data_dir: Option<&Path>) {
        self.store.state.held.clear();
        let Some(link) = self.ops.clone() else { return };
        if !self.card_sign_once().unwrap_or(false) {
            return;
        }
        let st = match on_card(|| self.card.get_state()) {
            Ok(st) => st,
            Err(e) => {
                tracing::warn!("card {}: stranded slots not checked this tap: GET_STATE: {e:#}", link.short_id);
                return;
            }
        };
        let local = &self.store.state;
        let open_inputs: HashSet<&str> = local.pending_txs.iter().flat_map(|p| p.inputs.iter().map(String::as_str)).collect();
        let mut candidates: Vec<(u8, HeldKind, u64)> = vec![];
        for s in &local.slots {
            match st.states.get(s.slot as usize) {
                Some(SlotState::Spent) => {
                    if let Some(n) = local.notes_on_current_key(s.slot).find(|n| n.status == NoteStatus::Live)
                        && !open_inputs.contains(n.nonce.as_str())
                    {
                        candidates.push((s.slot, HeldKind::Spent, n.amount_msat));
                    }
                }
                Some(SlotState::Change) if local.notes_on_current_key(s.slot).next().is_none() => {
                    candidates.push((s.slot, HeldKind::Change, 0));
                }
                _ => {}
            }
        }
        if candidates.is_empty() {
            return;
        }
        let mut known: HashSet<String> = local.pending_txs.iter().map(|p| p.txid.clone()).collect();
        // A live or pending note's txid only: a spent note's can name a transaction that landed
        // outside every till and the registry (docs/OFFLINE-PROTOCOL.md § 7, decision 5).
        known.extend(
            local.notes.iter().filter(|n| matches!(n.status, NoteStatus::Live | NoteStatus::Pending)).filter_map(|n| n.txid.clone()),
        );
        known.extend(link.open.iter().cloned());
        let mut queue_read = false;
        let mut held = vec![];
        let mut asked = 0;
        for (slot, kind, msat) in candidates {
            if let Some(r) = link.stranded.iter().find(|r| r.slot == slot && r.kind == kind.as_str()) {
                held.push(Held { slot, kind, txid: Some(r.txid.clone()), msat, release_after_ms: Some(r.release_after), due: r.due });
                continue;
            }
            let doubt = Held { slot, kind, txid: None, msat, release_after_ms: None, due: false };
            if asked == MAX_STRAND_CHECKS {
                held.push(doubt);
                continue;
            }
            asked += 1;
            let info = match on_card(|| self.card.get_slot(slot)) {
                Ok(info) => info,
                Err(e) => {
                    tracing::warn!("card {}: slot {slot} held on doubt: GET_SLOT: {e:#}", link.short_id);
                    held.push(doubt);
                    continue;
                }
            };
            // The card holds another key than the registry's: not the registry's note or slot.
            if info.xonly.map(|k| k.to_string()).as_deref() != self.store.state.slot_pubkey(slot) {
                continue;
            }
            let hash = match (kind, info.state) {
                (HeldKind::Spent, SlotState::Spent) => info.spent_hash,
                (HeldKind::Change, SlotState::Change) => info.opening.txid,
                _ => continue,
            };
            let txid = hex::encode(hash);
            if !queue_read {
                queue_read = true;
                known.extend(queued_txids(data_dir, &link.short_id));
            }
            if !known.contains(&txid) {
                held.push(Held { slot, kind, txid: Some(txid), msat, release_after_ms: None, due: false });
            }
        }
        if !held.is_empty() {
            let slots: Vec<String> = held.iter().map(|h| format!("{} ({})", h.slot, h.kind.as_str())).collect();
            let msat: u64 = held.iter().map(|h| h.msat).sum();
            tracing::warn!("card {}: holding {} stranded slot(s) back, {msat} msat: {}", link.short_id, held.len(), slots.join(", "));
        }
        self.store.state.held = held;
    }

    /// Report the slots this job held ([`card_strands`](Self::card_strands)) when the registry
    /// does not list one yet, or lists one as due: `POST /cards/:id/stranded` with every held slot
    /// whose transaction is known, so a change slot goes with its transaction's inputs. First the
    /// federation is asked about each due spent note, all within [`STRAND_CHECK_TIMEOUT`]: unspent
    /// is `unspent: true`, on which the registry may release the slot; spent is `spent: true`, on
    /// which it records the note spent and never releases it, and the note is spent here too, so
    /// the slot renews at the next charge with the PIN; no answer sends neither, and the next tap
    /// asks again. Releases come back in the card's state and reach the card at its next tap
    /// ([`card_releases`](Self::card_releases)). Never fails the job: a registry older than
    /// strandings answers 404, and that or any other error is logged.
    pub async fn ops_report_strands(&mut self) {
        let Some(link) = self.ops.clone() else { return };
        let held: Vec<Held> = self.store.state.held.iter().filter(|h| h.txid.is_some()).cloned().collect();
        let listed = |h: &Held| {
            link.stranded.iter().find(|r| r.slot == h.slot && r.kind == h.kind.as_str() && h.txid.as_deref() == Some(r.txid.as_str()))
        };
        if !held.iter().any(|h| listed(h).is_none_or(|r| r.due)) {
            return;
        }
        let deadline = tokio::time::Instant::now() + STRAND_CHECK_TIMEOUT;
        let mut spent: Vec<(u8, bool)> = vec![];
        for h in held.iter().filter(|h| h.kind == HeldKind::Spent && listed(h).is_some_and(|r| r.due)) {
            let Some(nonce) = self
                .store
                .state
                .notes_on_current_key(h.slot)
                .find(|n| matches!(n.status, NoteStatus::Live | NoteStatus::Spent))
                .and_then(|n| crate::glue::nonce_from_hex(&n.nonce).ok())
            else {
                continue;
            };
            match tokio::time::timeout_at(deadline, self.is_spent(nonce)).await {
                Ok(Ok(s)) => spent.push((h.slot, s)),
                Ok(Err(e)) => {
                    tracing::warn!("card {}: slot {}: the federation did not say whether its note is spent: {e:#}", link.short_id, h.slot)
                }
                Err(_) => {
                    tracing::warn!(
                        "card {}: the federation did not answer within {STRAND_CHECK_TIMEOUT:?}; asked again at the next tap",
                        link.short_id
                    );
                    break;
                }
            }
        }
        let flag = |h: &Held| spent.iter().find(|(s, _)| h.kind == HeldKind::Spent && *s == h.slot).map(|(_, x)| *x);
        let rows: Vec<Value> = held
            .iter()
            .take(MAX_STRANDED_ROWS)
            .map(|h| {
                let mut v = json!({"slot": h.slot, "kind": h.kind.as_str(), "txid": h.txid});
                match flag(h) {
                    Some(false) => v["unspent"] = json!(true),
                    Some(true) => v["spent"] = json!(true),
                    None => {}
                }
                v
            })
            .collect();
        match link.client.stranded(&link.short_id, &json!({"slots": rows})).await {
            Ok(_) => tracing::info!("card {}: reported {} stranded slot(s) to ops", link.short_id, rows.len()),
            Err(OpsError::UnknownCard(m)) => {
                tracing::warn!("card {}: ops takes no stranded slots (404, a registry older than them?): {m}", link.short_id)
            }
            Err(e) => tracing::warn!("card {}: stranded slots not reported this tap: {e}", link.short_id),
        }
        // Spent on the federation: spent here too.
        let mut changed = false;
        for &(slot, _) in spent.iter().filter(|(_, s)| *s) {
            let Some(pk) = self.store.state.slot_pubkey(slot).map(str::to_owned) else { continue };
            for n in self.store.state.notes.iter_mut().filter(|n| n.slot == slot && n.status == NoteStatus::Live) {
                if nonce_matches_pubkey(&n.nonce, &pk) {
                    n.status = NoteStatus::Spent;
                    changed = true;
                }
            }
        }
        if changed && let Err(e) = self.store.save() {
            tracing::warn!("card {}: recording a note the federation spent: {e:#}", link.short_id);
        }
    }

    /// Deliver what this till owes ops, then replace the card side of the local file with what
    /// ops holds. Fails if ops cannot be reached: a stale copy is never charged against.
    pub async fn ops_load(&mut self) -> Result<()> {
        let mut remote = self.ops_fetch().await?;
        // Publish a committed issue before replacing the local note list from the registry.
        self.finish_issue_submission().await?;
        if !self.store.state.ops_outbox.is_empty() {
            self.ops_push().await?;
            remote = self.ops_fetch().await?;
        }
        let cert_key = self.ops.as_ref().and_then(|l| l.cert_key.clone());
        merge_checked(&mut self.store.state, &remote, cert_key.as_deref());
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
                    Ok(a) => Ok(self.take_reg_sigs(a)),
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
                let local: Vec<&NoteRec> =
                    notes.iter().filter_map(|l| self.store.state.notes.iter().find(|n| n.nonce == l.nonce)).collect();
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
                let sigs: Vec<Value> =
                    local.iter().map(|n| json!({"nonce": n.nonce, "signature": n.signature.as_deref().unwrap_or_default()})).collect();
                link.client.outcome(id, &json!({"txid": txid, "accepted": true, "change": sigs})).await.map(|a| self.take_reg_sigs(a))
            }
        };
        let v = r?;
        self.ops.as_mut().expect("set").version = v;
        Ok(Sent::Done)
    }

    /// Ask the registry again for the signature of each note live in ops that has none, by
    /// repeating its transaction's accepted outcome (`POST /cards/:id/outcome`; a holder's load,
    /// which that route does not know, at `/load/outcome`), and keep what the answers sign. Only a
    /// transaction ops no longer holds open and this till owes no write for, so a repeat never
    /// decides anything; at most [`MAX_SIG_ASKS_PER_TAP`] a tap, and the first failure to reach
    /// the registry ends the asking for this tap. Never fails the tap. Answers how many notes it
    /// took a signature for.
    pub async fn ops_ask_reg_sigs(&mut self) -> usize {
        let Some(link) = self.ops.clone() else { return 0 };
        let now = crate::claim::now_ms();
        let due = sig_asks_due(&mut self.store.state, &link.open, now);
        let waiting =
            |s: &Self, t: &str| s.store.state.notes.iter().filter(|n| n.txid.as_deref() == Some(t) && n.reg_sig.is_none()).count();
        let id = link.short_id.as_str();
        let mut took = 0;
        let mut changed = false;
        for txid in due {
            let change = repeat_change(&self.store.state, &txid);
            let before = waiting(self, &txid);
            let answer = if self.store.state.sig_asks.get(&txid).is_some_and(|a| a.load) {
                Err(OpsError::UnknownCard("a holder's load".into()))
            } else {
                link.client.outcome(id, &json!({"txid": txid, "accepted": true, "change": change})).await
            };
            let answer = match answer {
                // Not a transaction the registry knows: a holder's load, asked at its own route
                // (`409` is a transaction it knows, and never comes here).
                Err(OpsError::UnknownCard(_)) => {
                    changed = true;
                    self.store.state.sig_asks.entry(txid.clone()).or_default().load = true;
                    if now < self.store.state.load_ask_ms.saturating_add(LOAD_ASK_EVERY_MS) {
                        continue;
                    }
                    self.store.state.load_ask_ms = now;
                    link.client.load_outcome(id, &json!({"txid": txid, "notes": change})).await
                }
                r => r,
            };
            match answer {
                Ok(a) => {
                    let v = self.take_reg_sigs(a);
                    self.ops.as_mut().expect("set").version = v;
                }
                // A 429 (the holder's budget, which anyone with the short id can spend) is no
                // answer about the transaction: like an unreachable registry, it is no miss.
                Err(e @ (OpsError::Unreachable(_) | OpsError::Refused(429, _))) => {
                    tracing::warn!("ops: note signatures not asked for this tap: {e}");
                    break;
                }
                Err(e) => tracing::warn!("ops: no note signatures from a repeat of {txid}: {e}"),
            }
            changed = true;
            let got = before.saturating_sub(waiting(self, &txid));
            took += got;
            let a = self.store.state.sig_asks.entry(txid).or_default();
            a.last_ms = now;
            if got == 0 {
                a.misses = a.misses.saturating_add(1);
            }
        }
        if changed && let Err(e) = self.store.save() {
            tracing::warn!("ops: the note signatures asked for are not saved yet: {e:#}");
        }
        took
    }

    /// Keep the registry's signature for each note an outcome's answer lists, on the local note with
    /// that nonce and slot. Answers the answer's version.
    /// A signature the till can tell is wrong ([`crate::glue::reg_sig_kept`]) is not kept: the
    /// note waits and is asked for again.
    fn take_reg_sigs(&mut self, a: OutcomeAnswer) -> u64 {
        let cert_key = self.ops.as_ref().and_then(|l| l.cert_key.clone());
        for s in &a.notes {
            let st = &self.store.state;
            let Some(i) = st.notes.iter().position(|n| n.nonce == s.nonce && n.slot == s.slot) else {
                tracing::warn!("ops: the registry signed a note this till does not hold (slot {})", s.slot);
                continue;
            };
            if !crate::glue::reg_sig_kept(st, cert_key.as_deref(), &st.notes[i], &s.reg_sig) {
                tracing::warn!("ops: the registry's signature over the note on slot {} does not hold; the note waits", s.slot);
                continue;
            }
            self.store.state.notes[i].reg_sig = Some(s.reg_sig.clone());
        }
        a.version
    }

    /// Queue `w`, replacing an earlier entry for the same transaction or slot.
    pub(crate) fn ops_enqueue(&mut self, w: OpsWrite) {
        let Some(link) = &self.ops else { return };
        if let OpsWrite::Outcome { txid, .. } = &w
            && !link.open.contains(txid)
        {
            return;
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
    pub(crate) async fn ops_intent(
        &mut self,
        txid: &str,
        client_ref: Option<&str>,
        tx_hex: &str,
        inputs: &[String],
        change: Vec<Value>,
    ) -> Result<()> {
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
        let change = mine.filter(|n| n.status == NoteStatus::Live).filter_map(|n| Some((n.nonce.clone(), n.signature.clone()?))).collect();
        self.ops_enqueue(OpsWrite::Outcome { txid: txid.to_owned(), accepted: true, change });
    }

    /// Keep a 3.0 card's spare reserve (docs/OFFLINE-PLAN.md § 10): when it has fewer than
    /// `self.spares.target` fresh keyed slots, key at most [`spares_to_key`] more in this tap.
    /// SPENT slots whose notes are settled (spent or dead in ops, confirmed spent by the
    /// federation, no charge open on them) are renewed first ([`overwrite_slot`], queued to ops
    /// like every renewal); they need the holder's PIN, so only a job that carries it renews.
    /// Then never-keyed slots, recorded in ops as split keying does ([`key_slots`]). Both take the
    /// change slots (24 to 31) first, and while fewer than [`MIN_CHANGE_SPARES`] of those are
    /// KEYED and free the tap keys that many whatever its budget ([`spare_may_take`]). Off ops,
    /// on a 2.x card, or with the reserve full it does nothing. The caller logs any error and
    /// goes on, except a refused PIN.
    ///
    /// [`overwrite_slot`]: CardWallet::overwrite_slot
    pub async fn card_spares(&mut self, released: usize, written: usize) -> Result<SpareReport> {
        let mut rep = SpareReport::default();
        let Some(link) = self.ops.clone() else { return Ok(rep) };
        if self.spares.target == 0 || self.spares.max_per_tap == 0 || !self.card_sign_once()? {
            return Ok(rep);
        }
        let st = on_card(|| self.card.get_state()).context("GET_STATE")?;
        let on_card_is = |slot: u8, want: &[SlotState]| st.states.get(slot as usize).is_some_and(|s| want.contains(s));
        let fresh: Vec<u8> = self
            .store
            .state
            .free_slots()
            .into_iter()
            .chain(self.store.state.free_change_slots())
            .filter(|s| on_card_is(*s, &[SlotState::Keyed]))
            .collect();
        rep.fresh = fresh.len();
        let n = spares_to_key(rep.fresh, self.spares, released, written);
        let spent: Vec<u8> = if self.pin.is_some() {
            let open: Vec<&String> = self.store.state.pending_txs.iter().flat_map(|p| &p.inputs).collect();
            let spent = self
                .store
                .state
                .overwritable_slots()
                .into_iter()
                .filter(|s| self.may_renew(*s) && on_card_is(*s, &[SlotState::Spent, SlotState::Keyed]))
                .filter(|s| self.store.state.notes_on_current_key(*s).all(|n| !open.contains(&&n.nonce)))
                .filter(|s| !link.dead_change.iter().any(|d| d.slot == *s))
                .collect();
            change_slots_first(spent)
        } else {
            vec![]
        };
        let unkeyed = change_slots_first(link.unkeyed.clone());
        // The change slots this tap owes beyond its budget: as many as the card lacks of
        // MIN_CHANGE_SPARES, if it has slots to give them.
        let open_change = spent.iter().chain(&unkeyed).filter(|s| is_change_slot(**s)).count();
        let floor = MIN_CHANGE_SPARES.saturating_sub(fresh.iter().filter(|s| is_change_slot(**s)).count()).min(open_change);
        if n == 0 && floor == 0 {
            return Ok(rep);
        }

        // 1. Settled SPENT slots: their keys are burned, so renewing one costs the card nothing.
        if spent.iter().any(|s| spare_may_take(*s, &[], n, floor)) {
            self.verify_pin_session()?;
            for slot in spent {
                if !spare_may_take(slot, &rep.renewed, n, floor) {
                    continue;
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

        // 2. Never-keyed slots.
        let mut taken = rep.renewed.clone();
        let mut want = vec![];
        for slot in unkeyed {
            if spare_may_take(slot, &taken, n, floor) {
                taken.push(slot);
                want.push(slot);
            }
        }
        if !want.is_empty() {
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

    /// Unkeyed slots this tap may key for a load or an online charge's change: those ops
    /// reported below 24, none off ops. Slots 24 to 31 are the spare reserve's
    /// ([`CardWallet::card_spares`]).
    pub(crate) fn keyable_slots(&self) -> usize {
        self.ops.as_ref().map_or(0, |l| l.unkeyed.iter().filter(|s| !is_change_slot(**s)).count())
    }

    /// Make sure the card has `need` free keyed slots, keying unkeyed ones if it has too few
    /// ([`slots_to_key`], [`key_slots`]). Returns the slots keyed. Off ops, or with enough free
    /// slots, it does nothing; with too few even after keying it keys nothing and leaves the
    /// refusal to the caller. Ops not answering is an error: the work that needed the slots is
    /// refused rather than the keys queued.
    pub(crate) async fn ops_make_room(&mut self, need: usize) -> Result<Vec<u8>> {
        let Some(link) = self.ops.clone() else { return Ok(vec![]) };
        let free = self.store.state.free_slots().len();
        let unkeyed: Vec<u8> = link.unkeyed.iter().copied().filter(|s| !is_change_slot(*s)).collect();
        let n = slots_to_key(need, free, unkeyed.len());
        if n == 0 || free + n < need {
            return Ok(vec![]);
        }
        let want = unkeyed[..n].to_vec();
        self.ops_key_unkeyed(&want).await?;
        Ok(want)
    }

    /// Whether this card's slot may be re-keyed. On ops, slot 0's key names the card (its short
    /// id), so it is never renewed: a renewed slot 0 would make the card unknown to every till.
    /// Off ops, slot 0 of a certified 4.x card is refused where the key is replaced
    /// ([`CardWallet::slot0_certified`], checked by `overwrite_slot`, `renew_spent_slots` and
    /// `card_setup`), since that asks the card and this does not.
    pub(crate) fn may_renew(&self, slot: u8) -> bool {
        self.ops.is_none() || slot != 0
    }
}

/// The transactions this till's offline queue (`<data_dir>/offline_queue.json`) holds for card
/// `short_id`, abandoned and refused ones aside: this till knows them, so their slots are
/// explained. An abandoned charge is never sent (an input slot of it paid another transaction or
/// was re-keyed), and a charge the registry refused (`DoubleSpent`, `Refused`) is never sent
/// either, so the slots the card spent or opened for it are stranded, and are held and reported
/// like a lost phone's. What the registry does with them depends on its refusal:
/// - a refusal that leaves the transaction unseen there (a change nonce that already carried a
///   note, `change_used`; the money gate's refusal, `released`; a 400 or another 409, which it
///   keeps no report of) takes a stranding for each slot still held and releases it after its
///   grace (docs/OFFLINE-PROTOCOL.md § 7);
/// - a double spend, an input not live at the registry, counts as the registry having seen the
///   transaction (`SEEN_REPORT`, registry/src/cards.ts). It releases the sale's change slots at
///   once (§ 6) and takes no stranding for it. An input it still lists live keeps its SPENT slot
///   on the card and is held here: the report of it is ignored, the registry never lists it, so
///   it is reported again at every tap and no release comes for it.
///
/// A queue that cannot be read is taken as empty: the slot is then held and reported, and the
/// sender's report of the charge clears its row in the registry.
fn queued_txids(data_dir: Option<&Path>, short_id: &str) -> Vec<String> {
    use fedimint_card_pos_core::queue::ChargeState;
    let Some(dir) = data_dir else { return vec![] };
    match fedimint_card_pos_core::queue::Queue::open(dir) {
        Ok(q) => q
            .file
            .charges
            .into_iter()
            .filter(|c| {
                c.short_id == short_id && !matches!(c.state, ChargeState::Abandoned | ChargeState::DoubleSpent | ChargeState::Refused)
            })
            .map(|c| c.txid)
            .collect(),
        Err(e) => {
            tracing::warn!("card {short_id}: the offline queue could not be read: {e:#}");
            vec![]
        }
    }
}

/// Replace the card side of `st` with `remote`, keeping what only this till knows: its own
/// outputs, its unsent writes and the history of keys ops no longer reports.
pub fn merge(st: &mut CardState, remote: &RemoteState) {
    merge_checked(st, remote, None)
}

/// [`merge`], keeping a registry signature over a note only where
/// [`crate::glue::reg_sig_kept`] does under `cert_key`: the till's own copy stands in for one
/// that does not hold, and without either the note waits and is asked for again.
pub fn merge_checked(st: &mut CardState, remote: &RemoteState, cert_key: Option<&str>) {
    // Slots: ops's keys, with this till's renewals that ops has not taken yet laid over them.
    st.slots = remote.slots.iter().map(|s| SlotRec { slot: s.slot, pubkey: s.pubkey.clone(), unknown: false }).collect();
    st.slots.sort_by_key(|s| s.slot);
    for w in &st.ops_outbox {
        if let OpsWrite::Slot { slot, old_pubkey, pubkey } = w
            && let Some(s) = st.slots.iter_mut().find(|s| s.slot == *slot && s.pubkey == *old_pubkey)
        {
            s.pubkey = pubkey.clone();
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
    // A signature this till already holds is kept if ops's copy lacks it.
    let held: std::collections::HashMap<String, String> =
        st.notes.iter().filter_map(|n| Some((n.nonce.clone(), n.reg_sig.clone()?))).collect();
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
            reg_sig: n.reg_sig.clone().or_else(|| held.get(&n.nonce).cloned()),
        })
        .chain(kept)
        .collect();
    for i in 0..st.notes.len() {
        let Some(sig) = st.notes[i].reg_sig.clone() else { continue };
        if crate::glue::reg_sig_kept(st, cert_key, &st.notes[i], &sig) {
            continue;
        }
        let ours = held.get(&st.notes[i].nonce).filter(|h| **h != sig && crate::glue::reg_sig_kept(st, cert_key, &st.notes[i], h)).cloned();
        if ours.is_none() {
            tracing::warn!("ops: the registry's signature over the note on slot {} does not hold; the note waits", st.notes[i].slot);
        }
        st.notes[i].reg_sig = ours;
    }

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

/// Whether `err` is the card refusing the holder's PIN (`63 CX`: wrong, X in a row) or a PIN wait
/// the card was taken from (`pin_wait`): either way no PIN was taken.
pub fn pin_refused(err: &anyhow::Error) -> bool {
    err.chain().any(|c| {
        c.downcast_ref::<fedimint_card_host_card::StatusWord>().is_some_and(|s| matches!(s.0, 0x63C0..=0x63CF))
            || c.downcast_ref::<fedimint_card_pos_core::PinWait>().is_some()
    })
}

/// Whether `err` is a PIN the till would not send a 4.x card: no certificate key to check the card
/// under ([`crate::glue::NoCertKey`], 503 `offline_not_ready`) or a card that failed the check
/// ([`fedimint_card_host_card::UntrustedCard`], 403 `card_not_genuine`). No PIN was sent.
pub fn pin_unsendable(err: &anyhow::Error) -> bool {
    err.chain().any(|c| {
        c.downcast_ref::<crate::glue::NoCertKey>().is_some() || c.downcast_ref::<fedimint_card_host_card::UntrustedCard>().is_some()
    })
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
        NoteRec {
            slot,
            amount_msat: 1024,
            nonce: nonce.into(),
            signature: None,
            status,
            blinding_key: None,
            txid: None,
            out_idx: None,
            reg_sig: None,
        }
    }

    fn remote(notes: Vec<RemoteNote>, open: Vec<RemoteTx>) -> RemoteState {
        RemoteState {
            federation_id: None,
            mint_version: Default::default(),
            version: 3,
            slots: (0..2).map(|s| RemoteSlot { slot: s, pubkey: format!("{s:02x}").repeat(32) }).collect(),
            unkeyed_slots: vec![],
            notes,
            open_txs: open,
            releases: vec![],
            stranded: vec![],
        }
    }

    #[test]
    fn card_state_is_bound_to_the_federation_and_mint() {
        use crate::wallet::MintVersion::{V1, V2};
        let mut state = remote(vec![], vec![]);
        assert!(state.belongs_to("ours", V1));
        assert!(!state.belongs_to("ours", V2));
        state.mint_version = V2;
        assert!(!state.belongs_to("ours", V2));
        state.federation_id = Some("theirs".into());
        assert!(!state.belongs_to("ours", V2));
        state.federation_id = Some("ours".into());
        assert!(state.belongs_to("ours", V2));
        assert!(!state.belongs_to("ours", V1));
    }

    #[test]
    fn the_spare_reserve_keys_the_shortfall_within_the_taps_budget() {
        let cfg = SpareConfig::default();
        assert_eq!((cfg.target, cfg.max_per_tap), (12, 2));
        assert_eq!(spares_to_key(12, cfg, 0, 0), 0, "full");
        assert_eq!(spares_to_key(15, cfg, 0, 0), 0);
        assert_eq!(spares_to_key(11, cfg, 0, 0), 1, "the shortfall");
        assert_eq!(spares_to_key(0, cfg, 0, 0), 2, "never more than the cap");
        // A tap busy with a release or up to two notes has room for one key; with more, none. A
        // note costs the 136 ms a SET_NOTE was measured at.
        assert_eq!(spares_to_key(0, cfg, 1, 0), 1);
        assert_eq!(spares_to_key(0, cfg, 0, 2), 1);
        assert_eq!(spares_to_key(0, cfg, 0, 3), 0);
        assert_eq!(spares_to_key(0, cfg, 4, 0), 0);
        assert_eq!(spares_to_key(0, cfg, 0, 16), 0);
        assert_eq!(spares_to_key(0, SpareConfig { target: 0, max_per_tap: 2 }, 0, 0), 0, "off");
        assert_eq!(spares_to_key(0, SpareConfig { target: 6, max_per_tap: 0 }, 0, 0), 0, "off");
    }

    /// The slots a tap takes, offered `offer` in order, with a budget of `n` and a floor.
    fn spares_taken(offer: &[u8], n: usize, floor: usize) -> Vec<u8> {
        let mut taken = vec![];
        for s in change_slots_first(offer.to_vec()) {
            if spare_may_take(s, &taken, n, floor) {
                taken.push(s);
            }
        }
        taken
    }

    #[test]
    fn the_spare_reserve_keys_change_slots_first_and_two_of_them_whatever_the_budget() {
        let never_keyed: Vec<u8> = (8..32).collect();
        assert_eq!(change_slots_first(vec![9, 30, 8, 24]), vec![24, 30, 8, 9]);
        // Within the budget: change slots first.
        assert_eq!(spares_taken(&never_keyed, 2, 0), vec![24, 25]);
        assert_eq!(spares_taken(&never_keyed, 2, 2), vec![24, 25]);
        // A tap whose notes spent the budget still keys the floor, and no more.
        assert_eq!(spares_taken(&never_keyed, 0, 2), vec![24, 25]);
        assert_eq!(spares_taken(&never_keyed, 1, 2), vec![24, 25]);
        assert_eq!(spares_taken(&never_keyed, 0, 1), vec![24]);
        assert_eq!(spares_taken(&never_keyed, 0, 0), Vec::<u8>::new());
        // All change slots keyed: the budget goes to low slots, lowest first.
        assert_eq!(spares_taken(&(8..24).collect::<Vec<u8>>(), 2, 0), vec![8, 9]);
        // A low slot never takes the place the floor keeps for a change slot.
        assert!(!spare_may_take(8, &[], 2, 2));
        assert!(!spare_may_take(8, &[9], 2, 1));
        assert!(spare_may_take(8, &[24], 2, 1));
        assert!(spare_may_take(26, &[8, 9], 2, 1), "the floor: one change slot past the budget");
        assert!(!spare_may_take(26, &[24, 25], 2, 2));
    }

    #[test]
    fn unkeyed_slots_parse_and_default_to_none_from_an_older_registry() {
        let old: RemoteState = serde_json::from_value(json!({"version": 1, "slots": [], "notes": [], "open_txs": []})).unwrap();
        assert!(old.unkeyed_slots.is_empty());
        let new: RemoteState = serde_json::from_value(
            json!({"version": 1, "slot_count": 32, "slots": [], "unkeyed_slots": [8, 9], "notes": [], "open_txs": []}),
        )
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
    fn stranded_slots_parse_and_default_to_none_from_an_older_registry() {
        let old: RemoteState = serde_json::from_value(json!({"version": 1, "slots": [], "notes": [], "open_txs": []})).unwrap();
        assert!(old.stranded.is_empty());
        let new: RemoteState = serde_json::from_value(json!({
            "version": 1, "slots": [], "notes": [], "open_txs": [], "stranded": [
                {"slot": 3, "kind": "spent", "txid": "ab", "amount_msat": 16384, "since": 10, "release_after": 20, "due": false},
                {"slot": 4, "kind": "change", "txid": "ab", "amount_msat": null, "since": 10, "release_after": 20, "due": true},
                {"slot": 5, "kind": "later", "txid": "cd"},
            ],
        }))
        .unwrap();
        let s = &new.stranded;
        assert_eq!((s[0].slot, s[0].kind.as_str(), s[0].amount_msat, s[0].release_after, s[0].due), (3, "spent", Some(16384), 20, false));
        assert_eq!((s[1].kind.as_str(), s[1].amount_msat, s[1].due), ("change", None, true));
        assert_eq!((s[2].kind.as_str(), s[2].since, s[2].due), ("later", 0, false), "a kind this till does not know parses");
        assert_eq!((HeldKind::Spent.as_str(), HeldKind::Change.as_str()), ("spent", "change"));
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
                reg_sig: None,
            }],
            vec![
                RemoteTx {
                    txid: "t1".into(),
                    till_id: 2,
                    client_ref: "r".into(),
                    signed_hex: Some("ff".into()),
                    inputs: vec!["02dd".into()],
                },
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
        merge(
            &mut st,
            &remote(
                vec![],
                vec![RemoteTx {
                    txid: "t".into(),
                    till_id: 1,
                    client_ref: "mine".into(),
                    signed_hex: Some("aa".into()),
                    inputs: vec!["02".into()],
                }],
            ),
        );
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
        merge(
            &mut st,
            &remote(
                vec![],
                vec![RemoteTx {
                    txid: "t".into(),
                    till_id: 1,
                    client_ref: "r".into(),
                    signed_hex: Some("aa".into()),
                    inputs: vec!["02".into()],
                }],
            ),
        );
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

    #[test]
    fn an_outcome_answer_keeps_only_the_notes_the_registry_signed() {
        let a = OutcomeAnswer::of(&json!({"version": 7, "notes": [
            {"slot": 1, "nonce": "02aa", "reg_sig": "3006020101020101"},
            {"slot": 2, "nonce": "02bb", "reg_sig": null},
        ]}))
        .unwrap();
        assert_eq!(a.version, 7);
        assert_eq!(a.notes, vec![SignedNote { slot: 1, nonce: "02aa".into(), reg_sig: "3006020101020101".into() }]);
        // A registry older than note signatures answers no notes at all.
        assert_eq!(OutcomeAnswer::of(&json!({"version": 3})).unwrap(), OutcomeAnswer { version: 3, notes: vec![] });
        assert!(OutcomeAnswer::of(&json!({"notes": []})).is_err(), "an answer without a version is not taken");
    }

    #[test]
    fn merge_carries_the_registrys_signature_and_keeps_one_the_till_holds() {
        let mut st = CardState::default();
        let mut held = note("02bb", 1, NoteStatus::Live);
        held.reg_sig = Some("aa".into());
        st.notes.push(held);
        let theirs = |nonce: &str, slot: u8, reg_sig: Option<&str>| RemoteNote {
            slot,
            amount_msat: 1024,
            nonce: nonce.into(),
            signature: None,
            blinding_key: None,
            status: NoteStatus::Live,
            txid: None,
            out_idx: None,
            reg_sig: reg_sig.map(Into::into),
        };
        merge(&mut st, &remote(vec![theirs("02aa", 0, Some("cc")), theirs("02bb", 1, None), theirs("02dd", 2, None)], vec![]));
        let sig = |nonce: &str| st.notes.iter().find(|n| n.nonce == nonce).unwrap().reg_sig.clone();
        assert_eq!(sig("02aa").as_deref(), Some("cc"));
        assert_eq!(sig("02bb").as_deref(), Some("aa"));
        assert_eq!(sig("02dd"), None);
    }

    /// A live note of `txid` with the federation's signature and no registry signature.
    fn waiting(nonce: &str, slot: u8, txid: &str) -> NoteRec {
        let mut n = note(nonce, slot, NoteStatus::Live);
        n.signature = Some("ab".into());
        n.txid = Some(txid.into());
        n.reg_sig = None;
        n
    }

    #[test]
    fn an_outcome_answer_skips_an_entry_that_does_not_parse_and_needs_only_the_version() {
        let a = OutcomeAnswer::of(&json!({"version": 4, "notes": [
            {"slot": "one", "nonce": "02aa", "reg_sig": "30"},
            7,
            {"slot": 2, "nonce": "02bb", "reg_sig": 5},
            {"slot": 3, "nonce": "02cc", "reg_sig": "3006"},
        ]}))
        .unwrap();
        assert_eq!(a, OutcomeAnswer { version: 4, notes: vec![SignedNote { slot: 3, nonce: "02cc".into(), reg_sig: "3006".into() }] });
        assert_eq!(OutcomeAnswer::of(&json!({"version": 5, "notes": {"slot": 1}})).unwrap().notes, vec![]);
        assert!(OutcomeAnswer::of(&json!({"notes": []})).is_err(), "no version");
    }

    #[test]
    fn a_repeat_never_asks_for_a_transaction_open_in_ops_or_owed_in_the_outbox() {
        let mut st = CardState {
            notes: vec![waiting("02aa", 0, "open"), waiting("02bb", 1, "owed"), waiting("02cc", 2, "loading"), waiting("02dd", 3, "done")],
            ops_outbox: vec![
                OpsWrite::Outcome { txid: "owed".into(), accepted: true, change: vec![] },
                OpsWrite::Load { txid: "loading".into(), client_ref: "r".into(), notes: vec![] },
            ],
            ..Default::default()
        };
        assert_eq!(sig_asks_due(&mut st, &["open".to_string()], 1_000), vec!["done".to_string()]);
    }

    #[test]
    fn a_transaction_that_brings_nothing_backs_off_and_leaves_its_place_to_the_others() {
        assert_eq!((0..4).map(sig_ask_backoff_ms).collect::<Vec<_>>(), vec![0, 0, 60_000, 120_000]);
        assert_eq!(sig_ask_backoff_ms(40), SIG_ASK_MOST_WAIT_MS);
        let mut st = CardState { notes: (0..6).map(|i| waiting(&format!("02a{i}"), i, &format!("t{i}"))).collect(), ..Default::default() };
        let now = 10_000_000;
        for i in 0..4 {
            st.sig_asks.insert(format!("t{i}"), crate::state::SigAsk { last_ms: now - 30_000, misses: 2, load: false });
        }
        st.sig_asks.insert("t4".into(), crate::state::SigAsk { last_ms: now - 200_000, misses: 3, load: false });
        st.sig_asks.insert("gone".into(), crate::state::SigAsk { last_ms: 0, misses: 9, load: false });
        // t0..t3 wait out their minute; t5 was never asked, t4 is past its two minutes.
        assert_eq!(sig_asks_due(&mut st, &[], now), vec!["t5".to_string(), "t4".to_string()]);
        assert!(!st.sig_asks.contains_key("gone"), "a transaction with nothing waiting is forgotten");
        // A minute on, all are due, least recently asked first, four a tap.
        assert_eq!(sig_asks_due(&mut st, &[], now + 60_000), ["t5", "t4", "t0", "t1"].map(String::from).to_vec());
    }

    #[test]
    fn a_repeat_lists_only_the_notes_of_its_transaction_still_waiting() {
        let mut st = CardState::default();
        // An offline charge's input carries its txid too (the registry files it so), and is spent.
        let mut spent = waiting("02aa", 0, "t1");
        spent.status = NoteStatus::Spent;
        let mut signed = waiting("02dd", 3, "t1");
        signed.reg_sig = Some("ab".into());
        st.notes = vec![spent, waiting("02bb", 1, "t1"), signed, waiting("02cc", 2, "t2")];
        let nonces: Vec<String> = repeat_change(&st, "t1").iter().map(|c| c["nonce"].as_str().unwrap().to_owned()).collect();
        assert_eq!(nonces, vec!["02bb"]);
    }

    #[test]
    fn a_load_the_hourly_limit_holds_back_leaves_the_taps_places_to_the_tills_own_charges() {
        let mut st = CardState { notes: (0..6).map(|i| waiting(&format!("02a{i}"), i, &format!("t{i}"))).collect(), ..Default::default() };
        let now = 10_000_000;
        st.load_ask_ms = now - 600_000;
        for i in 0..4 {
            st.sig_asks.insert(format!("t{i}"), crate::state::SigAsk { last_ms: now - 900_000, misses: 0, load: true });
        }
        st.sig_asks.insert("t4".into(), crate::state::SigAsk { last_ms: now - 1_000, misses: 0, load: false });
        // Four holder loads wait out the hour; the till's own charge t4 and the new t5 are asked.
        assert_eq!(sig_asks_due(&mut st, &[], now), vec!["t5".to_string(), "t4".to_string()]);
        // Once the hour is up, one load a tap (only one can reach the load route), then the rest.
        let later = now - 600_000 + LOAD_ASK_EVERY_MS;
        assert_eq!(sig_asks_due(&mut st, &[], later), ["t5", "t0", "t4"].map(String::from).to_vec());
    }

    #[test]
    fn merge_drops_a_signature_the_card_refused() {
        let mut st = CardState::default();
        st.refuse_reg_sig("cc");
        let theirs = |nonce: &str, reg_sig: &str| {
            let mut n = remote(vec![], vec![]);
            n.notes.push(RemoteNote {
                slot: 0,
                amount_msat: 1024,
                nonce: nonce.into(),
                signature: None,
                blinding_key: None,
                status: NoteStatus::Live,
                txid: None,
                out_idx: None,
                reg_sig: Some(reg_sig.into()),
            });
            n
        };
        merge(&mut st, &theirs("02aa", "cc"));
        assert_eq!(st.notes[0].reg_sig, None, "refused once, never kept again");
        merge(&mut st, &theirs("02aa", "dd"));
        assert_eq!(st.notes[0].reg_sig.as_deref(), Some("dd"));
    }
}
