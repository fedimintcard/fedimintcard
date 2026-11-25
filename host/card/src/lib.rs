//! Host side of the Fedimint spend-key card.
//!
//! The card holds secp256k1 keys in numbered slots and exposes three commands: `GEN_KEY`,
//! `GET_PUB` and `SIGN` (BIP340 over a 32-byte message). It never reveals a private key.
//!
//! * [`Card`] is the high-level API the federation tests use.
//! * [`CardClient`] implements it over any APDU [`Transport`] (bridge or PC/SC).
//! * [`SoftCard`] implements it in-process with real keys, for tests without a card.

use anyhow::{Context, Result, bail, ensure};
use secp256k1::schnorr::Signature;
use secp256k1::{Keypair, Message, SECP256K1, XOnlyPublicKey};

pub mod bridge;
#[cfg(feature = "pcsc")]
pub mod pcsc_backend;
pub mod sealed;
#[cfg(feature = "softkit")]
pub mod softkit;
pub mod v3;

pub use v3::{AppletVersion, CardStatus, Certificate, Opening, SlotInfo, SlotState};

pub use bridge::{BridgeError, BridgeTransport};
pub use sealed::{ClearPinRefused, PinKey, SealedCommand, TrustedCard, UntrustedCard, trust_card};

/// The most key slots an applet has (applet 2.1). The host never asks beyond it.
///
/// A card's own count comes from the card, never from this constant: walk `GET_PUB` up from slot
/// 0 until the applet answers [`SW_SLOT_OUT_OF_RANGE`] (see [`past_last_slot`]). Applet 2.0 cards,
/// prod card `15989f91` among them, have [`MIN_SLOTS`] and keep working.
pub const SLOTS: u8 = 32;
/// The first of the slots kept for change from offline sales: slots `CHANGE_SLOTS_FROM..SLOTS`
/// (24 to 31) take that change and nothing else, and slots `0..CHANGE_SLOTS_FROM` (0 to 23) take
/// loads and the change of online payments.
pub const CHANGE_SLOTS_FROM: u8 = 24;
/// Whether `slot` is one kept for change from offline sales ([`CHANGE_SLOTS_FROM`] and up).
pub const fn is_change_slot(slot: u8) -> bool {
    slot >= CHANGE_SLOTS_FROM && slot < SLOTS
}
/// Key slots on applet 2.0, the fewest any card we issued has. A card answering
/// [`SW_SLOT_OUT_OF_RANGE`] below this is not one of ours.
pub const MIN_SLOTS: u8 = 8;
/// Status word for a slot past the card's last one.
pub const SW_SLOT_OUT_OF_RANGE: u16 = 0x6B00;
/// `GEN_KEY` on a 2.1 card out of persistent memory: the slot's key objects could not be built
/// and the slot stays empty.
pub const SW_NO_SPACE: u16 = 0x6A84;
/// Status word the applet answers for an empty slot (or an occupied slot on GEN_KEY).
pub const SW_CONDITIONS_NOT_SATISFIED: u16 = 0x6985;
/// The applet's PIN bounds (`FedimintCardApplet.java:63-64`). A shorter or longer PIN is refused
/// before the card sees it, so a malformed one cannot spend a try.
pub const PIN_MIN: usize = 4;
pub const PIN_MAX: usize = 8;
/// Blocked after three wrong PINs; terminal. Applets 2.x and 3.0 only: a 4.0 card never answers
/// it (a wrong try makes the next one wait). Kept until the sidecar and pos-core read the wait.
pub const SW_PIN_BLOCKED: u16 = 0x6983;
/// A frame's P1 or P2 is not one the command takes (the same word as [`SW_SLOT_OUT_OF_RANGE`]).
pub const SW_WRONG_P1P2: u16 = 0x6B00;
/// Applet 4.0's PIN wait (`FedimintCardApplet.java` `countTry`, `waitOwed`): askings come in
/// stretches of this many, and the asking after a stretch writes what is still owed.
pub const PIN_STRETCH: u16 = 16;
/// The most askings a try can owe: from the fifteenth wrong try in a row on.
pub const PIN_OWED_MOST: u16 = 0x4000;
/// The wrong tries in a row stop counting here.
pub const PIN_WRONG_MOST: u8 = 15;
/// The most `6A98` answers one held VERIFY_PIN can draw: the cap's askings and the asking after
/// each of its stretches, the last of which is the try itself.
const MOST_KEEP_HOLDING: u32 = PIN_OWED_MOST as u32 + PIN_OWED_MOST as u32 / PIN_STRETCH as u32;
/// No PIN set on this card yet.
pub const SW_NO_PIN_SET: u16 = 0x6984;
/// A PIN is set and this session has not verified it.
pub const SW_PIN_REQUIRED: u16 = 0x6A94;
/// The card has no dispatcher for this CLA. On a J3R180 with no fedimint applet installed, every
/// `B0 xx` command answers this — including `GET_PUB`, which is why it means "no applet", not
/// "a key we do not know".
pub const SW_CLA_NOT_SUPPORTED: u16 = 0x6E00;
/// The applet is selected but does not implement this instruction.
pub const SW_INS_NOT_SUPPORTED: u16 = 0x6D00;

/// Upper bound on 61xx / 6Cxx follow-ups for one command.
const MAX_CHAIN: usize = 16;
const INS_GEN_KEY: u8 = 0x10;
const INS_GET_PUB: u8 = 0x11;
/// `SELECT` and `GET DATA` are ISO interindustry commands: CLA `00`.
const CLA_ISO: u8 = 0x00;
/// The applet's CLA.
pub const CLA: u8 = 0xB0;
pub const INS_VERIFY_PIN: u8 = 0x40;
pub const INS_SET_PIN: u8 = 0x41;
pub const INS_CHANGE_PIN: u8 = 0x42;
const INS_SIGN: u8 = 0x12;
/// `SELECT` by AID `F0 46 4D 43 41 52 44 01`.
pub const SELECT_APDU: [u8; 14] = [0x00, 0xA4, 0x04, 0x00, 0x08, 0xF0, 0x46, 0x4D, 0x43, 0x41, 0x52, 0x44, 0x01, 0x00];

/// Error carrying the card's status word, so callers can match on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusWord(pub u16);

impl std::fmt::Display for StatusWord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "card returned status word {:04X}", self.0)
    }
}
impl std::error::Error for StatusWord {}

/// A backend that cannot send raw APDUs (the trait's default `apdu`).
#[derive(Debug, Clone, Copy)]
pub struct NoRawApdu;

impl std::fmt::Display for NoRawApdu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("this card backend cannot send raw APDUs")
    }
}
impl std::error::Error for NoRawApdu {}

/// What the old-card error tells the holder (DESIGN § 7, the Setup app's copy).
pub const OLD_CARD_MESSAGE: &str = "This card has an older applet. Reset it and set it up again.";

/// SELECT answered `9000` but not as applet 4.0 does: two bytes, major 4. Nothing after such a
/// SELECT reaches the card's PIN commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsupportedApplet {
    /// A 2.x card (no version bytes), a 3.x one, or an answer that is not a version.
    Old { answer: Vec<u8> },
    /// A later major than this library knows.
    Newer(AppletVersion),
}

impl std::fmt::Display for UnsupportedApplet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Old { answer } => write!(f, "{OLD_CARD_MESSAGE} (SELECT answered {})", hex::encode(answer)),
            Self::Newer(v) => write!(f, "this card's applet is {v}, newer than this app knows (4.x); update the app"),
        }
    }
}

impl std::error::Error for UnsupportedApplet {}

impl UnsupportedApplet {
    /// The rule for every reader: `answer` is what a `9000` SELECT gave. Applet 4.x passes.
    pub fn check(answer: &[u8]) -> std::result::Result<AppletVersion, Self> {
        match AppletVersion::from_select(answer) {
            Some(v) if v.is_current() => Ok(v),
            Some(v) if v.major > AppletVersion::V4_0.major => Err(Self::Newer(v)),
            _ => Err(Self::Old { answer: answer.to_vec() }),
        }
    }
}

/// Whether `err` is the old-card error: the card's applet is older than 4.0.
pub fn is_old_card(err: &anyhow::Error) -> bool {
    matches!(err.downcast_ref::<UnsupportedApplet>(), Some(UnsupportedApplet::Old { .. }))
}

/// A PIN command on a [`CardClient`] that has not had a SELECT pass the rule for every reader.
/// Nothing was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotSelected;

impl std::fmt::Display for NotSelected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no PIN command before a SELECT that answers applet 4.x")
    }
}

impl std::error::Error for NotSelected {}

/// SIGN answered `6F00`: no signature passed the card's own check, so the card stored none and
/// the slot still holds its note. Nothing was spent; the holder can tap again. The error also
/// carries the status word ([`status_word`] gives `6F00`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignCheckFailed;

impl std::fmt::Display for SignCheckFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the card's signature failed its own check; nothing was spent, tap again")
    }
}

impl std::error::Error for SignCheckFailed {}

fn sign_check_failed() -> anyhow::Error {
    anyhow::Error::from(StatusWord(v3::SW_CRYPTO_ERROR)).context(SignCheckFailed)
}

/// Whether `err` is SIGN's `6F00`: the card refused its own signature and nothing was spent.
pub fn is_sign_check_failed(err: &anyhow::Error) -> bool {
    err.downcast_ref::<SignCheckFailed>().is_some()
}

/// Returns the status word if `err` came from the card refusing a command.
pub fn status_word(err: &anyhow::Error) -> Option<u16> {
    err.downcast_ref::<StatusWord>().map(|s| s.0)
}

/// Whether `err` is the card saying the slot asked for is past its last one: the end of a slot
/// walk, not a failure.
pub fn past_last_slot(err: &anyhow::Error) -> bool {
    status_word(err) == Some(SW_SLOT_OUT_OF_RANGE)
}

