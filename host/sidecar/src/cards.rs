//! The provisioning endpoints: `precheck`, `keySetup`, `issue` (plan § 4).
//!
//! These are the card-side half of shape B — the app installs the applet over NFC, then asks the
//! operator's machine to make the card useful. Everything that touches money stays here; the
//! app never sees a key and the registry never sees a note.
//!
//! Two rules hold across all three:
//!
//! * **One job at a time.** The card can be held by one session only, so each endpoint runs
//!   through [`TillHandle`]'s single actor rather than opening the card itself. A second request
//!   while one is in flight is answered `busy` (or, for a replayed `client_ref`, its result).
//! * **State before the federation hears of it.** `issue` reads this off the wallet's own
//!   `PendingTx` record: the record carrying the `client_ref` is written in the same save that
//!   creates it, before anything is signed, so a repeat of the ref can always find its
//!   transaction — including after a crash.
//!
//! `precheck` is deliberately read-only: it is the gate the app shows *before* it writes
//! anything, so it must answer from what is on the card without touching `card_state.json`.

use anyhow::{Context, Result, anyhow};
use fedimint_card_host_card::{
    PIN_MAX, PIN_MIN, SLOTS, SW_CLA_NOT_SUPPORTED, SW_CONDITIONS_NOT_SATISFIED, SW_INS_NOT_SUPPORTED,
    SW_NO_PIN_SET, SW_PIN_BLOCKED, SW_PIN_REQUIRED, status_word,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::glue::{CardWallet, SpendFault, on_card};
use crate::plan;
use crate::state::NoteStatus;

/// Applet status words that mean something specific to a provisioner.
const SW_FILE_NOT_FOUND: u16 = 0x6A82;

/// What the card in the field is, as far as a provisioner cares.
///
/// The three names are the plan's: a card this wallet can write to, one already carrying keys it
/// produced, and one holding keys it did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKind {
    /// Every slot answers `6985`: nothing has been generated.
    Blank,
    /// Every slot has a key, and they are the ones this wallet recorded.
    Installed,
    /// A slot answers with a key this wallet never produced — some other machine's card, or one
    /// re-keyed elsewhere. Never written to.
    Foreign,
}

impl CardKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CardKind::Blank => "blank",
            CardKind::Installed => "installed",
            CardKind::Foreign => "foreign",
        }
    }
}

/// What `precheck` found. Read-only: computed from the card and the state file, saved nowhere.
#[derive(Debug, Clone)]
pub struct Precheck {
    pub kind: CardKind,
    /// Keys present on the card, slot and hex public key, in slot order.
    pub keys: Vec<(u8, String)>,
    /// Slots that answer `6985` — where `card setup` would generate.
    pub empty_slots: Vec<u8>,
    /// Slots holding a key this state file did not produce.
    pub foreign_slots: Vec<u8>,
    /// The ISD will answer: `GET DATA` reached it and one of the status words it can give back
    /// came home. A transport failure is an error, not a closed ISD.
    pub isd_open: bool,
    /// Slots this wallet may issue to right now (no notes on the current key, no interrupted
    /// overwrite).
    pub free_slots: Vec<u8>,
    /// Set when the applet answered `6983`/`6A94`/`6984` — a live applet whose PIN state we know,
    /// which a blank card cannot produce.
    pub applet: AppletState,
    /// SELECT of our AID answered `0200`. The app's `applet_present`. Distinct from `applet`:
    /// a card can select (it is installed) while the PIN probes say nothing useful.
    pub applet_present: bool,
    /// Notes this wallet recorded on the keys the card is holding: what a sweep would destroy.
    /// Read from the state file, never asked of the federation, so precheck stays instant.
    pub live_notes: Vec<(u8, u64)>,
}

