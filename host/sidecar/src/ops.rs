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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use fedimint_core::secp256k1::XOnlyPublicKey;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::glue::{CardWallet, on_card};
use crate::state::{CardState, LoadNote, NoteRec, NoteStatus, OpsWrite, PendingTx, SlotRec, StateStore};

/// Every registry call is bounded; a till that cannot reach ops says so instead of hanging.
const OPS_TIMEOUT: Duration = Duration::from_secs(15);
/// Where the per-card files live inside the data directory.
pub const CARDS_DIR: &str = "cards";

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
    pub slots: Vec<RemoteSlot>,
    pub notes: Vec<RemoteNote>,
    pub open_txs: Vec<RemoteTx>,
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

    /// `POST /federations`: what a till read from the federation's config ([`FederationReport`]).
    pub async fn federation(&self, report: &FederationReport) -> Result<(), OpsError> {
        let mut body = json!({"federation_id": report.federation_id, "tbs_pks": report.tbs_pks});
        if let Some(name) = report.name.as_deref().filter(|n| !n.trim().is_empty()) {
            body["name"] = json!(name);
        }
        self.call(reqwest::Method::POST, "/federations", Some(&body)).await.map(|_| ())
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

/// `GET /cards/:id/slots` as the registry answers it.
#[derive(Debug, Clone, Deserialize)]
pub struct FreeSlots {
    pub version: u64,
    pub federation_id: String,
    pub free: Vec<RemoteSlot>,
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
    /// Charges open in ops as last read, plus any this job posted an intent for. Only these get an
    /// outcome: one another till already recorded needs nothing from this one.
    pub open: Vec<String>,
}

/// A card's short id: the last 4 bytes of its slot-0 x-only key, lower-case hex.
pub fn short_id_of(slot0: &XOnlyPublicKey) -> String {
    let hex = slot0.to_string();
    hex[hex.len() - 8..].to_owned()
}

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
        self.attach_id(cw, &short_id_of(&pk)).await
    }

    /// [`attach`](Self::attach) for a card named by its short id, without reading the card.
    pub async fn attach_id(&self, cw: &mut CardWallet, short_id: &str) -> Result<StateStore> {
        let card = StateStore::open(self.card_path(short_id))?;
        let base = std::mem::replace(&mut cw.store, card);
        cw.ops = Some(OpsLink { client: self.client.clone(), short_id: short_id.to_owned(), version: 0, remote_slots: vec![], open: vec![] });
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
        link.open = remote.open_txs.iter().map(|t| t.txid.clone()).collect();
        Ok(remote)
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
        let mine = st.notes.iter().filter(|n| n.txid.as_deref() == Some(txid));
        if mine.clone().any(|n| n.status == NoteStatus::Pending) {
            return;
        }
        let change = mine
            .filter(|n| n.status == NoteStatus::Live)
            .filter_map(|n| Some((n.nonce.clone(), n.signature.clone()?)))
            .collect();
        self.ops_enqueue(OpsWrite::Outcome { txid: txid.to_owned(), accepted: true, change });
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

    fn note(nonce: &str, slot: u8, status: NoteStatus) -> NoteRec {
        NoteRec { slot, amount_msat: 1024, nonce: nonce.into(), signature: None, status, blinding_key: None, txid: None, out_idx: None }
    }

    fn remote(notes: Vec<RemoteNote>, open: Vec<RemoteTx>) -> RemoteState {
        RemoteState {
            version: 3,
            slots: (0..2).map(|s| RemoteSlot { slot: s, pubkey: format!("{s:02x}").repeat(32) }).collect(),
            notes,
            open_txs: open,
        }
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