/// How many key slots the card in the field has: `GET_PUB` from slot 0 up until
/// [`SW_SLOT_OUT_OF_RANGE`], at most [`SLOTS`]. An empty slot (`6985`) counts; any other refusal
/// is returned. Fails on a card with fewer than [`MIN_SLOTS`].
pub fn slot_count(card: &mut dyn Card) -> Result<u8> {
    for slot in 0..SLOTS {
        match card.get_pub(slot) {
            Ok(_) => {}
            Err(e) if status_word(&e) == Some(SW_CONDITIONS_NOT_SATISFIED) => {}
            Err(e) if past_last_slot(&e) => {
                ensure!(slot >= MIN_SLOTS, "the card has only {slot} slot(s) (status 6B00 for slot {slot}); flash the {SLOTS}-slot applet");
                return Ok(slot);
            }
            Err(e) => return Err(e).with_context(|| format!("slot {slot}")),
        }
    }
    Ok(SLOTS)
}

/// The PIN the fixtures and tests provision on a card they own.
pub const TEST_PIN: &[u8] = b"1234";

/// The PIN this process will use, from `CARD_PIN`, falling back to [`TEST_PIN`].
///
/// A card provisioned by anyone else holds a PIN this crate cannot guess, and a guess costs a
/// retry (`63 CX`), so the operator has to name it. Defaulting to `TEST_PIN` keeps every test
/// and soft-card fixture working unchanged.
pub fn pin_from_env() -> Vec<u8> {
    match std::env::var("CARD_PIN") {
        Ok(p) => p.into_bytes(),
        Err(_) => TEST_PIN.to_vec(),
    }
}

/// The wait, served (DESIGN § 3, what a host does): `send` is the same VERIFY_PIN each time. On
/// applet 4.0 a try after wrong ones first owes askings: the card answers `6A98`
/// ([`v3::SW_KEEP_HOLDING`]) to each, and this sends again, with nothing between, until it answers
/// anything else, which it returns. `owed` is what GET_STATE said before the first asking.
#[doc(hidden)]
pub fn serve_wait(mut owed: u16, progress: &mut dyn FnMut(u16), send: &mut dyn FnMut() -> Result<()>) -> Result<()> {
    let mut asked = 0u16;
    progress(owed);
    for _ in 0..=MOST_KEEP_HOLDING {
        match send() {
            Err(e) if status_word(&e) == Some(v3::SW_KEEP_HOLDING) => {
                // The card's `waitOwed`, mirrored: askings 1..stretch, then the one that writes
                // what is left.
                let stretch = owed.min(PIN_STRETCH);
                if asked < stretch {
                    asked += 1;
                } else if owed > stretch {
                    asked = 0;
                    owed -= stretch;
                } else {
                    // The card would have tried the PIN at this asking.
                    bail!("the card answered 6A98 with no wait owed");
                }
                progress(owed - asked);
            }
            r => return r,
        }
    }
    bail!("the card kept answering 6A98 past the longest wait a PIN can owe")
}

/// The sealed PIN's paths take only a 4.x card. No P1 00 PIN is built here: a card older than 4.0
/// gets none from this library (the test-only `fedimint-card-old-card-pin` keeps that form).
fn require_current(card: &mut dyn Card) -> Result<()> {
    let v = card.applet_version()?;
    ensure!(v.is_current(), "the sealed PIN is applet 4.x's; this card's applet is {v}");
    Ok(())
}

/// VERIFY_PIN, sealed, with its wait served (applet 4.x, docs/OFFLINE-PROTOCOL.md § 9): GET_STATE
/// for the askings owed, then GET_NONCE and the PIN key checked under `trust`'s certificate, the
/// PIN sealed **once**, and the same bytes sent at every `6A98` until the card answers anything
/// else, which it returns: `Ok` for the right PIN, `63CX` for a wrong one (X the wrong tries in a
/// row), or the error that ended it (a pulled card: tap again, which seals again).
///
/// `progress` is called before the first asking and after each `6A98` with the askings still
/// owed (about 0.5 s each on a J3R180), so a caller can show "hold the card". The count is read
/// from GET_STATE once, before any asking: a command between askings would begin the stretch
/// again, which is also why nothing asks a new nonce during the wait.
pub fn verify_pin_held(card: &mut dyn Card, trust: &TrustedCard, pin: &[u8], progress: &mut dyn FnMut(u16)) -> Result<()> {
    require_current(card)?;
    let owed = card.get_state()?.owed;
    let mut block = sealed::pin_block(pin)?;
    let cmd = SealedCommand::new(card, trust, INS_VERIFY_PIN, &block);
    block.fill(0);
    let cmd = cmd?;
    serve_wait(owed, progress, &mut || cmd.send(card))
}

/// SET_PIN, sealed (applet 4.x): GET_NONCE, the check under `trust`, one envelope. On applet 4.0
/// the first SET_PIN needs this selection's anchor, the registry key and an empty card, and it
/// gives the PIN to the selection that sent it.
pub fn set_pin_sealed(card: &mut dyn Card, trust: &TrustedCard, pin: &[u8]) -> Result<()> {
    require_current(card)?;
    let mut block = sealed::pin_block(pin)?;
    let cmd = SealedCommand::new(card, trust, INS_SET_PIN, &block);
    block.fill(0);
    cmd?.send(card)
}

/// CHANGE_PIN, sealed (applet 4.x): old block ‖ new block in one envelope. Needs this session's
/// PIN and ends it. The old PIN is a try like VERIFY_PIN's: a wrong one answers `63CX`.
pub fn change_pin_sealed(card: &mut dyn Card, trust: &TrustedCard, old: &[u8], new: &[u8]) -> Result<()> {
    require_current(card)?;
    let (mut o, mut n) = (sealed::pin_block(old).context("the old PIN")?, sealed::pin_block(new).context("the new PIN")?);
    let mut clear = [0u8; 2 * sealed::PIN_BLOCK_LEN];
    clear[..9].copy_from_slice(&o);
    clear[9..].copy_from_slice(&n);
    let cmd = SealedCommand::new(card, trust, INS_CHANGE_PIN, &clear);
    o.fill(0);
    n.fill(0);
    clear.fill(0);
    cmd?.send(card)
}

/// Provision the PIN if the card has none, then open this selection's session with it, sealed
/// (applet 4.x), the wait served by [`verify_pin_held`] with `progress`.
///
/// `SIGN` refuses until both are true, so anything that spends must call this after `SELECT` and
/// after the keys are in place. The first SET_PIN gives the PIN to the selection that sent it, so
/// no VERIFY_PIN follows it. A card that already holds a different PIN answers `63 CX` and counts
/// a wrong try, so only call it on a card whose PIN you own.
pub fn open_pin_session_held(card: &mut dyn Card, trust: &TrustedCard, pin: &[u8], progress: &mut dyn FnMut(u16)) -> Result<()> {
    match verify_pin_held(card, trust, pin, progress) {
        // Already verified: the session is open.
        Ok(()) => Ok(()),
        Err(e) if status_word(&e) == Some(SW_NO_PIN_SET) => set_pin_sealed(card, trust, pin).context("SET_PIN on a fresh card"),
        Err(e) => Err(e).context("VERIFY_PIN"),
    }
}

pub trait Card {
    /// Generate a key in `slot`. Fails with `6985` if the slot is already used.
    fn gen_key(&mut self, slot: u8) -> Result<XOnlyPublicKey>;
    /// Wipe `slot` and generate a fresh key in it (GEN_KEY with P2 = 1). Works on a used or an
    /// empty slot; the old key is gone for good.
    ///
    /// Contract: an overwrite destroys the spend key. The caller must first prove that nothing
    /// is still locked to the slot's current pubkey, and record the pending overwrite together
    /// with the old pubkey before sending. On any failure (error, timeout, torn connection) call
    /// `get_pub` to learn the outcome: `6985` means the slot is empty; a different pubkey means
    /// the new key exists, so adopt it; the old pubkey means the command never ran. Never retry
    /// a P2 = 1 GEN_KEY blindly.
    fn gen_key_overwrite(&mut self, slot: u8) -> Result<XOnlyPublicKey>;
    /// x-only public key of the key in `slot`. Fails with `6985` if the slot is empty.
    fn get_pub(&mut self, slot: u8) -> Result<XOnlyPublicKey>;
    /// BIP340 signature over the raw 32 bytes. Requires a PIN on the card and a `VERIFY_PIN` in
    /// this selection: `6984` before the first `SET_PIN`, `6983` when blocked, `6A94` otherwise.
    /// `6985` means the slot is empty.
    fn sign(&mut self, slot: u8, msg32: &[u8; 32]) -> Result<Signature>;

