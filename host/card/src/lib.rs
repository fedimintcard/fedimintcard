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
pub mod v3;

pub use v3::{AppletVersion, CardStatus, Certificate, Opening, SlotInfo, SlotState};

pub use bridge::{BridgeError, BridgeTransport};

/// The most key slots an applet has (applet 2.1). The host never asks beyond it.
///
/// A card's own count comes from the card, never from this constant: walk `GET_PUB` up from slot
/// 0 until the applet answers [`SW_SLOT_OUT_OF_RANGE`] (see [`past_last_slot`]). Applet 2.0 cards,
/// prod card `15989f91` among them, have [`MIN_SLOTS`] and keep working.
pub const SLOTS: u8 = 32;
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
/// Blocked after three wrong PINs; terminal.
pub const SW_PIN_BLOCKED: u16 = 0x6983;
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
const CLA: u8 = 0xB0;
pub const INS_VERIFY_PIN: u8 = 0x40;
pub const INS_SET_PIN: u8 = 0x41;
pub const INS_CHANGE_PIN: u8 = 0x42;
const INS_SIGN: u8 = 0x12;
/// `SELECT` by AID `F0 46 4D 43 41 52 44 01`.
pub const SELECT_APDU: [u8; 14] = [
    0x00, 0xA4, 0x04, 0x00, 0x08, 0xF0, 0x46, 0x4D, 0x43, 0x41, 0x52, 0x44, 0x01, 0x00,
];

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

/// Provision the PIN if the card has none, then open this selection's session with it.
///
/// `SIGN` refuses until both are true, so anything that spends must call this after `SELECT` and
/// after the keys are in place. A card that already holds a different PIN answers `63 CX` and
/// spends a try, so only call it on a card whose PIN you own.
pub fn open_pin_session_with(card: &mut dyn Card, pin: &[u8]) -> Result<()> {
    match card.verify_pin(pin) {
        // Already verified: the session is open.
        Ok(()) => Ok(()),
        Err(e) if status_word(&e) == Some(SW_NO_PIN_SET) => {
            card.set_pin(pin).context("SET_PIN on a fresh card")?;
            card.verify_pin(pin).context("VERIFY_PIN after SET_PIN")
        }
        Err(e) => Err(e).context("VERIFY_PIN"),
    }
}

