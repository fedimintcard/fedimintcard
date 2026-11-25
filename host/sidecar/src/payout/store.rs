//! `payout.json`: the one durable record of the merchant's payout settings, destination and every
//! payout (0600, temp file, fsync, rename, like `pays.json`). Every write goes through
//! [`PayoutStore::commit`], which saves before it counts: when the save fails the store in memory
//! is left as the file is.
//!
//! Only holders of the admin payout lock write it. Readers (the status route) open it afresh; the
//! rename makes every read see one whole version.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::state::write_private_json;

pub const PAYOUT_FILE: &str = "payout.json";
pub const PAYOUT_VERSION: u32 = 1;

/// 21 million BTC, in sats.
pub const MAX_SAT: u64 = 2_100_000_000_000_000;
pub const MIN_INTERVAL_MINUTES: u64 = 15;
pub const MAX_INTERVAL_MINUTES: u64 = 10_080;
pub const MAX_FEE_PPM: u64 = 100_000;
pub const MAX_FEE_SAT: u64 = 1_000_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Automatic payouts on the cadence; manual requests refused.
    #[default]
    Auto,
    /// Manual requests only.
    Manual,
    /// Both.
    Both,
    /// Nothing new starts; open payouts are still driven.
    Off,
}

impl Mode {
    pub fn auto(self) -> bool {
        matches!(self, Mode::Auto | Mode::Both)
    }
    pub fn manual(self) -> bool {
        matches!(self, Mode::Manual | Mode::Both)
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Mode::Auto),
            "manual" => Some(Mode::Manual),
            "both" => Some(Mode::Both),
            "off" => Some(Mode::Off),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Manual => "manual",
            Mode::Both => "both",
            Mode::Off => "off",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub mode: Mode,
    pub reserve_sat: u64,
    pub threshold_sat: u64,
    pub max_fee_sat: u64,
    pub max_fee_ppm: u64,
    pub interval_minutes: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self { mode: Mode::Auto, reserve_sat: 10_000, threshold_sat: 50_000, max_fee_sat: 100, max_fee_ppm: 5_000, interval_minutes: 1_440 }
    }
}

impl Settings {
    pub fn check(&self) -> Result<(), String> {
        if self.reserve_sat > MAX_SAT || self.threshold_sat > MAX_SAT {
            return Err("reserve_sat and threshold_sat are at most 21,000,000 BTC".into());
        }
        if self.threshold_sat == 0 {
            return Err("threshold_sat is at least 1".into());
        }
        if self.max_fee_ppm > MAX_FEE_PPM {
            return Err(format!("max_fee_ppm is at most {MAX_FEE_PPM}"));
        }
        if self.max_fee_sat > MAX_FEE_SAT {
            return Err(format!("max_fee_sat is at most {MAX_FEE_SAT}"));
        }
        if !(MIN_INTERVAL_MINUTES..=MAX_INTERVAL_MINUTES).contains(&self.interval_minutes) {
            return Err(format!("interval_minutes is {MIN_INTERVAL_MINUTES} to {MAX_INTERVAL_MINUTES}"));
        }
        Ok(())
    }

    /// The gateway fee cap for a Lightning payout of `amount_msat`: the smaller of the absolute cap
    /// and the proportional one.
    pub fn fee_cap_msat(&self, amount_msat: u64) -> u64 {
        let ppm = (amount_msat as u128 * self.max_fee_ppm as u128 / 1_000_000) as u64;
        ppm.min(self.max_fee_sat.saturating_mul(1000))
    }

    pub fn interval_ms(&self) -> u64 {
        self.interval_minutes.saturating_mul(60_000)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "mode": self.mode.name(), "reserve_sat": self.reserve_sat, "threshold_sat": self.threshold_sat,
            "max_fee_sat": self.max_fee_sat, "max_fee_ppm": self.max_fee_ppm, "interval_minutes": self.interval_minutes,
        })
    }
}

/// A Lightning address as it was resolved, and (for the destination) confirmed by the merchant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Destination {
    /// `name@domain`, lowercase.
    pub address: String,
    /// The LNURL callback's host when the merchant confirmed: a payout to any other host stops.
    pub callback_host: String,
    pub min_sendable_msat: u64,
    pub max_sendable_msat: u64,
    /// The metadata's `text/plain`, shown to the merchant.
    pub description: String,
    #[serde(default)]
    pub confirmed_at_ms: u64,
    /// Set when a payout found the service no longer matching what was confirmed: no payout goes
    /// there until the merchant confirms again.
    #[serde(default)]
    pub problem: Option<String>,
}