    /// Public key of `slot`, generating a key first if the slot is empty. Idempotent, and safe
    /// when several tests race for the same slot.
    fn get_or_gen(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        match self.get_pub(slot) {
            Ok(pk) => Ok(pk),
            Err(e) if status_word(&e) == Some(SW_CONDITIONS_NOT_SATISFIED) => {
                match self.gen_key(slot) {
                    Ok(pk) => Ok(pk),
                    // Lost a race with another opener of the same slot.
                    Err(e) if status_word(&e) == Some(SW_CONDITIONS_NOT_SATISFIED) => self.get_pub(slot),
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// One APDU round trip at the ISO level, without the applet's CLA. This is the escape hatch
    /// the provisioning endpoints need: `SELECT` of an AID, `GET DATA` on the ISD, and the PIN
    /// commands. Returns the response data; a non-`9000` status word is an error carrying it.
    ///
    /// Backends that cannot speak raw APDUs keep this failing loudly rather than pretending.
    fn apdu(&mut self, _apdu: &[u8]) -> Result<Vec<u8>> {
        Err(NoRawApdu.into())
    }

    /// Select the applet by AID. `Ok(())` means `9000`.
    fn select_applet(&mut self) -> Result<()> {
        self.apdu(&SELECT_APDU).map(|_| ())
    }

    /// `GET DATA` on `FF21` (the card manager's free-memory template). Any answer — data or a
    /// status word — proves the ISD is there; only a transport error means it is not.
    fn get_data_ff21(&mut self) -> Result<Vec<u8>> {
        self.apdu(&[CLA_ISO, 0xCA, 0xFF, 0x21, 0x00])
    }

    /// `GET_NONCE` (applet 4.x): nonce (16) ‖ PIN key (33) ‖ binding (DER). `6985` with no
    /// certificate on the card. Parse and check it with [`PinKey`]; [`SealedCommand`] does both.
    fn get_nonce(&mut self) -> Result<Vec<u8>> {
        self.apdu(&sealed::GET_NONCE_APDU)
    }

    // ------------------------------------------------------------ applet 3.0 (v3.rs)

    /// The applet's version. A backend that kept `SELECT`'s answer gives it; otherwise the card is
    /// asked `GET_STATE`, which a 3.0 applet answers and a 2.x one refuses (`6D00`), so the PIN
    /// session is left alone (a `SELECT` would end it). A card that cannot be asked is taken for
    /// 2.x: it then gets none of 3.0's commands.
    fn applet_version(&mut self) -> Result<AppletVersion> {
        match self.apdu(&v3_apdu(v3::INS_GET_STATE, 0, &[])) {
            Ok(st) => Ok(state_version(&st)),
            Err(e) if matches!(status_word(&e), Some(SW_INS_NOT_SUPPORTED) | Some(SW_CLA_NOT_SUPPORTED)) => Ok(AppletVersion::V2_1),
            Err(e) if e.downcast_ref::<NoRawApdu>().is_some() => Ok(AppletVersion::V2_1),
            Err(e) => Err(e).context("asking the card for its version (GET_STATE)"),
        }
    }

    /// `SET_NOTE`: the note on a KEYED slot, or the finished change on a CHANGE slot (same amount,
    /// else `6A80`), with the registry's signature over it (`reg_sig`, DER; [`v3::note_message`]).
    /// No PIN. `6982`: the signature does not verify under the card's registry key; `6985`: the
    /// card has no registry key, or the slot takes no note. Nothing is written on any refusal.
    fn set_note(&mut self, slot: u8, amount_msat: u64, sig48: &[u8; 48], reg_sig: &[u8]) -> Result<()> {
        ensure!(
            (v3::NOTE_SIG_MIN..=v3::NOTE_SIG_MAX).contains(&reg_sig.len()),
            "a note's registry signature is {} to {} bytes of DER",
            v3::NOTE_SIG_MIN,
            v3::NOTE_SIG_MAX
        );
        let mut d = v3::note_bytes(amount_msat, sig48).to_vec();
        d.extend_from_slice(reg_sig);
        self.apdu(&v3_apdu(v3::INS_SET_NOTE, slot, &d)).map(|_| ())
    }

    /// `GET_SLOT`: state, key, note, spent hash, opening. No PIN.
    fn get_slot(&mut self, slot: u8) -> Result<SlotInfo> {
        SlotInfo::parse(&self.apdu(&v3_apdu(v3::INS_GET_SLOT, slot, &[]))?)
    }

    /// `GET_STATE`: every slot's state, the clock, the limit, what attestation has. No PIN.
    fn get_state(&mut self) -> Result<CardStatus> {
        CardStatus::parse(&self.apdu(&v3_apdu(v3::INS_GET_STATE, 0, &[]))?)
    }

    /// `SET_CHANGE`: a KEYED slot is owed change of `amount_msat` from output `opening`.
    fn set_change(&mut self, slot: u8, amount_msat: u64, opening: &Opening) -> Result<()> {
        let mut d = amount_msat.to_be_bytes().to_vec();
        d.extend_from_slice(&opening.to_bytes());
        self.apdu(&v3_apdu(v3::INS_SET_CHANGE, slot, &d)).map(|_| ())
    }

    /// `SET_LIMIT`: the day's limit in msat (0: none). Only before `SET_PIN`.
    fn set_limit(&mut self, limit_msat: u64) -> Result<()> {
        self.apdu(&v3_apdu(v3::INS_SET_LIMIT, 0, &limit_msat.to_be_bytes())).map(|_| ())
    }

    /// `SET_HEADER`: a Bitcoin block header; answers the card's clock after it.
    fn set_header(&mut self, header: &[u8; 80]) -> Result<u32> {
        let out = self.apdu(&v3_apdu(v3::INS_SET_HEADER, 0, header))?;
        ensure!(out.len() == 4, "SET_HEADER answered {} bytes, not 4", out.len());
        Ok(u32::from_be_bytes(out[..4].try_into().expect("4")))
    }

    /// `ATTEST_INIT`: (re)make the attestation key; its public point, 65 bytes uncompressed.
    fn attest_init(&mut self) -> Result<Vec<u8>> {
        let out = self.apdu(&v3_apdu(v3::INS_ATTEST_INIT, 0, &[]))?;
        ensure!(out.len() == 65 && out[0] == 0x04, "ATTEST_INIT answered no uncompressed P-256 point");
        Ok(out)
    }

    /// `SET_CERT`: the registry's certificate, once, before `SET_PIN`.
    fn set_cert(&mut self, cert: &[u8]) -> Result<()> {
        ensure!((1..=v3::CERT_MAX).contains(&cert.len()), "a certificate is 1 to {} bytes", v3::CERT_MAX);
        self.apdu(&v3_apdu(v3::INS_SET_CERT, 0, cert)).map(|_| ())
    }

    /// `GET_CERT`. `6A83` when the card has none.
    fn get_cert(&mut self) -> Result<Vec<u8>> {
        self.apdu(&v3_apdu(v3::INS_GET_CERT, 0, &[]))
    }

    /// `ATTEST`: ECDSA-SHA256 (DER) over the label, `challenge` and slot 0's key.
    fn attest(&mut self, challenge: &[u8; 32]) -> Result<Vec<u8>> {
        self.apdu(&v3_apdu(v3::INS_ATTEST, 0, challenge))
    }

    /// `SET_REGISTRY_KEY`: the registry's certificate key, 65 bytes uncompressed, before
    /// `SET_PIN` (docs/OFFLINE-PROTOCOL.md § 6).
    fn set_registry_key(&mut self, key: &[u8; 65]) -> Result<()> {
        self.apdu(&v3_apdu(v3::INS_SET_REGISTRY_KEY, 0, key)).map(|_| ())
    }

    /// `RELEASE`: the registry's signature (DER) that `txid` will never be accepted. A SPENT slot
    /// whose spent hash is `txid` becomes NOTE, a CHANGE slot owed by `txid` becomes KEYED; the
    /// answer is the new state. `6982` for a bad signature, `6985` for a slot it does not fit or a
    /// card with no registry key. No PIN.
    fn release(&mut self, slot: u8, txid: &[u8; 32], der: &[u8]) -> Result<SlotState> {
        ensure!(
            (v3::RELEASE_SIG_MIN..=v3::RELEASE_SIG_MAX).contains(&der.len()),
            "a release signature is {} to {} bytes of DER",
            v3::RELEASE_SIG_MIN,
            v3::RELEASE_SIG_MAX
        );
        let mut d = txid.to_vec();
        d.extend_from_slice(der);
        let out = self.apdu(&v3_apdu(v3::INS_RELEASE, slot, &d))?;
        ensure!(out.len() == 1, "RELEASE answered {} bytes, not 1", out.len());
        SlotState::from_byte(out[0])
    }
}

/// The version a GET_STATE answer shows: 65 bytes is 4.0, anything shorter 3.0.
fn state_version(answer: &[u8]) -> AppletVersion {
    if answer.len() == v3::GET_STATE_LEN { AppletVersion::V4_0 } else { AppletVersion::V3_0 }
}

/// A 3.0 command: the applet's CLA, `ins`, P1 = `p1`, P2 = 0, the data with its Lc when there is
/// any, and Le = 0.
pub fn v3_apdu(ins: u8, p1: u8, data: &[u8]) -> Vec<u8> {
    let mut a = vec![CLA, ins, p1, 0x00];
    if !data.is_empty() {
        a.push(data.len() as u8);
        a.extend_from_slice(data);
    }
    a.push(0x00);
    a
}

/// Sends one APDU and returns `data || SW1 SW2`.
/// What carries APDUs to one card. A transport that selects an applet by itself (an NFC stack's
/// auto-select, a relay that re-selects), or that goes on across a card being swapped, leaves a
/// [`CardClient`]'s SELECT state stale: call [`CardClient::select`] again after any of these.
pub trait Transport {
    fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>>;
}

pub struct CardClient<T: Transport> {
    transport: T,
    /// The version the last SELECT answered, when it passed the rule for every reader (`9000`,
    /// two bytes, major 4). `None` before any SELECT and after one that did not pass. The PIN
    /// commands are sent only while it is `Some`.
    selected: Option<AppletVersion>,
    /// Why the last SELECT did not pass, when it answered `9000`: what a PIN command then gives.
    refused: Option<UnsupportedApplet>,
    /// Whether any SELECT went out through this client. Until one does, the transport is taken to
    /// select by itself and [`Card::applet_version`] asks GET_STATE.
    sent_select: bool,
}

impl<T: Transport> CardClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport, selected: None, refused: None, sent_select: false }
    }

    /// Select the applet. Needed on a real card and before any PIN command. Passes only when the
    /// card answers `9000` with exactly two bytes and major 4 (the rule for every reader); an older
    /// card is [`UnsupportedApplet::Old`], the old-card error ([`is_old_card`]). The version is
    /// kept for [`Card::applet_version`].
    pub fn select(&mut self) -> Result<()> {
        self.forget_select();
        let out = self.exchange(&SELECT_APDU)?;
        self.took_select(&out)?;
        Ok(())
    }

    /// Apply the rule for every reader to a `9000` SELECT's answer.
    fn took_select(&mut self, out: &[u8]) -> std::result::Result<AppletVersion, UnsupportedApplet> {
        let r = UnsupportedApplet::check(out);
        self.selected = r.as_ref().ok().copied();
        self.refused = r.as_ref().err().cloned();
        r
    }

    /// A SELECT is about to go out: whatever the last one passed no longer holds.
    fn forget_select(&mut self) {
        self.selected = None;
        self.refused = None;
        self.sent_select = true;
    }

    /// Why no PIN command may go out now: the last SELECT's refusal, or [`NotSelected`].
    fn not_selected(&self) -> anyhow::Error {
        match &self.refused {
            Some(why) => why.clone().into(),
            None => NotSelected.into(),
        }
    }

    /// Whether the last SELECT passed the rule for every reader.
    pub fn selected(&self) -> Option<AppletVersion> {
        self.selected
    }

    pub fn into_transport(self) -> T {
        self.transport
    }

    /// Send an APDU, follow `61xx` with GET RESPONSE and `6Cxx` by resending with Le = xx
    /// (every APDU we build ends in its Le byte), and split off the status word.
    fn exchange(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        let mut resp = self.transport.transmit(apdu)?;
        for _ in 0..MAX_CHAIN {
            ensure!(resp.len() >= 2, "response shorter than a status word");
            let (body, sw) = resp.split_at(resp.len() - 2);
            data.extend_from_slice(body);
            match (sw[0], sw[1]) {
                (0x90, 0x00) => return Ok(data),
                (0x61, n) => resp = self.transport.transmit(&[0x00, 0xC0, 0x00, 0x00, n])?,
                (0x6C, _) if apdu.len() >= 4 && apdu[1] == INS_GEN_KEY && apdu[3] == 0x01 => {
                    // Never resend an overwrite: the first attempt may already have replaced the key.
                    return Err(StatusWord(0x6C00 | u16::from(sw[1])).into());
                }
                (0x6C, n) => {
                    let mut again = apdu.to_vec();
                    *again.last_mut().expect("non-empty APDU") = n;
                    resp = self.transport.transmit(&again)?;
                }
                (a, b) => return Err(StatusWord(u16::from_be_bytes([a, b])).into()),
            }
        }
        bail!("card kept answering 61xx/6Cxx for {MAX_CHAIN} rounds")
    }

    fn pubkey_cmd(&mut self, ins: u8, slot: u8, p2: u8) -> Result<XOnlyPublicKey> {
        let out = self.exchange(&[CLA, ins, slot, p2, 0x00])?;
        XOnlyPublicKey::from_slice(&out).context("card returned an invalid x-only public key")
    }
}

impl<T: Transport> Card for CardClient<T> {
    fn gen_key(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        self.pubkey_cmd(INS_GEN_KEY, slot, 0x00)
    }