/// The applet's PIN state, as far as one read can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppletState {
    /// No applet on this card selected, or it is not installed: the slot reads gave us nothing
    /// but `6985`, so we cannot say a PIN exists.
    Unknown,
    /// `6984`: written but no PIN yet — `keySetup` must set one.
    NotSet,
    /// `6983`: three wrong PINs; it will never sign again.
    Blocked,
    /// `6A94`: a PIN is set and this session has not given it.
    Set,
}

impl AppletState {
    pub fn as_str(self) -> &'static str {
        match self {
            AppletState::Unknown => "unknown",
            AppletState::NotSet => "not_set",
            AppletState::Blocked => "blocked",
            AppletState::Set => "set",
        }
    }
}

impl Precheck {
    /// The app's `CardPrecheck` (`apps/provisioner/src/api.ts`) plus the detail a human needs to
    /// say *why*. The app reads only the first five fields; the rest is for the CLI and the logs.
    pub fn to_json(&self) -> Value {
        let live_msat: u64 = self.live_notes.iter().map(|(_, a)| *a).sum();
        json!({
            "ok": true,
            "applet_present": self.applet_present,
            "isd_open": self.isd_open,
            "keyed_slots": self.keys.iter().map(|(s, _)| *s).collect::<Vec<u8>>(),
            "live_msat": live_msat / 1000,
            "live_msat_exact": live_msat,
            "safe_to_replace": self.kind == CardKind::Blank,
            "kind": self.kind.as_str(),
            "applet": self.applet.as_str(),
            "empty_slots": self.empty_slots,
            "foreign_slots": self.foreign_slots,
            "free_slots": self.free_slots,
            "live_notes": self.live_notes.iter().map(|(s, a)| json!({"slot": s, "msat": a})).collect::<Vec<_>>(),
            "slots": SLOTS,
        })
    }
}

/// Read the card in `cw.card` without writing anything. `cw` must already hold an open card.
///
/// Every slot is asked its public key: `6985` means empty, a key means present. A key that the
/// state file has no record of — or has a different one for — is foreign. The ISD probe is a
/// `GET DATA` that any card answers with a status word or a transport error; only the *error*
/// path is a closed reader, so an installed-but-refusing card still reports `isd_open`.
pub fn precheck(cw: &mut CardWallet) -> Result<Precheck> {
    let mut keys = vec![];
    let mut empty = vec![];
    let mut foreign = vec![];
    let mut applet = AppletState::Unknown;

    // SELECT decides whether the slot walk is meaningful at all: a card without our applet has no
    // dispatcher for CLA `B0`, so every GET_PUB answers `6E00` (CLA not supported) and the walk
    // would mistake "no applet" for a transport failure and answer 502. A blank card is a normal
    // state — the first one a batch meets — so it is reported, not refused.
    let applet_present = on_card(|| cw.card.select_applet()).is_ok();
    if applet_present {
        for slot in 0..SLOTS {
        match on_card(|| cw.card.get_pub(slot)) {
            Ok(pk) => {
                let hexpk = pk.to_string();
                let known = cw.store.state.slot_pubkey(slot) == Some(hexpk.as_str());
                if !known {
                    foreign.push(slot);
                }
                keys.push((slot, hexpk));
            }
            Err(e) => match status_word(&e) {
                Some(SW_CONDITIONS_NOT_SATISFIED) | Some(SW_FILE_NOT_FOUND) => empty.push(slot),
                // No dispatcher for `B0` at all: a card with no applet, so every slot is empty.
                Some(SW_CLA_NOT_SUPPORTED) | Some(SW_INS_NOT_SUPPORTED) => empty.push(slot),
                // A slot answered a PIN word: the applet is live and this is its PIN state.
                Some(SW_PIN_BLOCKED) => {
                    applet = AppletState::Blocked;
                    empty.push(slot);
                }
                Some(SW_PIN_REQUIRED) => {
                    applet = AppletState::Set;
                    empty.push(slot);
                }
                Some(SW_NO_PIN_SET) => {
                    applet = AppletState::NotSet;
                    empty.push(slot);
                }
                _ => return Err(e),
            },
        }
        }
    }

    let kind = if !foreign.is_empty() {
        CardKind::Foreign
    } else if !keys.is_empty() && empty.is_empty() {
        CardKind::Installed
    } else if keys.is_empty() {
        CardKind::Blank
    } else {
        // Partly keyed: a card whose setup was interrupted. It is ours (no foreign key), so the
        // app may finish it — `card setup` generates into the empty slots and leaves the rest.
        CardKind::Blank
    };

    // One probe of the card manager, no SELECT first: data or a status word proves it answered.
    // Only `6A82`/`6A88` mean "the ISD is there but has no such file" — anything else that is not
    // data is a refusal, and reporting a refusal as `isd_open` would send the app into a load the
    // card will reject.
    let isd = on_card(|| cw.card.get_data_ff21());
    let isd_open = isd.is_ok()
        || matches!(
            isd.err().as_ref().and_then(status_word),
            Some(SW_FILE_NOT_FOUND) | Some(0x6A88)
        );

    let live_notes: Vec<(u8, u64)> = cw
        .store
        .state
        .notes
        .iter()
        .filter(|n| matches!(n.status, NoteStatus::Live | NoteStatus::Pending))
        .map(|n| (n.slot, n.amount_msat))
        .collect();

    Ok(Precheck {
        kind,
        keys,
        empty_slots: empty,
        foreign_slots: foreign,
        isd_open,
        free_slots: cw.store.state.free_slots(),
        applet,
        applet_present,
        live_notes,
    })
}

