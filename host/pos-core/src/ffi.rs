//! The interface the POS apps call (feature `ffi`; UniFFI, Swift module `FedimintCardPosCore`,
//! Kotlin package `org.fedimintcard.poscore`; docs/POS-OFFLINE.md § The apps' interface).
//!
//! The app gives the core two things: an [`ApduTransport`] over the phone's NFC session with the
//! card, and a directory in its protected storage for the [`PosCore`] store. Every call blocks
//! until it is done; `charge` waits on the card, so the app calls it off the NFC delegate's queue
//! (iOS) or the NFC callback's thread (Android), never on them.
//!
//! Every method but `phone_id` returns a `Result` (Swift `throws`): a panic in the core is caught
//! and answered as [`PosCoreError::Internal`], never a trap in the app (Swift calls an export that
//! does not throw with `try!`).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use fedimint_card_host_card::{BridgeError, CardClient, SELECT_APDU, Transport};

use crate::charge::Taken;
use crate::refusal::refusal;
use crate::store::{self, Store};

/// The phone's NFC session with the card: `transmit` sends one command APDU and answers the
/// card's response, its data followed by the two status bytes. An error is the card leaving the
/// field (the charge answers `no_card`).
#[uniffi::export(with_foreign)]
pub trait ApduTransport: Send + Sync {
    fn transmit(&self, apdu: Vec<u8>) -> Result<Vec<u8>, TransportError>;
}

/// Why the NFC session could not carry an APDU.
#[derive(Debug, uniffi::Error)]
pub enum TransportError {
    Lost { reason: String },
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let TransportError::Lost { reason } = self;
        write!(f, "the card left the field: {reason}")
    }
}

impl std::error::Error for TransportError {}

impl From<uniffi::UnexpectedUniFFICallbackError> for TransportError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        TransportError::Lost { reason: e.reason }
    }
}

/// The card behind the app's transport, as the card crate's `Transport`: it selects the applet
/// before its first command (so the card hears nothing until the core has checked the kit and
/// the request), and a failed APDU is [`BridgeError::NoCard`].
struct Nfc {
    t: Arc<dyn ApduTransport>,
    selected: bool,
}

impl Transport for Nfc {
    fn transmit(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        let lost = |e: TransportError| anyhow::Error::new(BridgeError::NoCard(e.to_string()));
        if !self.selected {
            // A card without the applet answers a status word; its first command then says so.
            self.t.transmit(SELECT_APDU.to_vec()).map_err(lost)?;
            self.selected = true;
        }
        self.t.transmit(apdu.to_vec()).map_err(lost)
    }
}

/// The store could not be opened or written, the till's answer was not one, or the core failed
/// in a way it does not know (a panic, caught: `Internal`).
#[derive(Debug, uniffi::Error)]
#[uniffi(flat_error)]
pub enum PosCoreError {
    Store(String),
    Answer(String),
    Internal(String),
}

impl std::fmt::Display for PosCoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PosCoreError::Store(m) | PosCoreError::Answer(m) | PosCoreError::Internal(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for PosCoreError {}

/// A card slot and an amount, as the till API shows them.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SlotMsat {
    pub slot: u8,
    pub msat: u64,
}

/// How a charge ended.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ChargeOutcome {
    /// Paid offline: the signed transaction waits in the store for the till. `replayed`: the same
    /// sale (`client_ref`) taken before, or one a card taken away mid-tap left and this tap
    /// finished.
    Paid {
        txid: String,
        price_msat: u64,
        /// What the till gets (its outputs): the price and the overpay.
        till_msat: u64,
        overpay_msat: u64,
        fees_msat: u64,
        fee_margin_msat: u64,
        inputs: Vec<SlotMsat>,
        change: Vec<SlotMsat>,
        replayed: bool,
    },
    /// Not charged: `code` is the till API's (docs/TILL-API.md); a code the app does not know is a
    /// card error. The prices are `offline_no_change_room`'s and `offline_pool_exhausted`'s;
    /// `tries_left` is `wrong_pin`'s.
    Refused {
        code: String,
        detail: String,
        suggested_msat: Option<u64>,
        below_msat: Option<u64>,
        above_msat: Option<u64>,
        tries_left: Option<u8>,
    },
}

/// What the store holds: "N payments waiting to sync" is `waiting`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct PhoneStats {
    pub waiting: u32,
    pub waiting_msat: u64,
    pub signing: u32,
    pub refused: u32,
    pub synced: u32,
}

/// A charge the till refused at sync, until `dismiss`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RefusedCharge {
    pub txid: String,
    pub code: String,
    pub price_msat: u64,
    pub ts_unix_ms: u64,
}

/// `body` is what to `POST /v1/offline/charges` (empty `charges` when nothing waits); `txids` the
/// charges in it; `refused` the charges the till refused, to list.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Drain {
    pub body: String,
    pub txids: Vec<String>,
    pub refused: Vec<RefusedCharge>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct SyncReport {
    pub synced: u32,
    pub refused: u32,
    pub retry: u32,
}

/// The phone's offline store and charge.
#[derive(uniffi::Object)]
pub struct PosCore {
    store: Store,
}

fn store_error(e: anyhow::Error) -> PosCoreError {
    PosCoreError::Store(format!("{e:#}"))
}