    fn gen_key_overwrite(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        self.pubkey_cmd(INS_GEN_KEY, slot, 0x01)
    }

    fn get_pub(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        self.pubkey_cmd(INS_GET_PUB, slot, 0x00)
    }

    fn apdu(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        // Any ISO SELECT may select another application (another applet with the same CLA, the
        // card manager, this one on another channel): it ends what the last SELECT passed, and only
        // this applet's own SELECT, byte for byte, can pass again.
        let selecting = apdu.len() >= 2 && apdu[0] & 0x80 == 0 && apdu[1] == 0xA4;
        let ours = matches!(apdu.len(), 13 | 14) && apdu[..13] == SELECT_APDU[..13];
        if selecting {
            self.forget_select();
        }
        // The rule for every reader: no PIN command reaches a card whose SELECT did not pass.
        let pin_cmd = apdu.len() >= 2 && apdu[0] == CLA && matches!(apdu[1], INS_VERIFY_PIN | INS_SET_PIN | INS_CHANGE_PIN);
        if pin_cmd && self.selected.is_none() {
            return Err(self.not_selected());
        }
        // A card SELECT showed as 4.x takes the sealed form only (§ 9): a P1 00 PIN never leaves.
        if pin_cmd && self.selected.is_some_and(|v| v.is_current()) && apdu.get(2) != Some(&sealed::P1_SEALED) {
            return Err(ClearPinRefused.into());
        }
        let out = self.exchange(apdu)?;
        if ours {
            let _ = self.took_select(&out);
        }
        Ok(out)
    }

    fn applet_version(&mut self) -> Result<AppletVersion> {
        match self.selected {
            Some(v) => Ok(v),
            // A SELECT went out and did not pass: GET_STATE would reach an old card or another
            // application.
            None if self.sent_select => Err(self.not_selected()),
            None => {
                let v = match self.exchange(&v3_apdu(v3::INS_GET_STATE, 0, &[])) {
                    Ok(st) => state_version(&st),
                    Err(e) if matches!(status_word(&e), Some(SW_INS_NOT_SUPPORTED) | Some(SW_CLA_NOT_SUPPORTED)) => AppletVersion::V2_1,
                    Err(e) => return Err(e).context("asking the card for its version (GET_STATE)"),
                };
                Ok(v)
            }
        }
    }

    fn sign(&mut self, slot: u8, msg32: &[u8; 32]) -> Result<Signature> {
        let mut apdu = vec![CLA, INS_SIGN, slot, 0x00, 0x20];
        apdu.extend_from_slice(msg32);
        apdu.push(0x00);
        let out = self.exchange(&apdu).map_err(|e| if status_word(&e) == Some(v3::SW_CRYPTO_ERROR) { sign_check_failed() } else { e })?;
        Signature::from_slice(&out).context("card returned an invalid signature")
    }
}

/// In-process stand-in for the applet, with the same slot semantics (including `6985`) and the
/// same PIN state machine. [`SoftCard::new`] is a 2.1 card with [`SLOTS`] slots and 2.1's PIN:
/// `SET_PIN` once, three tries, `6984` before a PIN exists, `6983` after it is out of tries;
/// [`SoftCard::with_slots`] makes one of any size, such as a 2.0 card with [`MIN_SLOTS`].
///
/// [`SoftCard::v4`] is an applet 4.0 card (`FedimintCardApplet.java`, docs/OFFLINE-PROTOCOL.md
/// § 1), followed exactly: slot states, sign once per slot (the same hash may be signed again),
/// notes signed by the registry and change behind the PIN, the day's limit on a clock that only a
/// Bitcoin header with the work moves (the weak rule and the anchor), and a P-256 attestation key
/// with its certificate. Its PIN is 4.0's: every try counted with its wait before the compare,
/// the wait served by resending VERIFY_PIN (`6A98`), never blocked, and the first SET_PIN giving
/// the PIN to its selection. [`SoftCard::v3`] is an old card for the hosts' refusal: SELECT
/// answers `03 00`, and behind SELECT it follows the 4.0 rules, which no host that checks SELECT
/// reaches.
#[derive(Default)]
pub struct SoftCard {
    /// One entry per slot the card has; a slot past the end answers `6B00`.
    slots: Vec<Option<Keypair>>,
    pin: Option<Vec<u8>>,
    /// 2.x only: tries before the block. A 4.0 card keeps it at 3 and counts [`Self::wrong`].
    tries_left: u8,
    /// Whether this session has verified the PIN, which is what lets `SIGN` run. Cleared by
    /// `SELECT`, as on the applet: a session is a selected applet, not a card.
    validated: bool,
    /// 4.0: this selection's first SET_PIN gave it the PIN (`held[H_PIN_GIVEN]`). SELECT and every
    /// try clear it.
    given_by_set: bool,
    /// 4.0 `pinMarks`: the wrong tries in a row, at most [`PIN_WRONG_MOST`], and the askings the
    /// next try owes, at most [`PIN_OWED_MOST`]. Persistent; a right PIN clears both.
    wrong: u8,
    owed: u16,
    /// 4.0 `held[H_ASKED]`: the askings of the stretch under way. SELECT and any command but
    /// VERIFY_PIN set it to 0.
    asked: u16,
    /// Tests: the next SIGN of a NOTE slot fails its signature check (`6F00`).
    fail_sign_check: bool,
    /// Whether the applet is installed. A blank card answers `6A82` to SELECT.
    installed: bool,
    /// What SELECT answers. 3.0 and later get [`Self::v3`]'s rules.
    version: AppletVersion,
    /// 3.0: per slot, its state, note, spent hash and opening (one entry per slot).
    v3: Vec<V3Slot>,
    clock: Clock,
    cert: Vec<u8>,
    attest: Option<p256::ecdsa::SigningKey>,
    /// The registry's certificate key (SET_REGISTRY_KEY), which signs releases and notes.
    registry_key: Option<p256::ecdsa::VerifyingKey>,
    /// This selection anchored the clock (the applet's transient `held[H_ANCHORED]`); SELECT
    /// clears it.
    anchored: bool,
    /// Tests: the floor in place of [`v3::FLOOR_BITS`] ([`SoftCard::use_floor`]).
    floor: Option<[u8; 4]>,
    /// 4.x: the PIN key, built and generated at each ATTEST_INIT (`pinKey`).
    pin_key: Option<secp256k1::SecretKey>,
    /// 4.x: the PIN key's binding by the attestation key, made at the first GET_NONCE after
    /// SET_CERT and kept (`pinKeySig`); empty until then.
    pin_binding: Vec<u8>,
    /// 4.x: the live nonce (RAM): GET_NONCE makes it, the first sealed command that reaches its
    /// opening spends it, SELECT clears it.
    nonce: Option<[u8; sealed::NONCE_LEN]>,
    /// Tests: GET_NONCE answers this key in place of the PIN key, with the PIN key's binding
    /// ([`SoftCard::spoof_pin_key`]).
    spoofed_pin_key: Option<secp256k1::SecretKey>,
    /// The INS of every APDU the card took, in order ([`SoftCard::received_ins`]).
    ins_log: Vec<u8>,
}

impl Default for AppletVersion {
    fn default() -> Self {
        AppletVersion::V2_1
    }
}

/// A 3.0 slot beside its key: what the applet keeps in `state`, `notes`, `spentHash`, `openings`.
#[derive(Clone)]
struct V3Slot {
    state: SlotState,
    note: [u8; v3::NOTE_LEN],
    spent: [u8; 32],
    /// The one signature the slot gave, answered again for the same hash (applet `332a45f`).
    sig: [u8; 64],
    opening: [u8; v3::OPEN_LEN],
}

impl V3Slot {
    fn empty() -> Self {
        Self { state: SlotState::Empty, note: [0; v3::NOTE_LEN], spent: [0; 32], sig: [0; 64], opening: [0; v3::OPEN_LEN] }
    }
}

/// The applet's clock record.
#[derive(Default, Clone, Copy)]
struct Clock {
    now: u32,
    window: u32,
    /// The hardest `bits` taken (little-endian, as in a header); zeros before the first header.
    bits: [u8; 4],
    limit: u64,
    spent: u64,
}

impl SoftCard {
    /// An applet 2.1 card (SELECT answers no version): an old card for the hosts' refusal.
    pub fn new() -> Self {
        Self::with_slots(SLOTS)
    }