/// `POST /v1/cards/keySetup`: `card setup`, then `SET_PIN`, and a `VERIFY_PIN` to prove it.
///
/// Idempotent in the way that matters to a batch: running it on a card that already has keys and
/// a PIN changes nothing and answers the same keys. A card with keys but no PIN gets the PIN.
///
/// The PIN is optional. A card with keys and no PIN is usable for issuing but cannot spend; the
/// plan records that as `pinNotSet` and refuses to hand it out, so the endpoint reports it rather
/// than failing the whole tap.
pub struct KeySetupOutcome {
    pub keys: Vec<(u8, String)>,
    pub short_id: String,
    /// `set` (already had one), `set_now` (this call set it), `not_set` (none asked for or the
    /// card refused), `blocked`.
    pub pin: PinState,
    /// Set when a PIN was given but `VERIFY_PIN` refused it.
    pub pin_verified: Option<bool>,
    /// Slots keyed by this call rather than found already present.
    pub generated: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinState {
    AlreadySet,
    SetNow,
    NotSet,
    Blocked,
}

impl PinState {
    pub fn as_str(self) -> &'static str {
        match self {
            PinState::AlreadySet => "set",
            PinState::SetNow => "set_now",
            PinState::NotSet => "not_set",
            PinState::Blocked => "blocked",
        }
    }
}

/// The identity a batch records: derived from slot 0's key, so two cards with the same key set
/// cannot collide. 8 hex chars of a hash of the full key set, which is what the app's screens
/// show and the registry stores.
pub fn short_id(keys: &[(u8, String)]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for (slot, pk) in keys {
        slot.hash(&mut h);
        pk.hash(&mut h);
    }
    format!("{:016x}", h.finish())[..8].to_string()
}

impl KeySetupOutcome {
    /// The app's `KeySetupResult` plus the detail: `pin_set` is true only when SET_PIN answered
    /// `9000` *on this call* or the PIN was already there and VERIFY_PIN accepted it.
    pub fn to_json(&self) -> Value {
        json!({
            "ok": true,
            "keys": self.keys.iter().map(|(s, k)| json!({"slot": s, "pubkey": k})).collect::<Vec<_>>(),
            "short_id": self.short_id,
            "pin_set": self.pin_verified == Some(true),
            "pin": self.pin.as_str(),
            "pin_verified": self.pin_verified,
            "generated": self.generated,
        })
    }
}