impl Destination {
    pub fn to_json(&self) -> Value {
        json!({
            "address": self.address, "callback_host": self.callback_host, "description": self.description,
            "min_sendable_sat": self.min_sendable_msat.div_ceil(1000), "max_sendable_sat": self.max_sendable_msat / 1000,
            "confirmed_at_ms": self.confirmed_at_ms, "problem": self.problem,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDestination {
    pub dest: Destination,
    /// Hex; the merchant's confirm names it.
    pub token: String,
    pub expires_at_ms: u64,
}

impl PendingDestination {
    pub fn to_json(&self) -> Value {
        let mut v = self.dest.to_json();
        let o = v.as_object_mut().expect("an object");
        o.remove("confirmed_at_ms");
        o.remove("problem");
        o.insert("confirm_token".into(), json!(self.token));
        o.insert("expires_at_ms".into(), json!(self.expires_at_ms));
        v
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Lightning,
    Ecash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Auto,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Lightning: recorded; asking the service for an invoice. Nothing can be funded yet.
    Resolving,
    /// Lightning: the checked invoice is saved; nothing funded.
    Invoiced,
    /// Lightning: saved before the wallet pays; the wallet's own record of the invoice settles it.
    Funding,
    Paid,
    /// Nothing left the wallet, or it came back.
    Failed,
    /// eCash: saved (with the tag the wallet's spend carries) before the wallet spends.
    Exporting,
    /// eCash: the notes are out of the wallet and kept here (also once the merchant marks them
    /// delivered: this may be the only copy, so it is never wiped on the merchant's word).
    Exported,
    /// eCash: saved before the wallet reissues the notes back (tagged); settled from the wallet's
    /// own reissue operation.
    Reclaiming,
    /// eCash: the wallet's reissue of the notes finished: the value is back in the wallet.
    Reclaimed,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Resolving => "resolving",
            State::Invoiced => "invoiced",
            State::Funding => "funding",
            State::Paid => "paid",
            State::Failed => "failed",
            State::Exporting => "exporting",
            State::Exported => "exported",
            State::Reclaiming => "reclaiming",
            State::Reclaimed => "reclaimed",
        }
    }

    /// Not settled: the value may still leave the wallet.
    pub fn is_open(self) -> bool {
        matches!(self, State::Resolving | State::Invoiced | State::Funding | State::Exporting)
    }

    /// The worker drives it: open, or a reclaim in flight (value coming back, never leaving).
    pub fn needs_drive(self) -> bool {
        self.is_open() || self == State::Reclaiming
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub kind: Kind,
    pub origin: Origin,
    #[serde(default)]
    pub request_id: Option<String>,
    pub state: State,
    pub amount_msat: u64,
    /// Lightning: the most the gateway may take.
    #[serde(default)]
    pub fee_cap_msat: u64,
    /// What is held back for the federation's own fees while the payout is open.
    #[serde(default)]
    pub fee_allowance_msat: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub destination_confirmed_at_ms: Option<u64>,
    /// The invoice, saved before funding. Carries the payment secret: never answered or logged.
    #[serde(default)]
    pub bolt11: Option<String>,
    #[serde(default)]
    pub payment_hash: Option<String>,
    #[serde(default)]
    pub payee: Option<String>,
    #[serde(default)]
    pub operation_id: Option<String>,
    #[serde(default)]
    pub fee_paid_msat: Option<u64>,
    /// eCash: the notes, until the wallet's own reissue of them finished. A bearer secret: never
    /// logged.
    #[serde(default)]
    pub notes: Option<String>,
    /// eCash: when the merchant marked the notes handed over. Notes are kept.
    #[serde(default)]
    pub delivered_at_ms: Option<u64>,
    /// eCash: the wallet's reissue operation of a reclaim, hex.
    #[serde(default)]
    pub reclaim_operation_id: Option<String>,
    /// Federation fees the wallet's operation paid, read from its record after the fact.
    #[serde(default)]
    pub federation_fee_msat: Option<u64>,
    #[serde(default)]
    pub detail: Option<String>,
}

impl Record {
    pub fn new(kind: Kind, origin: Origin, request_id: Option<String>, amount_msat: u64, now_ms: u64) -> Self {
        let state = match kind {
            Kind::Lightning => State::Resolving,
            Kind::Ecash => State::Exporting,
        };
        Self {
            id: format!("po_{}", hex::encode(rand::random::<[u8; 8]>())),
            kind,
            origin,
            request_id,
            state,
            amount_msat,
            fee_cap_msat: 0,
            fee_allowance_msat: 0,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            address: None,
            destination_confirmed_at_ms: None,
            bolt11: None,
            payment_hash: None,
            payee: None,
            operation_id: None,
            fee_paid_msat: None,
            notes: None,
            delivered_at_ms: None,
            reclaim_operation_id: None,
            federation_fee_msat: None,
            detail: None,
        }
    }

    /// The tag the wallet's reissue for a reclaim of this payout carries.
    pub fn reclaim_tag(&self) -> String {
        format!("fedimintcard-payout-reclaim:{}", self.id)
    }

    /// The tag the wallet's eCash spend for this payout carries in its operation meta: what finds
    /// the notes in the wallet's operation log after a crash.
    pub fn export_tag(&self) -> String {
        format!("fedimintcard-payout:{}", self.id)
    }

    /// What this payout holds back while it is open: the amount and every fee it may still pay.
    pub fn held_msat(&self) -> u64 {
        if self.state.is_open() { self.amount_msat.saturating_add(self.fee_cap_msat).saturating_add(self.fee_allowance_msat) } else { 0 }
    }

    /// Ordinary status and history responses never carry bearer notes.
    pub fn summary_json(&self) -> Value {
        self.view(false)
    }

    pub fn to_json(&self) -> Value {
        self.view(true)
    }

    fn view(&self, include_notes: bool) -> Value {
        let mut v = json!({
            "id": self.id,
            "kind": match self.kind { Kind::Lightning => "lightning", Kind::Ecash => "ecash" },
            "origin": match self.origin { Origin::Auto => "auto", Origin::Manual => "manual" },
            "request_id": self.request_id,
            "state": self.state.name(),
            "amount_sat": self.amount_msat / 1000,
            "created_at_ms": self.created_at_ms,
            "updated_at_ms": self.updated_at_ms,
        });
        if self.kind == Kind::Lightning {
            v["address"] = json!(self.address);
            v["payment_hash"] = json!(self.payment_hash);
            v["fee_cap_sat"] = json!(self.fee_cap_msat / 1000);
            if let Some(f) = self.fee_paid_msat {
                v["fee_paid_sat"] = json!(f.div_ceil(1000));
            }
        }
        if self.kind == Kind::Ecash {
            v["delivered_at_ms"] = json!(self.delivered_at_ms);
            if include_notes
                && matches!(self.state, State::Exported | State::Reclaiming)
                && let Some(n) = &self.notes
            {
                v["notes"] = json!(n);
            }
        }
        if let Some(f) = self.federation_fee_msat {
            v["federation_fee_sat"] = json!(f.div_ceil(1000));
        }
        if let Some(d) = &self.detail {
            v["detail"] = json!(d);
        }
        v
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PayoutFile {
    pub version: u32,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub destination: Option<Destination>,
    #[serde(default)]
    pub pending: Option<PendingDestination>,
    #[serde(default)]
    pub records: Vec<Record>,
    /// When the next automatic payout may start.
    #[serde(default)]
    pub next_auto_at_ms: u64,
    /// Set when a payout's federation fees, read from the wallet after the fact, came to more than
    /// was held back for them: no new payout starts until the merchant resumes (`resume`).
    #[serde(default)]
    pub halted: Option<String>,
}

pub struct PayoutStore {
    path: PathBuf,
    file: PayoutFile,
}

impl PayoutStore {
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join(PAYOUT_FILE)
    }

    /// Open `payout.json` in `dir`; a missing file is an empty store. A newer version, or a file
    /// that does not parse, is an error: nothing may be decided from a record this build cannot
    /// read.
    pub fn open(dir: &Path) -> Result<Self> {
        let path = Self::path_in(dir);
        let file = match std::fs::read(&path) {
            Ok(b) => {
                let f: PayoutFile = serde_json::from_slice(&b).with_context(|| format!("parsing {}", path.display()))?;
                if f.version > PAYOUT_VERSION {
                    bail!("{} has version {}, this build understands {PAYOUT_VERSION}", path.display(), f.version);
                }
                f
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PayoutFile { version: PAYOUT_VERSION, ..Default::default() },
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path, file })
    }

    pub fn file(&self) -> &PayoutFile {
        &self.file
    }

    pub fn get(&self, id: &str) -> Option<&Record> {
        self.file.records.iter().find(|r| r.id == id)
    }

    pub fn by_request(&self, request_id: &str) -> Option<&Record> {
        self.file.records.iter().find(|r| r.request_id.as_deref() == Some(request_id))
    }

    pub fn open_records(&self) -> impl Iterator<Item = &Record> {
        self.file.records.iter().filter(|r| r.state.is_open())
    }

    /// Whether any payout ever carried this payment hash: an invoice is used once.
    pub fn hash_seen(&self, payment_hash: &str, except: &str) -> bool {
        self.file.records.iter().any(|r| r.id != except && r.payment_hash.as_deref() == Some(payment_hash))
    }

    /// Apply `f` and save. When the save fails, memory stays as the file is.
    pub fn commit(&mut self, f: impl FnOnce(&mut PayoutFile)) -> Result<()> {
        let before = self.file.clone();
        f(&mut self.file);
        self.file.version = PAYOUT_VERSION;
        if let Err(e) = write_private_json(&self.path, &self.file) {
            self.file = before;
            return Err(e);
        }
        Ok(())
    }

    pub fn insert(&mut self, rec: Record) -> Result<()> {
        self.commit(|f| f.records.push(rec))
    }

    /// Replace the record with `rec.id` and save.
    pub fn put(&mut self, rec: &Record) -> Result<()> {
        if self.get(&rec.id).is_none() {
            bail!("no payout {}", rec.id);
        }
        self.commit(|f| {
            if let Some(r) = f.records.iter_mut().find(|r| r.id == rec.id) {
                *r = rec.clone();
            }
        })
    }

    /// Newest first.
    pub fn recent(&self, n: usize) -> Vec<&Record> {
        self.file.records.iter().rev().take(n).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_save_leaves_memory_as_the_file_is() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = PayoutStore::open(dir.path()).unwrap();
        s.insert(Record::new(Kind::Lightning, Origin::Manual, Some("a".into()), 1_000, 1)).unwrap();
        // A directory where the file goes: the save fails.
        let blocked = tempfile::tempdir().unwrap();
        std::fs::create_dir(PayoutStore::path_in(blocked.path())).unwrap();
        let mut b = PayoutStore { path: PayoutStore::path_in(blocked.path()), file: s.file.clone() };
        assert!(b.insert(Record::new(Kind::Ecash, Origin::Manual, None, 2_000, 2)).is_err());
        assert_eq!(b.file.records.len(), 1);
        let again = PayoutStore::open(dir.path()).unwrap();
        assert_eq!(again.file.records.len(), 1);
        assert!(again.by_request("a").is_some());
    }

    #[test]
    fn a_newer_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(PayoutStore::path_in(dir.path()), br#"{"version": 99}"#).unwrap();
        assert!(PayoutStore::open(dir.path()).is_err());
        std::fs::write(PayoutStore::path_in(dir.path()), b"not json").unwrap();
        assert!(PayoutStore::open(dir.path()).is_err());
    }

    #[test]
    fn the_fee_cap_is_the_smaller_of_the_two() {
        let s = Settings { max_fee_sat: 100, max_fee_ppm: 5_000, ..Settings::default() };
        assert_eq!(s.fee_cap_msat(1_000_000), 5_000); // 0.5% of 1,000 sat
        assert_eq!(s.fee_cap_msat(1_000_000_000), 100_000); // the absolute cap
        assert!(Settings { interval_minutes: 5, ..Settings::default() }.check().is_err());
        assert!(Settings { threshold_sat: 0, ..Settings::default() }.check().is_err());
        assert!(Settings::default().check().is_ok());
    }

    #[test]
    fn open_payouts_hold_their_amount_and_fees() {
        let mut r = Record::new(Kind::Lightning, Origin::Auto, None, 10_000, 1);
        r.fee_cap_msat = 50;
        r.fee_allowance_msat = 7;
        assert_eq!(r.held_msat(), 10_057);
        r.state = State::Paid;
        assert_eq!(r.held_msat(), 0);
        let mut e = Record::new(Kind::Ecash, Origin::Manual, None, 10_000, 1);
        e.notes = Some("secret".into());
        assert!(e.to_json().get("notes").is_none(), "not while exporting");
        e.state = State::Exported;
        assert_eq!(e.to_json()["notes"], "secret");
        assert!(e.summary_json().get("notes").is_none());
        e.delivered_at_ms = Some(5);
        assert_eq!(e.to_json()["notes"], "secret", "kept once delivered");
        e.state = State::Reclaimed;
        assert!(e.to_json().get("notes").is_none());
        assert!(!State::Reclaiming.is_open() && State::Reclaiming.needs_drive());
    }
}