    /// An installed card with `n` key slots: slot `n` and up answer `6B00`.
    pub fn with_slots(n: u8) -> Self {
        Self { slots: vec![None; n as usize], tries_left: 3, installed: true, v3: vec![V3Slot::empty(); n as usize], ..Default::default() }
    }

    /// An applet 4.0 card: 32 slots and the 4.0 rules.
    pub fn v4() -> Self {
        Self { version: AppletVersion::V4_0, ..Self::with_slots(SLOTS) }
    }

    /// An old card: SELECT answers `03 00`. Behind SELECT it follows the 4.0 rules.
    pub fn v3() -> Self {
        Self { version: AppletVersion::V3_0, ..Self::with_slots(SLOTS) }
    }

    /// A card with no applet on it: SELECT answers `6A82`, and every slot is empty.
    pub fn blank() -> Self {
        Self { installed: false, ..Self::new() }
    }

    /// The INS byte of every APDU this card took through its APDU layer, in order. (GEN_KEY,
    /// GET_PUB and SIGN called as [`Card`] methods are not APDUs and are not listed.)
    pub fn received_ins(&self) -> &[u8] {
        &self.ins_log
    }

    /// Empty [`Self::received_ins`], so a test can look only at what comes next.
    pub fn forget_received(&mut self) {
        self.ins_log.clear();
    }

    /// Whether any PIN command reached this card: VERIFY_PIN, SET_PIN, CHANGE_PIN (sealed or
    /// not) or GET_NONCE, which opens a sealed one.
    pub fn heard_a_pin_command(&self) -> bool {
        self.ins_log.iter().any(|i| [INS_VERIFY_PIN, INS_SET_PIN, INS_CHANGE_PIN, sealed::INS_GET_NONCE].contains(i))
    }

    /// How many key slots this card has.
    pub fn slot_len(&self) -> u8 {
        self.slots.len() as u8
    }

    /// The version SELECT answers.
    pub fn version(&self) -> AppletVersion {
        self.version
    }

    /// A card whose applet is there but has never been pinned.
    pub fn pinned_with(&mut self, pin: &[u8]) {
        self.pin = Some(pin.to_vec());
        self.tries_left = 3;
    }

    pub fn is_installed(&self) -> bool {
        self.installed
    }

    /// Make SELECT answer `6A82`: the applet is not on this card.
    pub fn visit_blank_applet(&mut self) {
        self.installed = false;
    }

    /// Key every slot with keys no wallet recorded — another machine's card. The slots are full,
    /// so `card setup` would find nothing to generate and `keySetup` must refuse.
    pub fn install_foreign_keys(&mut self) {
        for (s, v) in self.slots.iter_mut().zip(self.v3.iter_mut()) {
            if s.is_none() {
                *s = Some(Keypair::new(SECP256K1, &mut rand::thread_rng()));
                v.state = SlotState::Keyed;
            }
        }
    }

    /// Tests: the card takes headers against `bits` as its floor instead of [`v3::FLOOR_BITS`], so
    /// a test can mine them, as the applet's tests put their floor in place (`Headers.useFloor`).
    pub fn use_floor(&mut self, bits: [u8; 4]) {
        self.floor = Some(bits);
    }

    /// Tests: key an empty `slot` with `key`, a key the test knows (vectors whose slot key is a
    /// known point). The slot is KEYED, as after GEN_KEY.
    pub fn key_slot_with(&mut self, slot: u8, key: Keypair) -> Result<()> {
        if self.slot(slot)?.is_some() {
            return Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into());
        }
        self.slots[slot as usize] = Some(key);
        self.v3[slot as usize] = V3Slot { state: SlotState::Keyed, ..V3Slot::empty() };
        Ok(())
    }

    /// Tests: write the PIN's marks, as the applet's tests do (`PinWaitTest.setMarks`): the wrong
    /// tries in a row and the askings the next try owes.
    pub fn set_pin_marks(&mut self, wrong: u8, owed: u16) {
        self.wrong = wrong;
        self.owed = owed;
    }

    /// Tests: the next SIGN that reaches a NOTE slot's signature answers `6F00`, as the applet
    /// does when no signature passes its own check: the slot stays NOTE, and the clock and the
    /// day's spend are unchanged. One SIGN only.
    pub fn fail_next_sign_check(&mut self) {
        self.fail_sign_check = true;
    }

    /// Tests: the card's state of `slot` as GET_SLOT would answer it (3.0).
    pub fn slot_state(&self, slot: u8) -> Option<SlotState> {
        self.v3.get(slot as usize).map(|v| v.state)
    }

    /// Tests: a clone of this card's keys and notes as an attacker who extracted them would have
    /// it, with fresh PIN state and no attestation key of the card's own (a clone cannot copy it).
    pub fn extract_clone(&self) -> Self {
        Self {
            slots: self.slots.clone(),
            pin: self.pin.clone(),
            ins_log: vec![],
            tries_left: 3,
            validated: false,
            given_by_set: false,
            wrong: 0,
            owed: 0,
            asked: 0,
            fail_sign_check: false,
            installed: true,
            version: self.version,
            v3: self
                .v3
                .iter()
                .map(|v| {
                    // The attacker resets spent slots to the notes they held.
                    let mut c = v.clone();
                    if c.state == SlotState::Spent {
                        c.state = SlotState::Note;
                        c.spent = [0; 32];
                        c.sig = [0; 64];
                    }
                    c
                })
                .collect(),
            clock: Clock { spent: 0, ..self.clock },
            cert: self.cert.clone(),
            attest: Some(p256::ecdsa::SigningKey::random(&mut rand::thread_rng())),
            registry_key: self.registry_key,
            anchored: false,
            floor: self.floor,
            // The PIN key never leaves a card: the clone makes its own, which only its own
            // (uncertified) attestation key binds. GET_NONCE answers it (the applet refuses GET_NONCE only without a
            // certificate, FCA getNonce), and the host's check under the certificate refuses it.
            pin_key: Some(secp256k1::SecretKey::new(&mut rand::thread_rng())),
            pin_binding: vec![],
            nonce: None,
            spoofed_pin_key: None,
        }
    }

    /// Tests: GET_NONCE answers a key of the card's choosing in place of its PIN key, with the
    /// PIN key's own binding. A host refuses it before it seals.
    pub fn spoof_pin_key(&mut self) {
        self.spoofed_pin_key = Some(secp256k1::SecretKey::new(&mut rand::thread_rng()));
    }

    /// Tests: the PIN key's scalar in place of the one ATTEST_INIT made (the vectors' card, as the
    /// applet's tests set `ownPinKey`). The binding is made again at the next GET_NONCE.
    pub fn use_pin_key(&mut self, d: secp256k1::SecretKey) {
        self.pin_key = Some(d);
        self.pin_binding.clear();
    }

    /// Tests: make `nonce` the live nonce, as a GET_NONCE that drew it would.
    pub fn use_nonce(&mut self, nonce: [u8; sealed::NONCE_LEN]) {
        self.nonce = Some(nonce);
    }

    /// Tests: whether the card holds a live nonce.
    pub fn nonce_live(&self) -> bool {
        self.nonce.is_some()
    }

