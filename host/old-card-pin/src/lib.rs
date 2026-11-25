//! The PIN commands of applets **older than 4.0** (a 2.x or 3.x card) in those applets' form
//! (P1 00), kept for tests only.
//!
//! The 4.0 till, the POS phones and the command line never send them: a card older than 4.0 gets
//! the old-card refusal before any PIN command (docs/OFFLINE-PROTOCOL.md § 9). Only the Setup app
//! keeps this form, inside the reset of a card older than 4.0.
//!
//! This crate is what is left of the P1 00 form in Rust, for the tests of 2.x and 3.x cards and of
//! the applet's refusals. `fedimint-card-host-card` exports no P1 00 PIN builder, and the product
//! crates (`fedimint-card-sidecar`, `fedimint-card-pos-core`) may only take this one as a
//! dev-dependency, which their library code cannot name in any build. `tests/not_in_the_product.rs`
//! fails if either lists it as a dependency or names it in its sources.

use anyhow::{Context, Result, ensure};
use fedimint_card_host_card::{
    CLA, Card, ClearPinRefused, INS_CHANGE_PIN, INS_SET_PIN, INS_VERIFY_PIN, PIN_MAX, PIN_MIN, SW_NO_PIN_SET, pin_from_env, serve_wait,
    status_word,
};

/// A P1 00 PIN APDU (`SET_PIN` or `VERIFY_PIN`): CLA, INS, P1 = P2 = 0, the PIN, Le = 0.
///
/// The bounds are the applet's (`PIN_MIN`..`PIN_MAX`), not a preference: an out-of-range length is
/// refused here so the card is never asked and cannot burn a try.
pub fn pin_apdu(ins: u8, pin: &[u8]) -> Result<Vec<u8>> {
    ensure!((PIN_MIN..=PIN_MAX).contains(&pin.len()), "PIN must be {PIN_MIN} to {PIN_MAX} bytes, not {}", pin.len());
    let mut apdu = vec![CLA, ins, 0x00, 0x00, pin.len() as u8];
    apdu.extend_from_slice(pin);
    apdu.push(0x00);
    Ok(apdu)
}

/// `CHANGE_PIN` APDU with P1 00: P1 = the old PIN's length, P2 = 0, data old ‖ new, Le = 0. Both
/// PINs are held to the applet's bounds here, so a malformed one never reaches the card.
pub fn change_pin_apdu(old: &[u8], new: &[u8]) -> Result<Vec<u8>> {
    for (what, p) in [("old", old), ("new", new)] {
        ensure!((PIN_MIN..=PIN_MAX).contains(&p.len()), "{what} PIN must be {PIN_MIN} to {PIN_MAX} bytes, not {}", p.len());
    }
    let mut apdu = vec![CLA, INS_CHANGE_PIN, old.len() as u8, 0x00, (old.len() + new.len()) as u8];
    apdu.extend_from_slice(old);
    apdu.extend_from_slice(new);
    apdu.push(0x00);
    Ok(apdu)
}

/// The P1 00 PIN commands on any [`Card`], for applets below 4.0. A 4.x card answers them `6B00`,
/// and a `CardClient` sends none of them at all: it refuses a P1 00 PIN to a card whose SELECT
/// showed 4.x ([`ClearPinRefused`]) and any PIN command to a card whose SELECT did not pass.
pub trait ClearPin: Card {
    /// `SET_PIN` with P1 00. `6985` when a PIN is already set.
    fn set_pin(&mut self, pin: &[u8]) -> Result<()> {
        self.apdu(&pin_apdu(INS_SET_PIN, pin)?).map(|_| ())
    }

    /// One `VERIFY_PIN` with P1 00. `6984` when no PIN is set; a 2.x card answers `63CX` with X
    /// tries left and `6983` when blocked.
    fn verify_pin(&mut self, pin: &[u8]) -> Result<()> {
        self.apdu(&pin_apdu(INS_VERIFY_PIN, pin)?).map(|_| ())
    }

    /// `CHANGE_PIN` with P1 00: needs this session's PIN and ends it.
    fn change_pin(&mut self, old: &[u8], new: &[u8]) -> Result<()> {
        self.apdu(&change_pin_apdu(old, new)?).map(|_| ())
    }
}

impl<C: Card + ?Sized> ClearPin for C {}

/// VERIFY_PIN with P1 00, resent while the card answers `6A98`. A 4.x card is refused before
/// anything is sent ([`ClearPinRefused`]).
pub fn verify_pin_held_clear(card: &mut dyn Card, pin: &[u8], progress: &mut dyn FnMut(u16)) -> Result<()> {
    if card.applet_version()?.is_current() {
        return Err(anyhow::Error::new(ClearPinRefused).context("applet 4.x: use the sealed PIN with a TrustedCard"));
    }
    serve_wait(0, progress, &mut || card.verify_pin(pin))
}

/// SET_PIN if the card has no PIN, then VERIFY_PIN (an old card's SET_PIN opens nothing), with
/// P1 00. A 4.x card is refused unsent.
pub fn open_pin_session_with(card: &mut dyn Card, pin: &[u8]) -> Result<()> {
    match verify_pin_held_clear(card, pin, &mut |_| {}) {
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