/// [`open_pin_session_with`] using [`pin_from_env`].
pub fn open_pin_session(card: &mut dyn Card) -> Result<()> {
    open_pin_session_with(card, &pin_from_env())
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

    /// `SET_PIN` (once per card). `6985` when a PIN is already set, `6983` when blocked.
    fn set_pin(&mut self, pin: &[u8]) -> Result<()> {
        self.apdu(&pin_apdu(INS_SET_PIN, pin)?).map(|_| ())
    }

    /// `VERIFY_PIN` for this session. `6984` when no PIN is set, `63 CX` with X tries left,
    /// `6983` when blocked. `6A94` is what `SIGN` answers once the session has no verified PIN.
    fn verify_pin(&mut self, pin: &[u8]) -> Result<()> {
        self.apdu(&pin_apdu(INS_VERIFY_PIN, pin)?).map(|_| ())
    }

    // ------------------------------------------------------------ applet 3.0 (v3.rs)

    /// The applet's version. A backend that kept `SELECT`'s answer gives it; otherwise the card is
    /// asked `GET_STATE`, which a 3.0 applet answers and a 2.x one refuses (`6D00`), so the PIN
    /// session is left alone (a `SELECT` would end it). A card that cannot be asked is taken for
    /// 2.x: it then gets none of 3.0's commands.
    fn applet_version(&mut self) -> Result<AppletVersion> {
        match self.apdu(&v3_apdu(v3::INS_GET_STATE, 0, &[])) {
            Ok(_) => Ok(AppletVersion::V3_0),
            Err(e) if matches!(status_word(&e), Some(SW_INS_NOT_SUPPORTED) | Some(SW_CLA_NOT_SUPPORTED)) => Ok(AppletVersion::V2_1),
            Err(e) if e.downcast_ref::<NoRawApdu>().is_some() => Ok(AppletVersion::V2_1),
            Err(e) => Err(e).context("asking the card for its version (GET_STATE)"),
        }
    }

    /// `SET_NOTE`: the note on a KEYED slot, or the finished change on a CHANGE slot (same amount,
    /// else `6A80`). No PIN.
    fn set_note(&mut self, slot: u8, amount_msat: u64, sig48: &[u8; 48]) -> Result<()> {
        let mut d = amount_msat.to_be_bytes().to_vec();
        d.extend_from_slice(sig48);
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

/// `SET_PIN`/`VERIFY_PIN` APDU: the applet's CLA `B0` (not ISO `00`), P1 = P2 = 0, the PIN as Lc
/// bytes, Le = 0.
///
/// The CLA is load-bearing: `FedimintCardApplet.process` refuses anything but `B0` before it looks
/// at the INS, so an ISO-CLA PIN command gets `6E00` on a card that has the PIN applet installed —
/// indistinguishable from a card that does not. Measured on a J3R180 on 2026-10-28.
///
/// The bounds are the applet's (`PIN_MIN`..`PIN_MAX`), not a preference: an out-of-range length is
/// refused here so the card is never asked and cannot burn a try.
pub fn pin_apdu(ins: u8, pin: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        (PIN_MIN..=PIN_MAX).contains(&pin.len()),
        "PIN must be {PIN_MIN} to {PIN_MAX} bytes, not {}",
        pin.len()
    );
    let mut apdu = vec![CLA, ins, 0x00, 0x00, pin.len() as u8];
    apdu.extend_from_slice(pin);
    apdu.push(0x00);
    Ok(apdu)
}

/// Sends one APDU and returns `data || SW1 SW2`.
pub trait Transport {
    fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>>;
}

pub struct CardClient<T: Transport> {
    transport: T,
    /// What the last `select` answered, when it was a version (two bytes).
    selected: Option<AppletVersion>,
}

impl<T: Transport> CardClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport, selected: None }
    }

    /// Select the applet. Needed on a real card; harmless on the bridge. The applet answers its
    /// version, kept for [`Card::applet_version`].
    pub fn select(&mut self) -> Result<()> {
        let out = self.exchange(&SELECT_APDU)?;
        self.selected = AppletVersion::from_select(&out);
        Ok(())
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
        let out = self.exchange(apdu)?;
        if apdu.get(..SELECT_APDU.len() - 1) == Some(&SELECT_APDU[..SELECT_APDU.len() - 1]) {
            self.selected = AppletVersion::from_select(&out);
        }
        Ok(out)
    }

    fn applet_version(&mut self) -> Result<AppletVersion> {
        match self.selected {
            Some(v) => Ok(v),
            None => {
                let v = match self.exchange(&v3_apdu(v3::INS_GET_STATE, 0, &[])) {
                    Ok(_) => AppletVersion::V3_0,
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
        let out = self.exchange(&apdu)?;
        Signature::from_slice(&out).context("card returned an invalid signature")
    }
}

/// In-process stand-in for the applet, with the same slot semantics (including `6985`) and the
/// same PIN state machine: `SET_PIN` once, three tries, `6984` before a PIN exists, `6983` after
/// it is out of tries. [`SoftCard::new`] is a 2.1 card with [`SLOTS`] slots;
/// [`SoftCard::with_slots`] makes one of any size, such as a 2.0 card with [`MIN_SLOTS`].
///
/// [`SoftCard::v3`] is an applet 3.0 card (`FedimintCardApplet.java`, docs/OFFLINE-PROTOCOL.md
/// § 1), followed exactly: slot states, sign once per slot (the same hash may be signed again),
/// notes and change, the day's limit on a clock that only a Bitcoin header with the work moves,
/// and a P-256 attestation key with its certificate.
#[derive(Default)]
pub struct SoftCard {
    /// One entry per slot the card has; a slot past the end answers `6B00`.
    slots: Vec<Option<Keypair>>,
    pin: Option<Vec<u8>>,
    tries_left: u8,
    /// Whether this session has verified the PIN, which is what lets `SIGN` run. Cleared by
    /// `SELECT`, as on the applet: a session is a selected applet, not a card.
    validated: bool,
    /// Whether the applet is installed. A blank card answers `6A82` to SELECT.
    installed: bool,
    /// What SELECT answers. 3.0 and later get [`Self::v3`]'s rules.
    version: AppletVersion,
    /// 3.0: per slot, its state, note, spent hash and opening (one entry per slot).
    v3: Vec<V3Slot>,
    clock: Clock,
    cert: Vec<u8>,
    attest: Option<p256::ecdsa::SigningKey>,
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
    pub fn new() -> Self {
        Self::with_slots(SLOTS)
    }

    /// An installed card with `n` key slots: slot `n` and up answer `6B00`.
    pub fn with_slots(n: u8) -> Self {
        Self {
            slots: vec![None; n as usize],
            tries_left: 3,
            installed: true,
            v3: vec![V3Slot::empty(); n as usize],
            ..Default::default()
        }
    }

    /// An applet 3.0 card: 32 slots and the 3.0 rules.
    pub fn v3() -> Self {
        Self { version: AppletVersion::V3_0, ..Self::with_slots(SLOTS) }
    }

    /// A card with no applet on it: SELECT answers `6A82`, and every slot is empty.
    pub fn blank() -> Self {
        Self { installed: false, ..Self::new() }
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
            tries_left: 3,
            validated: false,
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

    fn set_header_v3(&mut self, h: &[u8]) -> Result<Vec<u8>> {
        let h: &[u8; 80] = h.try_into().map_err(|_| StatusWord(v3::SW_WRONG_LENGTH))?;
        let little_work = || -> anyhow::Error { StatusWord(v3::SW_LITTLE_WORK).into() };
        let wrong_data = || -> anyhow::Error { StatusWord(v3::SW_WRONG_DATA).into() };
        let bits: [u8; 4] = h[72..76].try_into().expect("4");
        let hash = v3::header_work_hash(h);
        let target = v3::target_of(&bits).ok_or_else(wrong_data)?;
        if hash > target {
            return Err(little_work());
        }
        let floor = v3::target_of(&v3::FLOOR_BITS).expect("the floor parses");
        if target > floor {
            return Err(little_work());
        }
        let seen = self.clock.bits != [0; 4];
        if seen {
            let hardest = v3::target_of(&self.clock.bits).ok_or_else(wrong_data)?;
            if target > v3::times_four(&hardest) {
                return Err(little_work());
            }
        }
        let time = v3::header_time(h);
        if time > self.clock.now {
            let harder = !seen || target < v3::target_of(&self.clock.bits).ok_or_else(wrong_data)?;
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
        let len_is = |n: usize| -> Result<()> {
            if body.len() == n { Ok(()) } else { Err(StatusWord(v3::SW_WRONG_LENGTH).into()) }
        };
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
                let s = slot()?;
                len_is(v3::NOTE_LEN)?;
                let v = &mut self.v3[s];
                match v.state {
                    SlotState::Change if v.note[..8] != body[..8] => return Err(StatusWord(v3::SW_WRONG_DATA).into()),
                    SlotState::Change | SlotState::Keyed => {}
                    _ => return Err(refuse()),
                }
                if body[..8] == [0; 8] {
                    return Err(StatusWord(v3::SW_WRONG_DATA).into());
                }
                v.note.copy_from_slice(body);
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
                Ok(out)
            }
            v3::INS_SET_CHANGE => {
                let s = slot()?;
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
        if self.installed {
            Ok(())
        } else {
            Err(StatusWord(SW_CLA_NOT_SUPPORTED).into())
        }
    }

    /// The applet's `checkPinReady`: blocked outranks no PIN, which outranks not verified in
    /// this session. `SIGN` always calls it, so a card that has never been pinned cannot sign, and
    /// a fresh `SELECT` needs its own `VERIFY_PIN`.
    fn require_pin_session(&self) -> Result<()> {
        if self.tries_left == 0 {
            return Err(StatusWord(SW_PIN_BLOCKED).into());
        }
        if self.pin.is_none() {
            return Err(StatusWord(SW_NO_PIN_SET).into());
        }
        if !self.validated {
            return Err(StatusWord(SW_PIN_REQUIRED).into());
        }
        Ok(())
    }
}

impl Card for SoftCard {
    fn gen_key(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        self.require_installed()?;
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
        let occupied = self.slot(slot)?.is_some();
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
        match self.slot(slot)? {
            Some(kp) => Ok(kp.x_only_public_key().0),
            None => Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into()),
        }
    }

    fn sign(&mut self, slot: u8, msg32: &[u8; 32]) -> Result<Signature> {
        self.require_installed()?;
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
        match (cla, ins) {
            // The applet refuses any CLA but its own before looking at the INS.
            (c, _) if c != CLA && c != CLA_ISO => Err(StatusWord(SW_CLA_NOT_SUPPORTED).into()),
            (CLA_ISO, 0xA4) => {
                let lc = apdu[4] as usize;
                ensure!(apdu.len() >= 5 + lc, "SELECT too short for its Lc");
                if !self.installed || &apdu[5..5 + lc] != SELECT_APDU_HEAD {
                    return Err(StatusWord(0x6A82).into());
                }
                // A session lives inside a selection: a fresh SELECT needs its own VERIFY_PIN.
                self.validated = false;
                // A 3.0 applet answers its version. The 2.x stand-in has always answered no
                // data, and the hosts take that for 2.x.
                Ok(if self.is_v3() { vec![self.version.major, self.version.minor] } else { vec![] })
            }
            (CLA_ISO, 0xCA) if apdu[2] == 0xFF && apdu[3] == 0x21 => {
                // TLV: 81 applications, 82 non-volatile, 83 volatile. One free slot's worth.
                Ok(vec![0xFF, 0x21, 0x09, 0x81, 0x01, 0x01, 0x82, 0x02, 0x10, 0x00, 0x83, 0x00])
            }
            (CLA, INS_SET_PIN) => {
                let pin = apdu_data(apdu)?;
                ensure!(
                    (PIN_MIN..=PIN_MAX).contains(&pin.len()),
                    "PIN must be {PIN_MIN} to {PIN_MAX} bytes"
                );
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
            let mut c = CardClient::new(pcsc_backend::PcscTransport::connect(
                std::env::var("CARD_PCSC_READER").ok().as_deref(),
            )?);
            c.select()?;
            Ok(Box::new(c))
        }
        #[cfg(not(feature = "pcsc"))]
        "pcsc" => bail!("CARD_BACKEND=pcsc needs `--features pcsc`"),
        other => bail!("unknown CARD_BACKEND {other:?} (soft|bridge|pcsc)"),
    }
}