    /// 4.x GET_NONCE (`B0 44 01 00`): 6B00 for another P1 or P2, 6985 with no certificate; then
    /// the binding, made once, and a fresh nonce, live from here.
    fn get_nonce_v4(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        use p256::ecdsa::signature::Signer as _;
        if apdu[2] != sealed::P1_SEALED || apdu[3] != 0 {
            return Err(StatusWord(SW_WRONG_P1P2).into());
        }
        let (false, Some(d), Some(att)) = (self.cert.is_empty(), self.pin_key, &self.attest) else {
            return Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into());
        };
        let key = d.public_key(SECP256K1).serialize();
        if self.pin_binding.is_empty() {
            let sig: p256::ecdsa::Signature = att.sign(&sealed::binding_message(&key));
            self.pin_binding = sig.to_der().as_bytes().to_vec();
        }
        let nonce: [u8; sealed::NONCE_LEN] = rand::random();
        self.nonce = Some(nonce);
        let shown = self.spoofed_pin_key.map_or(key, |k| k.public_key(SECP256K1).serialize());
        Ok([&nonce[..], &shown, &self.pin_binding].concat())
    }

    /// The opening (`unseal`): spends the live nonce, opened or not, and answers the clear data,
    /// or `None` for an envelope that does not open.
    fn open_sealed(&mut self, ins: u8, data: &[u8]) -> Option<Vec<u8>> {
        let nonce = self.nonce.take()?;
        sealed::open_with(sealed::SEAL_LABEL, self.pin_key.as_ref()?, &nonce, ins, data)
    }

    /// A failed opening on VERIFY_PIN or CHANGE_PIN: 8 random bytes tried as a PIN, so it counts
    /// a wrong try with its wait and ends the session (§ 9: a try like any other, `63CX`).
    fn failed_opening(&mut self) -> anyhow::Error {
        let r: [u8; 8] = rand::random();
        match self.try_pin(&r) {
            Ok(()) => StatusWord(0x63C0 | u16::from(self.wrong)).into(),
            Err(e) => e,
        }
    }

    fn slot(&self, slot: u8) -> Result<&Option<Keypair>> {
        self.slots.get(slot as usize).ok_or_else(|| StatusWord(SW_SLOT_OUT_OF_RANGE).into())
    }

    fn is_v3(&self) -> bool {
        self.version.sign_once()
    }

    /// The applet's `PIN_STATE_NOT_SET`.
    fn pin_not_set(&self) -> bool {
        self.pin.is_none() && self.tries_left > 0
    }

    /// The applet's `pinGiven`: this selection has the PIN, by a right try or by its SET_PIN.
    fn pin_given(&self) -> bool {
        self.validated || self.given_by_set
    }

    /// 4.0 `tryPin`: the try counted with the wait the next one would owe (`countTry`, before the
    /// compare, right PINs included), then the compare; a right PIN clears both marks and opens
    /// the session, a wrong one answers `63CX`, X the wrong tries in a row. A try first ends
    /// whatever the session had.
    fn try_pin(&mut self, candidate: &[u8]) -> Result<()> {
        self.given_by_set = false;
        let w = self.wrong;
        self.wrong = (w + 1).min(PIN_WRONG_MOST);
        self.owed = if w < 14 { 1 << w } else { PIN_OWED_MOST };
        if self.pin.as_deref() == Some(candidate) {
            self.wrong = 0;
            self.owed = 0;
            self.validated = true;
            return Ok(());
        }
        self.validated = false;
        Err(StatusWord(0x63C0 | u16::from(self.wrong)).into())
    }

    /// 4.0 `waitOwed`: the askings owed, served [`PIN_STRETCH`] at a time. Askings 1..stretch
    /// answer `6A98`; the asking after a stretch writes what is still owed and answers `6A98`
    /// again while anything is, or returns, and the try follows.
    fn wait_owed(&mut self) -> Result<()> {
        if self.owed == 0 {
            return Ok(());
        }
        let stretch = self.owed.min(PIN_STRETCH);
        if self.asked < stretch {
            self.asked += 1;
            return Err(StatusWord(v3::SW_KEEP_HOLDING).into());
        }
        self.asked = 0;
        self.owed -= stretch;
        if self.owed != 0 {
            return Err(StatusWord(v3::SW_KEEP_HOLDING).into());
        }
        Ok(())
    }

    /// 4.x VERIFY_PIN, sealed only (§ 9's order): no PIN 6984, P1/P2 6B00 (P1 00
    /// included), Lc 6700, no live nonce 6985 (nothing written), the wait, a short field 6700,
    /// then the opening, which spends the nonce: an envelope that does not open is a wrong try;
    /// a block whose length is not 4 to 8 is 6700 with no try; then the try.
    fn verify_pin_v4(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        let len = lc_of(apdu);
        if self.pin.is_none() {
            return Err(StatusWord(SW_NO_PIN_SET).into());
        }
        if apdu[2] != sealed::P1_SEALED || apdu[3] != 0 {
            return Err(StatusWord(SW_WRONG_P1P2).into());
        }
        if len != sealed::SEALED_LC {
            return Err(StatusWord(v3::SW_WRONG_LENGTH).into());
        }
        if self.nonce.is_none() {
            return Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into());
        }
        self.wait_owed()?;
        let got = received(apdu).to_vec();
        if got.len() < len {
            return Err(StatusWord(v3::SW_WRONG_LENGTH).into());
        }
        let Some(clear) = self.open_sealed(INS_VERIFY_PIN, &got) else {
            return Err(self.failed_opening());
        };
        let Some(candidate) = sealed::unblock(&clear).map(<[u8]>::to_vec) else {
            return Err(StatusWord(v3::SW_WRONG_LENGTH).into());
        };
        self.try_pin(&candidate).map(|()| vec![])
    }

    /// 4.x SET_PIN, sealed only, in the applet's order: P1/P2 6B00, a PIN set 6985, Lc 6700, no
    /// anchor in this selection 6A96, no registry key or a slot holding value 6985, no live nonce
    /// 6985, a short field 6700; an envelope that does not open, or a bad block, 6700 with
    /// nothing written. The selection that sets the PIN has it.
    fn set_pin_v4(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        let len = lc_of(apdu);
        if apdu[2] != sealed::P1_SEALED || apdu[3] != 0 {
            return Err(StatusWord(SW_WRONG_P1P2).into());
        }
        if self.pin.is_some() {
            return Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into());
        }
        if len != sealed::SEALED_LC {
            return Err(StatusWord(v3::SW_WRONG_LENGTH).into());
        }
        if !self.anchored {
            return Err(StatusWord(v3::SW_NOT_ANCHORED).into());
        }
        if self.registry_key.is_none() || self.holds_value() || self.nonce.is_none() {
            return Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into());
        }
        let got = received(apdu).to_vec();
        if got.len() < len {
            return Err(StatusWord(v3::SW_WRONG_LENGTH).into());
        }
        let pin = self.open_sealed(INS_SET_PIN, &got).and_then(|c| sealed::unblock(&c).map(<[u8]>::to_vec));
        let Some(pin) = pin else {
            return Err(StatusWord(v3::SW_WRONG_LENGTH).into());
        };
        self.pin = Some(pin);
        self.given_by_set = true;
        Ok(vec![])
    }

    /// 4.x CHANGE_PIN, sealed only: data old block ‖ new block. No PIN 6984; P1/P2 6B00; no PIN in
    /// this session 6A94; Lc 6700; no live nonce 6985; a short field 6700; an envelope that does
    /// not open is a wrong try and ends the session; a bad block 6700 at no try; then the old PIN
    /// is a try, and a right one ends the session and sets the new PIN.
    fn change_pin_v4(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        let lc = lc_of(apdu);
        if self.pin.is_none() {
            return Err(StatusWord(SW_NO_PIN_SET).into());
        }
        if apdu[2] != sealed::P1_SEALED || apdu[3] != 0 {
            return Err(StatusWord(SW_WRONG_P1P2).into());
        }
        if !self.pin_given() {
            return Err(StatusWord(SW_PIN_REQUIRED).into());
        }
        if lc != sealed::SEALED_CHANGE_LC {
            return Err(StatusWord(v3::SW_WRONG_LENGTH).into());
        }
        if self.nonce.is_none() {
            return Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into());
        }
        let got = received(apdu).to_vec();
        if got.len() < lc {
            return Err(StatusWord(v3::SW_WRONG_LENGTH).into());
        }
        let Some(clear) = self.open_sealed(INS_CHANGE_PIN, &got) else {
            return Err(self.failed_opening());
        };
        let (Some(was), Some(new)) = (sealed::unblock(&clear[..9]), sealed::unblock(&clear[9..])) else {
            return Err(StatusWord(v3::SW_WRONG_LENGTH).into());
        };
        let (was, new) = (was.to_vec(), new.to_vec());
        self.try_pin(&was)?;
        // A change ends the session: the next CHANGE_PIN needs its own VERIFY_PIN.
        self.validated = false;
        self.pin = Some(new);
        Ok(vec![])
    }

    /// The applet's `holdsValue`: a slot holds a note or change owed to it.
    fn holds_value(&self) -> bool {
        self.v3.iter().any(|v| matches!(v.state, SlotState::Note | SlotState::Change))
    }

    /// The applet's `dayAfter`: the window and amount after spending `amount`, or 6A95.
    fn day_after(&self, amount: u64) -> Result<(u32, u64)> {
        let c = self.clock;
        let (mut window, mut spent) = (c.window, c.spent);
        if c.now != 0 {
            if window == 0 {
                window = c.now;
            } else if window.checked_add(v3::DAY_SECONDS).is_some_and(|end| c.now >= end) {
                window = c.now;
                spent = 0;
            }
        }
        let (s, wrapped) = spent.overflowing_add(amount);
        if c.limit != 0 && (wrapped || s > c.limit) {
            return Err(StatusWord(v3::SW_OVER_LIMIT).into());
        }
        Ok((window, s))
    }

    /// SET_HEADER: the card's test ([`v3::check_header`]), then the anchor while no PIN is set and
    /// no slot holds value, else a later header moves the clock. Either raises the hardest only
    /// where harder.
    fn set_header_v3(&mut self, h: &[u8]) -> Result<Vec<u8>> {
        let h: &[u8; 80] = h.try_into().map_err(|_| StatusWord(v3::SW_WRONG_LENGTH))?;
        let bits: [u8; 4] = h[72..76].try_into().expect("4");
        let hardest = (self.clock.bits != [0; 4]).then_some(self.clock.bits);
        let floor = self.floor.unwrap_or(v3::FLOOR_BITS);
        let harder = v3::check_header_at(h, hardest.as_ref(), &floor).map_err(StatusWord)?;
        let time = v3::header_time(h);
        if v3::anchors(!self.pin_not_set(), self.holds_value()) {
            self.clock = Clock { now: time, window: 0, bits: if harder { bits } else { self.clock.bits }, spent: 0, ..self.clock };
            self.anchored = true;
        } else if time > self.clock.now {
            self.clock.now = time;
            if harder {
                self.clock.bits = bits;
            }
        }
        Ok(self.clock.now.to_be_bytes().to_vec())
    }

    /// The 3.0 commands' APDU layer (`process` in the applet), after the CLA and SELECT.
    fn apdu_v3(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        let ins = apdu[1];
        let body = apdu_body(apdu)?;
        let len_is = |n: usize| -> Result<()> { if body.len() == n { Ok(()) } else { Err(StatusWord(v3::SW_WRONG_LENGTH).into()) } };
        // The applet's `slot()`: P1 a slot, P2 zero.
        let slot = || -> Result<usize> {
            let p1 = apdu[2] as usize;
            if p1 >= self.slots.len() || apdu[3] != 0 {
                return Err(StatusWord(SW_SLOT_OUT_OF_RANGE).into());
            }
            Ok(p1)
        };
        let refuse = || -> anyhow::Error { StatusWord(SW_CONDITIONS_NOT_SATISFIED).into() };
        match ins {
            v3::INS_SET_NOTE => {
                // The applet's order: length, the slot's state, the registry key, the amount, then
                // the signature; nothing is written before the signature verifies.
                let s = slot()?;
                let sig_len = body.len().wrapping_sub(v3::NOTE_LEN);
                if body.len() < v3::NOTE_LEN || !(v3::NOTE_SIG_MIN..=v3::NOTE_SIG_MAX).contains(&sig_len) {
                    return Err(StatusWord(v3::SW_WRONG_LENGTH).into());
                }
                let (note, der) = body.split_at(v3::NOTE_LEN);
                match self.v3[s].state {
                    SlotState::Change if self.v3[s].note[..8] != note[..8] => return Err(StatusWord(v3::SW_WRONG_DATA).into()),
                    SlotState::Change | SlotState::Keyed => {}
                    _ => return Err(refuse()),
                }
                let Some(key) = &self.registry_key else {
                    return Err(refuse());
                };
                if note[..8] == [0; 8] {
                    return Err(StatusWord(v3::SW_WRONG_DATA).into());
                }
                let Some(kp) = &self.slots[s] else {
                    return Err(refuse());
                };
                let amount = u64::from_be_bytes(note[..8].try_into().expect("8"));
                let sig48: [u8; 48] = note[8..].try_into().expect("48");
                if !v3::verify_note(key, &kp.x_only_public_key().0.serialize(), amount, &sig48, der) {
                    return Err(StatusWord(v3::SW_SECURITY_STATUS_NOT_SATISFIED).into());
                }
                let v = &mut self.v3[s];
                v.note.copy_from_slice(note);
                v.state = SlotState::Note;
                v.opening = [0; v3::OPEN_LEN];
                Ok(vec![])
            }
            v3::INS_GET_SLOT => {
                let s = slot()?;
                len_is(0)?;
                let v = &self.v3[s];
                let mut out = vec![v.state.byte()];
                match (&self.slots[s], v.state) {
                    (Some(kp), st) if st != SlotState::Empty => out.extend_from_slice(&kp.x_only_public_key().0.serialize()),
                    _ => out.extend_from_slice(&[0; 32]),
                }
                out.extend_from_slice(&v.note);
                out.extend_from_slice(&v.spent);
                out.extend_from_slice(&v.opening);
                Ok(out)
            }
            v3::INS_GET_STATE => {
                len_is(0)?;
                let mut out: Vec<u8> = self.v3.iter().map(|v| v.state.byte()).collect();
                out.resize(32, 0);
                out.extend_from_slice(&self.clock.now.to_be_bytes());
                out.extend_from_slice(&self.clock.window.to_be_bytes());
                out.extend_from_slice(&self.clock.limit.to_be_bytes());
                out.extend_from_slice(&self.clock.spent.to_be_bytes());
                out.push(self.cert.len() as u8);
                out.push(u8::from(self.attest.is_some()));
                out.push(u8::from(self.registry_key.is_some()));
                // The PIN: its state, the wrong tries in a row, the askings owed (big-endian).
                out.push(u8::from(!self.pin_not_set()));
                out.push(self.wrong);
                out.extend_from_slice(&self.owed.to_be_bytes());
                out.push(u8::from(self.anchored));
                // The signer: neither the chip's RSA path nor a nonce key in RAM.
                out.push(0);
                Ok(out)
            }
            v3::INS_SET_CHANGE => {
                let s = slot()?;
                // After SET_PIN, change needs this selection's PIN (its one sender holds a session).
                if !self.pin_not_set() && !self.pin_given() {
                    return Err(StatusWord(SW_PIN_REQUIRED).into());
                }
                len_is(8 + v3::OPEN_LEN)?;
                if self.v3[s].state != SlotState::Keyed {
                    return Err(refuse());
                }
                if body[..8] == [0; 8] {
                    return Err(StatusWord(v3::SW_WRONG_DATA).into());
                }
                let v = &mut self.v3[s];
                v.note = [0; v3::NOTE_LEN];
                v.note[..8].copy_from_slice(&body[..8]);
                v.opening.copy_from_slice(&body[8..]);
                v.state = SlotState::Change;
                Ok(vec![])
            }
            v3::INS_SET_LIMIT => {
                len_is(8)?;
                if !self.pin_not_set() {
                    return Err(refuse());
                }
                self.clock.limit = u64::from_be_bytes(body.try_into().expect("8"));
                Ok(vec![])
            }
            v3::INS_SET_HEADER => {
                len_is(v3::HEADER_LEN)?;
                let h = body.to_vec();
                self.set_header_v3(&h)
            }
            v3::INS_ATTEST_INIT => {
                len_is(0)?;
                if !self.pin_not_set() || !self.cert.is_empty() {
                    return Err(refuse());
                }
                // The PIN key first, its binding cleared, then the attestation key (§ 4's order).
                self.pin_key = Some(secp256k1::SecretKey::new(&mut rand::thread_rng()));
                self.pin_binding.clear();
                let key = p256::ecdsa::SigningKey::random(&mut rand::thread_rng());
                let w = key.verifying_key().to_encoded_point(false).as_bytes().to_vec();
                self.attest = Some(key);
                Ok(w)
            }
            v3::INS_SET_CERT => {
                if body.is_empty() || body.len() > v3::CERT_MAX {
                    return Err(StatusWord(v3::SW_WRONG_LENGTH).into());
                }
                if !self.pin_not_set() || !self.cert.is_empty() || self.attest.is_none() {
                    return Err(refuse());
                }
                self.cert = body.to_vec();
                Ok(vec![])
            }
            v3::INS_GET_CERT => {
                len_is(0)?;
                if self.cert.is_empty() {
                    return Err(StatusWord(v3::SW_RECORD_NOT_FOUND).into());
                }
                Ok(self.cert.clone())
            }
            v3::INS_ATTEST => {
                len_is(32)?;
                let (Some(key), false, Some(Some(kp))) = (&self.attest, self.cert.is_empty(), self.slots.first()) else {
                    return Err(refuse());
                };
                if self.v3[0].state == SlotState::Empty {
                    return Err(refuse());
                }
                use p256::ecdsa::signature::Signer as _;
                let challenge: [u8; 32] = body.try_into().expect("32");
                let msg = v3::attest_message(&challenge, &kp.x_only_public_key().0.serialize());
                let sig: p256::ecdsa::Signature = key.sign(&msg);
                Ok(sig.to_der().as_bytes().to_vec())
            }
            v3::INS_SET_REGISTRY_KEY => {
                len_is(65)?;
                if !self.pin_not_set() {
                    return Err(refuse());
                }
                if body[0] != 0x04 {
                    return Err(StatusWord(v3::SW_WRONG_DATA).into());
                }
                let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(body).map_err(|_| StatusWord(v3::SW_WRONG_DATA))?;
                self.registry_key = Some(key);
                Ok(vec![])
            }
            v3::INS_RELEASE => {
                let s = slot()?;
                if body.len() < 32 + v3::RELEASE_SIG_MIN || body.len() > 32 + v3::RELEASE_SIG_MAX {
                    return Err(StatusWord(v3::SW_WRONG_LENGTH).into());
                }
                let (Some(key), Some(Some(kp))) = (&self.registry_key, self.slots.first()) else {
                    return Err(refuse());
                };
                if self.v3[0].state == SlotState::Empty {
                    return Err(refuse());
                }
                let txid: [u8; 32] = body[..32].try_into().expect("32");
                let slot0 = kp.x_only_public_key().0.serialize();
                if !v3::verify_release(key, &slot0, s as u8, &txid, &body[32..]) {
                    return Err(StatusWord(v3::SW_SECURITY_STATUS_NOT_SATISFIED).into());
                }
                let v = &mut self.v3[s];
                match v.state {
                    SlotState::Spent if v.spent == txid => {
                        v.state = SlotState::Note;
                        v.spent = [0; 32];
                        v.sig = [0; 64];
                    }
                    SlotState::Change if v.opening[..32] == txid => {
                        v.state = SlotState::Keyed;
                        v.opening = [0; v3::OPEN_LEN];
                        v.note = [0; v3::NOTE_LEN];
                    }
                    _ => return Err(refuse()),
                }
                Ok(vec![v.state.byte()])
            }
            sealed::INS_GET_NONCE => self.get_nonce_v4(apdu),
            _ => Err(StatusWord(SW_INS_NOT_SUPPORTED).into()),
        }
    }
}