/// `card setup` over the wallet, recording which slots it generated.
///
/// [`CardWallet::card_setup`] already walks the slots and leaves present keys alone; this only
/// observes which ones were absent first so the reply can say what changed. Never `--fresh`:
/// re-keying destroys live notes, and a provisioner that did that would silently burn value.
pub fn key_setup(cw: &mut CardWallet, pin: Option<&[u8]>) -> Result<KeySetupOutcome> {
    let before: Vec<Option<String>> = (0..SLOTS)
        .map(|slot| match on_card(|| cw.card.get_pub(slot)) {
            Ok(pk) => Some(pk.to_string()),
            Err(e) if status_word(&e) == Some(SW_CONDITIONS_NOT_SATISFIED) => None,
            // No applet to dispatch `B0`: every slot is empty, because there are no slots yet.
            // Reading this as "a key we did not produce" refused the blank card — the one card a
            // batch always starts from — and named a slot that does not exist (`!6e00`).
            Err(e) if matches!(status_word(&e), Some(SW_CLA_NOT_SUPPORTED) | Some(SW_INS_NOT_SUPPORTED)) => None,
            Err(e) => Some(format!("!{}", status_word(&e).map(|w| format!("{w:04x}")).unwrap_or_else(|| e.to_string()))),
        })
        .collect();

    // Refuse a foreign card outright: `card_setup` would otherwise treat our empty record as
    // licence to re-key, and those keys may hold someone else's money.
    if let Some((slot, pk)) = before.iter().enumerate().find_map(|(i, k)| {
        let k = k.as_deref()?;
        (cw.store.state.slot_pubkey(i as u8) != Some(k)).then(|| (i, k))
    }) {
        return Err(anyhow::Error::new(SpendFault::WrongCard(format!(
            "slot {slot} holds a key this wallet did not produce ({})",
            &pk[..pk.len().min(16)]
        ))));
    }

    let keys: Vec<(u8, String)> = cw.card_setup(false)?.into_iter().map(|(s, pk)| (s, pk.to_string())).collect();
    let generated: Vec<u8> = before
        .iter()
        .enumerate()
        .filter(|(i, k)| k.is_none() && keys.iter().any(|(s, _)| *s == *i as u8))
        .map(|(i, _)| i as u8)
        .collect();

    let sid = short_id(&keys);
    let (pin_state, pin_verified) = match pin {
        None => (PinState::NotSet, None),
        Some(p) => set_pin(cw, p)?,
    };

    Ok(KeySetupOutcome { keys, short_id: sid, pin: pin_state, pin_verified, generated })
}

/// `SET_PIN` then `VERIFY_PIN`. The applet answers `6985` when a PIN already exists, which is the
/// idempotent case, and `6983` when it is out of tries, which is terminal.
fn set_pin(cw: &mut CardWallet, pin: &[u8]) -> Result<(PinState, Option<bool>)> {
    if !(PIN_MIN..=PIN_MAX).contains(&pin.len()) {
        return Err(anyhow!("PIN must be {PIN_MIN} to {PIN_MAX} digits, not {}", pin.len()));
    }
    let existing = match on_card(|| cw.card.set_pin(pin)) {
        Ok(()) => PinState::SetNow,
        Err(e) => match status_word(&e) {
            // The applet answers 6985 when a PIN already exists: nothing to do, and the card is
            // unchanged, which is what makes a re-run of keySetup safe.
            Some(SW_CONDITIONS_NOT_SATISFIED) => PinState::AlreadySet,
            Some(SW_PIN_BLOCKED) => return Ok((PinState::Blocked, Some(false))),
            w => return Err(e.context(format!("SET_PIN answered {}", w.map(|w| format!("{w:04X}")).unwrap_or_default()))),
        },
    };
    let verified = match on_card(|| cw.card.verify_pin(pin)) {
        Ok(()) => true,
        Err(e) => match status_word(&e) {
            // A wrong PIN or a blocked card: the card exists and is programmed, but it cannot
            // spend. Reported, not raised: the plan records `pinNotSet`.
            Some(SW_PIN_BLOCKED) | Some(0x6300..=0x63FF) => false,
            Some(_) => false,
            None => return Err(e).context("VERIFY_PIN"),
        },
    };
    Ok((existing, Some(verified)))
}