/// Run `f`, answering a panic in it as [`PosCoreError::Internal`]. The store stays usable: its
/// queue is never half-changed (`Queue::commit`) and its locks are taken back after a panic.
fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T, PosCoreError>) -> Result<T, PosCoreError> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|p| {
        let m = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
        Err(PosCoreError::Internal(format!("{what}: the core failed unexpectedly ({m}); nothing more is known")))
    })
}

fn slots(v: impl Iterator<Item = (u8, u64)>) -> Vec<SlotMsat> {
    v.map(|(slot, msat)| SlotMsat { slot, msat }).collect()
}

#[uniffi::export]
impl PosCore {
    /// Open the store in `dir` (made if missing). Fails if it is open elsewhere.
    #[uniffi::constructor]
    pub fn open(dir: String) -> Result<Arc<Self>, PosCoreError> {
        guarded("open", || Ok(Arc::new(Self { store: Store::open(std::path::Path::new(&dir)).map_err(store_error)? })))
    }

    /// The id to ask the till's kit for (`GET /v1/offline/kit?phone=`). A field read: it cannot
    /// fail.
    pub fn phone_id(&self) -> String {
        self.store.phone_id().to_owned()
    }

    /// Take `price_msat` from the card on `transport`, offline, with the kit the till answered
    /// (`kit_json`, the kit or the whole `{"ok":true,"kit":…}`), for the sale `client_ref` (1 to 128
    /// ASCII characters, the same on a retry) and the holder's `pin`. Blocks on the card, and on a
    /// charge already running on this store. A refusal is `Refused`; an error is the core failing
    /// unexpectedly (`Internal`), the sale's outcome unknown as for `no_card`.
    pub fn charge(
        &self,
        transport: Arc<dyn ApduTransport>,
        kit_json: String,
        price_msat: u64,
        client_ref: String,
        pin: String,
    ) -> Result<ChargeOutcome, PosCoreError> {
        guarded("charge", || Ok(self.charge_now(transport, &kit_json, price_msat, &client_ref, &pin)))
    }

    pub fn stats(&self) -> Result<PhoneStats, PosCoreError> {
        guarded("stats", || {
            let s: store::PhoneStats = self.store.stats();
            Ok(PhoneStats { waiting: s.waiting as u32, waiting_msat: s.waiting_msat, signing: s.signing as u32, refused: s.refused as u32, synced: s.synced as u32 })
        })
    }

    /// The `client_ref`s of sales a card taken away mid-tap left signing here. Retry such a sale
    /// only with `charge`, never on the till's `POST /v1/charge` (docs/POS-OFFLINE.md).
    pub fn signing_refs(&self) -> Result<Vec<String>, PosCoreError> {
        guarded("signing_refs", || Ok(self.store.signing_refs()))
    }

    /// The charges waiting for the till (at most `max`, and 50) as the body to post, and the ones
    /// it refused.
    pub fn drain(&self, max: u32) -> Result<Drain, PosCoreError> {
        guarded("drain", || {
            let d = self.store.drain(max as usize);
            Ok(Drain {
                body: d.body,
                txids: d.txids,
                refused: d
                    .refused
                    .into_iter()
                    .map(|r| RefusedCharge { txid: r.txid, code: r.code, price_msat: r.price_msat, ts_unix_ms: r.ts_unix_ms })
                    .collect(),
            })
        })
    }

    /// Take the till's answer to the drained body (`200 {"ok":true,"results":[…]}`).
    pub fn mark_synced(&self, answer_json: String) -> Result<SyncReport, PosCoreError> {
        guarded("mark_synced", || {
            let r = self.store.mark_synced(&answer_json).map_err(|e| PosCoreError::Answer(format!("{e:#}")))?;
            Ok(SyncReport { synced: r.synced as u32, refused: r.refused as u32, retry: r.retry as u32 })
        })
    }

    /// Remove a refused charge once the app has shown it; only a refused one. Answers whether
    /// there was one.
    pub fn dismiss(&self, txid: String) -> Result<bool, PosCoreError> {
        guarded("dismiss", || self.store.dismiss(&txid).map_err(store_error))
    }
}

impl PosCore {
    fn charge_now(&self, transport: Arc<dyn ApduTransport>, kit_json: &str, price_msat: u64, client_ref: &str, pin: &str) -> ChargeOutcome {
        let mut card = CardClient::new(Nfc { t: transport, selected: false });
        match self.store.charge(&mut card, kit_json, price_msat, client_ref, pin.as_bytes()) {
            Ok(Taken { charge: c, replayed }) => ChargeOutcome::Paid {
                txid: c.txid.clone(),
                price_msat: c.price_msat,
                till_msat: c.till_msat,
                overpay_msat: c.overpay_msat,
                fees_msat: c.fees_msat,
                fee_margin_msat: c.fee_margin_msat,
                inputs: slots(c.inputs.iter().map(|i| (i.slot, i.amount_msat))),
                change: slots(c.change.iter().map(|i| (i.slot, i.amount_msat))),
                replayed,
            },
            Err(e) => {
                let r = refusal(&e);
                ChargeOutcome::Refused {
                    code: r.code.to_owned(),
                    detail: r.detail,
                    suggested_msat: r.suggested_msat,
                    below_msat: r.below_msat,
                    above_msat: r.above_msat,
                    tries_left: r.tries_left,
                }
            }
        }
    }
}
