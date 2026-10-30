//! Host side of the Fedimint spend-key card.
//!
//! The card holds secp256k1 keys in numbered slots and exposes three commands: `GEN_KEY`,
//! `GET_PUB` and `SIGN` (BIP340 over a 32-byte message). It never reveals a private key.
//!
//! * [`Card`] is the high-level API the federation tests use.
//! * [`CardClient`] implements it over any APDU [`Transport`] (bridge or PC/SC).
//! * [`SoftCard`] implements it in-process with real keys, for tests without a card.

use anyhow::{Context, Result, anyhow, bail, ensure};
use secp256k1::schnorr::Signature;
use secp256k1::{Keypair, Message, SECP256K1, XOnlyPublicKey};

pub mod bridge;
#[cfg(feature = "pcsc")]
pub mod pcsc_backend;

pub use bridge::{BridgeError, BridgeTransport};

/// Number of key slots on the applet.
pub const SLOTS: u8 = 8;
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

/// Returns the status word if `err` came from the card refusing a command.
pub fn status_word(err: &anyhow::Error) -> Option<u16> {
    err.downcast_ref::<StatusWord>().map(|s| s.0)
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
    /// BIP340 signature over the raw 32 bytes.
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
        Err(anyhow!("this card backend cannot send raw APDUs"))
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
}

impl<T: Transport> CardClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Select the applet. Needed on a real card; harmless on the bridge.
    pub fn select(&mut self) -> Result<()> {
        self.exchange(&SELECT_APDU).map(|_| ())
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
        self.exchange(apdu)
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
/// it is out of tries.
#[derive(Default)]
pub struct SoftCard {
    slots: [Option<Keypair>; SLOTS as usize],
    pin: Option<Vec<u8>>,
    tries_left: u8,
    /// Whether the applet is installed. A blank card answers `6A82` to SELECT.
    installed: bool,
}

impl SoftCard {
    pub fn new() -> Self {
        Self { tries_left: 3, installed: true, ..Default::default() }
    }

    /// A card with no applet on it: SELECT answers `6A82`, and every slot is empty.
    pub fn blank() -> Self {
        Self { tries_left: 3, installed: false, ..Default::default() }
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
        for slot in 0..SLOTS {
            if self.slots[slot as usize].is_none() {
                self.slots[slot as usize] = Some(Keypair::new(SECP256K1, &mut rand::thread_rng()));
            }
        }
    }

    fn slot(&self, slot: u8) -> Result<&Option<Keypair>> {
        self.slots.get(slot as usize).ok_or_else(|| StatusWord(0x6B00).into())
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
}

impl Card for SoftCard {
    fn gen_key(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        self.require_installed()?;
        if self.slot(slot)?.is_some() {
            return Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into());
        }
        let kp = Keypair::new(SECP256K1, &mut rand::thread_rng());
        self.slots[slot as usize] = Some(kp);
        Ok(kp.x_only_public_key().0)
    }

    fn gen_key_overwrite(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        self.require_installed()?;
        self.slot(slot)?;
        let kp = Keypair::new(SECP256K1, &mut rand::thread_rng());
        self.slots[slot as usize] = Some(kp);
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
        match self.slot(slot)? {
            Some(kp) => Ok(SECP256K1.sign_schnorr(&Message::from_digest(*msg32), kp)),
            None => Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into()),
        }
    }

    /// The APDU layer, modelled on the applet's dispatch: SELECT by AID, `GET DATA FF21`, and the
    /// three PIN commands. Unknown instructions answer `6D00`, like the applet.
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
                Ok(vec![])
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
                    return Ok(vec![]);
                }
                self.tries_left -= 1;
                Err(StatusWord(0x6300 | u16::from(self.tries_left)).into())
            }
            _ => Err(StatusWord(SW_INS_NOT_SUPPORTED).into()),
        }
    }
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