impl SoftCard {
    /// What the chip does with any command when the applet is not installed: CLA `B0` has no
    /// dispatcher, so every instruction answers `6E00` — not `6A82`, which is what `SELECT`
    /// gives. Modelled here because the trait methods used to skip the APDU layer and accept
    /// commands a blank card rejects; a real J3R180 answered `6E00` to `GET_PUB` on 2026-10-28.
    fn require_installed(&self) -> Result<()> {
        if self.installed { Ok(()) } else { Err(StatusWord(SW_CLA_NOT_SUPPORTED).into()) }
    }

    /// The applet's `checkPinReady`: no PIN outranks no PIN in this session (and on a 2.x card
    /// blocked outranks both). `SIGN` always calls it, so a card that has never been pinned cannot
    /// sign, and a fresh `SELECT` needs its own `VERIFY_PIN`.
    fn require_pin_session(&self) -> Result<()> {
        if !self.is_v3() && self.tries_left == 0 {
            return Err(StatusWord(SW_PIN_BLOCKED).into());
        }
        if self.pin.is_none() {
            return Err(StatusWord(SW_NO_PIN_SET).into());
        }
        if !self.pin_given() {
            return Err(StatusWord(SW_PIN_REQUIRED).into());
        }
        Ok(())
    }

    /// The applet's `process` before its dispatch: any command but VERIFY_PIN, SELECT among them,
    /// begins the PIN's stretch again. The trait's GEN_KEY, GET_PUB and SIGN skip [`Card::apdu`],
    /// so they call it themselves.
    fn not_an_asking(&mut self) {
        self.asked = 0;
    }
}