// ------------------------------------------------------------------ issue

/// Body of `POST /v1/cards/issue`.
#[derive(Debug, Deserialize)]
pub struct IssueReq {
    /// The app's `IssueRequest`. `amounts_msat` is the name the app and the plan's wire table use.
    #[serde(alias = "amount_msat")]
    pub amounts_msat: Vec<u64>,
    #[serde(default)]
    pub client_ref: Option<String>,
}

/// A finished issue: what was put on the card, or the plan that would be.
#[derive(Debug, Clone)]
pub struct IssueOutcome {
    pub client_ref: Option<String>,
    /// Satoshi held on the card after this call.
    pub live_msat: u64,
    pub txid: String,
    /// `(slot, msat)` of the notes this call created.
    pub notes: Vec<(u8, u64)>,
    /// What the plan would write, before the wallet was charged.
    pub plan: Value,
    /// True when this call found an earlier one under the same `client_ref`.
    pub replayed: bool,
}

impl IssueOutcome {
    /// The app's `IssueResult` plus the preview and the replay flag. `live_msat` is what the card
    /// holds now, in satoshi, so the summary can show it without a second call.
    pub fn to_json(&self) -> Value {
        json!({
            "ok": true,
            "client_ref": self.client_ref,
            "txid": self.txid,
            "notes": self.notes.iter().map(|(s, a)| json!({"slot": s, "msat": a})).collect::<Vec<_>>(),
            "live_msat": self.live_msat,
            "replayed": self.replayed,
            "plan": self.plan,
        })
    }
}

/// Preview a topup without doing it: the notes `amounts` becomes, the slots they land in, and
/// whether the wallet can fund it. This is what `issue` shows before committing, and it runs
/// before any state is written.
pub async fn issue_plan(cw: &CardWallet, amounts: &[u64]) -> Result<Value> {
    let notes: Vec<u64> = amounts.iter().flat_map(|a| plan::binary(*a)).collect();
    let m = cw.wallet.min_denom();
    let below: Vec<u64> = notes.iter().copied().filter(|a| *a < m).collect();
    let slots = cw.store.state.free_slots();
    let bal = cw.wallet.balance_msat().await?;
    let need: u64 = notes.iter().sum::<u64>() + notes.iter().map(|a| 2 * cw.wallet.fee(*a)).sum::<u64>();
    Ok(json!({
        "notes": notes,
        "slots": slots.iter().take(notes.len()).copied().collect::<Vec<u8>>(),
        "free_slots": slots.len(),
        "enough_slots": slots.len() >= notes.len(),
        "min_denom_msat": m,
        "below_min_denom": below,
        "wallet_balance_msat": bal,
        "needed_msat": need,
        "funded": bal >= need,
    }))
}

/// Notes this wallet recorded as live or pending on the card, whatever slot they sit in.
///
/// The question a provisioner asks is about the *card*, not the slots a particular plan would
/// use: a card holding one note in slot 0 and seven empty slots would otherwise accept a second
/// batch's notes in slots 1-7 and quietly be worth both. Slots are recycled, so "free" says
/// nothing about whether the card has already been issued to.
pub fn held_notes(cw: &CardWallet) -> Vec<(u8, u64)> {
    cw.store
        .state
        .notes
        .iter()
        .filter(|n| matches!(n.status, NoteStatus::Live | NoteStatus::Pending))
        .map(|n| (n.slot, n.amount_msat))
        .collect()
}