impl Card for SoftCard {
    fn gen_key(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        self.require_installed()?;
        self.not_an_asking();
        if self.slot(slot)?.is_some() {
            return Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into());
        }
        let kp = Keypair::new(SECP256K1, &mut rand::thread_rng());
        self.slots[slot as usize] = Some(kp);
        self.v3[slot as usize] = V3Slot { state: SlotState::Keyed, ..V3Slot::empty() };
        Ok(kp.x_only_public_key().0)
    }

    fn gen_key_overwrite(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        self.require_installed()?;
        self.not_an_asking();
        let occupied = self.slot(slot)?.is_some();
        if slot == 0 && !self.cert.is_empty() {
            // As the applet: slot 0's key is the one the certificate names, and a certificate is
            // taken only once, so the re-key form refuses slot 0 after SET_CERT, before anything
            // is written.
            return Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into());
        }
        if self.is_v3() {
            // 3.0: a slot holding a note, or owed change, holds value and is never re-keyed; a
            // KEYED or SPENT one takes the PIN once the card has one. An empty slot is keyed as
            // by a plain GEN_KEY.
            if occupied {
                if matches!(self.v3[slot as usize].state, SlotState::Note | SlotState::Change) {
                    return Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into());
                }
                if !self.pin_not_set() {
                    self.require_pin_session()?;
                }
            }
        } else if self.pin.is_some() || self.tries_left == 0 {
            // Replacing a key destroys whatever is locked to it, so once the card has a PIN the
            // overwrite needs `SIGN`'s PIN session. Before the first `SET_PIN` nothing can be
            // spent, so provisioning re-keys freely.
            self.require_pin_session()?;
        }
        let kp = Keypair::new(SECP256K1, &mut rand::thread_rng());
        self.slots[slot as usize] = Some(kp);
        self.v3[slot as usize] = V3Slot { state: SlotState::Keyed, ..V3Slot::empty() };
        Ok(kp.x_only_public_key().0)
    }

    fn get_pub(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        self.require_installed()?;
        self.not_an_asking();
        match self.slot(slot)? {
            Some(kp) => Ok(kp.x_only_public_key().0),
            None => Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into()),
        }
    }

    fn sign(&mut self, slot: u8, msg32: &[u8; 32]) -> Result<Signature> {
        self.require_installed()?;
        self.not_an_asking();
        // The gate comes first, as on the applet: without the PIN, a reader learns nothing
        // about the slots.
        self.require_pin_session()?;
        let Some(kp) = *self.slot(slot)? else {
            return Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into());
        };
        let sig = || SECP256K1.sign_schnorr(&Message::from_digest(*msg32), &kp);
        if !self.is_v3() {
            return Ok(sig());
        }
        let v = &self.v3[slot as usize];
        match v.state {
            // The one transaction this slot signed, asked again: the stored signature, byte for
            // byte (the applet computes one signature per key in its life). Spends nothing new.
            SlotState::Spent if &v.spent == msg32 => Ok(Signature::from_slice(&v.sig).expect("stored signature")),
            SlotState::Note => {
                let amount = u64::from_be_bytes(v.note[..8].try_into().expect("8"));
                let (window, spent) = self.day_after(amount)?;
                // The applet's signer checks its signature before anything is stored; one that
                // fails is 6F00 with the slot still NOTE.
                if std::mem::take(&mut self.fail_sign_check) {
                    return Err(sign_check_failed());
                }
                let s = sig();
                let v = &mut self.v3[slot as usize];
                v.spent = *msg32;
                v.sig = s.serialize();
                v.state = SlotState::Spent;
                self.clock.window = window;
                self.clock.spent = spent;
                Ok(s)
            }
            _ => Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into()),
        }
    }

    fn applet_version(&mut self) -> Result<AppletVersion> {
        self.require_installed()?;
        Ok(self.version)
    }

    /// The APDU layer, modelled on the applet's dispatch: SELECT by AID, `GET DATA FF21`, the
    /// three PIN commands and, on a 3.0 card, its commands. Unknown instructions answer `6D00`,
    /// like the applet.
    fn apdu(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        ensure!(apdu.len() >= 4, "APDU shorter than its header");
        let (cla, ins) = (apdu[0], apdu[1]);
        self.ins_log.push(ins);
        if ins != INS_VERIFY_PIN {
            self.not_an_asking();
        }
        match (cla, ins) {
            // The applet refuses any CLA but its own before looking at the INS.
            (c, _) if c != CLA && c != CLA_ISO => Err(StatusWord(SW_CLA_NOT_SUPPORTED).into()),
            (CLA_ISO, 0xA4) => {
                let lc = apdu[4] as usize;
                ensure!(apdu.len() >= 5 + lc, "SELECT too short for its Lc");
                if !self.installed || &apdu[5..5 + lc] != SELECT_APDU_HEAD {
                    return Err(StatusWord(0x6A82).into());
                }
                // A session lives inside a selection: a fresh SELECT needs its own VERIFY_PIN, and
                // its own anchor.
                self.validated = false;
                self.given_by_set = false;
                self.anchored = false;
                self.nonce = None;
                // A 3.0 or later applet answers its version. The 2.x stand-in has always answered
                // no data, and the hosts take that for 2.x.
                Ok(if self.is_v3() { vec![self.version.major, self.version.minor] } else { vec![] })
            }
            (CLA_ISO, 0xCA) if apdu[2] == 0xFF && apdu[3] == 0x21 => {
                // TLV: 81 applications, 82 non-volatile, 83 volatile. One free slot's worth.
                Ok(vec![0xFF, 0x21, 0x09, 0x81, 0x01, 0x01, 0x82, 0x02, 0x10, 0x00, 0x83, 0x00])
            }
            (CLA, INS_SET_PIN) if self.is_v3() => self.set_pin_v4(apdu),
            (CLA, INS_VERIFY_PIN) if self.is_v3() => self.verify_pin_v4(apdu),
            (CLA, INS_CHANGE_PIN) if self.is_v3() => self.change_pin_v4(apdu),
            (CLA, INS_SET_PIN) => {
                let pin = apdu_data(apdu)?;
                ensure!((PIN_MIN..=PIN_MAX).contains(&pin.len()), "PIN must be {PIN_MIN} to {PIN_MAX} bytes");
                if self.tries_left == 0 {
                    return Err(StatusWord(SW_PIN_BLOCKED).into());
                }
                if self.pin.is_some() {
                    return Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into());
                }
                self.pin = Some(pin.to_vec());
                self.tries_left = 3;
                Ok(vec![])
            }
            (CLA, INS_VERIFY_PIN) => {
                let pin = apdu_data(apdu)?;
                if self.tries_left == 0 {
                    return Err(StatusWord(SW_PIN_BLOCKED).into());
                }
                let Some(want) = &self.pin else {
                    return Err(StatusWord(SW_NO_PIN_SET).into());
                };
                if want.as_slice() == pin {
                    self.tries_left = 3;
                    self.validated = true;
                    return Ok(vec![]);
                }
                // A wrong PIN leaves nothing open.
                self.validated = false;
                self.tries_left -= 1;
                // `63 CX` with X tries left, as the applet answers (`PinTest`); the try that
                // spends the last one is the block itself and answers `6983`, not `63C0`.
                if self.tries_left == 0 {
                    return Err(StatusWord(SW_PIN_BLOCKED).into());
                }
                Err(StatusWord(0x63C0 | u16::from(self.tries_left)).into())
            }
            (CLA, _) if self.is_v3() => self.apdu_v3(apdu),
            // The 3.0 and 4.0 applets take only SELECT under the ISO class.
            (CLA_ISO, _) if self.is_v3() => Err(StatusWord(SW_CLA_NOT_SUPPORTED).into()),
            _ => Err(StatusWord(SW_INS_NOT_SUPPORTED).into()),
        }
    }
}

/// The data field of a command that may have none: `[CLA INS P1 P2]`, `[… Le]`, or
/// `[… Lc data… (Le)]`. A field shorter than its Lc is `6700`.
fn apdu_body(apdu: &[u8]) -> Result<&[u8]> {
    if apdu.len() <= 5 {
        return Ok(&[]);
    }
    let lc = apdu[4] as usize;
    if apdu.len() < 5 + lc {
        return Err(StatusWord(v3::SW_WRONG_LENGTH).into());
    }
    Ok(&apdu[5..5 + lc])
}

/// The applet's `lcOf`: the Lc byte, or one past any PIN command's (17) for an Lc of 128 and up.
fn lc_of(apdu: &[u8]) -> usize {
    match apdu.get(4) {
        None => 0,
        Some(&b) if b >= 0x80 => 2 * PIN_MAX + 1,
        Some(&b) => b as usize,
    }
}

/// What the applet's receive gives: the data bytes present, at most Lc of them.
fn received(apdu: &[u8]) -> &[u8] {
    if apdu.len() <= 5 {
        return &[];
    }
    &apdu[5..apdu.len().min(5 + apdu[4] as usize)]
}

/// The AID bytes `SELECT` names, without the header/Lc that precede them.
const SELECT_APDU_HEAD: &[u8] = &[0xF0, 0x46, 0x4D, 0x43, 0x41, 0x52, 0x44, 0x01];

/// The data field of a short APDU: `Lc` at byte 4, data from byte 5.
fn apdu_data(apdu: &[u8]) -> Result<&[u8]> {
    let lc = apdu[4] as usize;
    ensure!(apdu.len() >= 5 + lc, "APDU shorter than its Lc");
    Ok(&apdu[5..5 + lc])
}

/// Pick a backend from the environment.
///
/// * `CARD_BACKEND=soft` (default): [`SoftCard`].
/// * `CARD_BACKEND=bridge`: HTTP bridge at `CARD_BRIDGE_URL` (default `http://127.0.0.1:9876`).
/// * `CARD_BACKEND=pcsc`: first reader, or the one named by `CARD_PCSC_READER`; needs the `pcsc` feature.
pub fn open_from_env() -> Result<Box<dyn Card>> {
    let backend = std::env::var("CARD_BACKEND").unwrap_or_else(|_| "soft".into());
    match backend.as_str() {
        "soft" => Ok(Box::new(SoftCard::new())),
        "bridge" => {
            let url = bridge::url_from_env();
            let mut c = CardClient::new(BridgeTransport::new(&url));
            c.select().with_context(|| format!("bridge at {url}"))?;
            Ok(Box::new(c))
        }
        #[cfg(feature = "pcsc")]
        "pcsc" => {
            let mut c = CardClient::new(pcsc_backend::PcscTransport::connect(std::env::var("CARD_PCSC_READER").ok().as_deref())?);
            c.select()?;
            Ok(Box::new(c))
        }
        #[cfg(not(feature = "pcsc"))]
        "pcsc" => bail!("CARD_BACKEND=pcsc needs `--features pcsc`"),
        other => bail!("unknown CARD_BACKEND {other:?} (soft|bridge|pcsc)"),
    }
}

#[cfg(test)]
mod slot_split_tests {
    use super::*;

    // Setup keys slots 0 to 7 on every card: none of them is a change slot.
    const _: () = assert!(CHANGE_SLOTS_FROM >= MIN_SLOTS);

    #[test]
    fn the_last_eight_slots_are_kept_for_offline_change() {
        let change: Vec<u8> = (0..=u8::MAX).filter(|s| is_change_slot(*s)).collect();
        assert_eq!(change, (24..=31).collect::<Vec<u8>>());
        assert_eq!(SLOTS - CHANGE_SLOTS_FROM, 8);
        assert!((0..MIN_SLOTS).all(|s| !is_change_slot(s)));
    }
}
