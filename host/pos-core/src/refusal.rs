//! One vocabulary for why a charge did not happen (docs/TILL-API.md): the till's API codes and
//! HTTP statuses, which a phone's offline charge answers too. The till's `classify` asks
//! [`class_of`] first for every cause in an error's chain, then looks at its own errors (the
//! registry's, the federation's).

use fedimint_card_host_card::v3::{SW_KEEP_HOLDING, SW_OVER_LIMIT};
use fedimint_card_host_card::{BridgeError, OLD_CARD_MESSAGE, PIN_OWED_MOST, StatusWord, UnsupportedApplet, is_sign_check_failed};

use crate::plan::PlanError;
use crate::queue::OfflineError;

/// Why a charge was refused, as an app shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub code: &'static str,
    pub detail: String,
    /// `offline_no_change_room`, `offline_pool_exhausted`: prices the card pays instead.
    pub suggested_msat: Option<u64>,
    pub below_msat: Option<u64>,
    pub above_msat: Option<u64>,
    /// `wrong_pin`: the wrong tries in a row (`63 CX`, X; applet 4.0 never blocks).
    pub wrong_tries: Option<u8>,
    /// `pin_wait`: about how many seconds the card still owes before it takes the PIN;
    /// `wrong_pin`: about how long the next try waits. The holder holds the card that long.
    pub wait_s: Option<u32>,
}

/// About how long one asking of a PIN wait takes, in milliseconds: 0.5 s on a J3R180, 0.75 s on
/// a J3R452 (DESIGN § 3). The slower chip's, so "about N s" is not short.
pub const ASKING_MS: u32 = 750;

/// About how many seconds `askings` take, rounded up.
pub fn wait_seconds(askings: u16) -> u32 {
    (u32::from(askings) * ASKING_MS).div_ceil(1_000)
}

/// The askings the next try owes after `wrong` wrong tries in a row: 2^(wrong-1), at most the
/// card's cap (applet 4.0's schedule).
pub fn next_wait(wrong: u8) -> u16 {
    match wrong {
        0 => 0,
        w => (1u16 << (w.min(15) - 1)).min(PIN_OWED_MOST),
    }
}

/// The card's PIN wait was not served to its end: the card left the field while it still owed
/// `askings`. No PIN was tried, nothing changed; tap again and hold the card until it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinWait {
    pub askings: u16,
}

impl std::fmt::Display for PinWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "hold the card: it owes a wait of about {} s before it takes the PIN; tap again and hold it until it answers",
            wait_seconds(self.askings)
        )
    }
}

impl std::error::Error for PinWait {}

/// The HTTP status and code of one cause of an error, when it is one the offline charge raises:
/// [`OfflineError`], a PIN wait the card left in ([`PinWait`]: `pin_wait`), a card older than
/// applet 4.0 (`old_card`), the card's status word (`wrong_pin`, `pin_wait`, `card_limit`, else
/// `card_error`), the card's transport ([`BridgeError`]: `no_card` when the card left), and the
/// planner's [`PlanError`]. SIGN's `6F00` is a context, not a cause: [`class_of_error`] finds it.
pub fn class_of(cause: &(dyn std::error::Error + 'static)) -> Option<(u16, &'static str)> {
    if let Some(o) = cause.downcast_ref::<OfflineError>() {
        return Some(o.class());
    }
    if cause.downcast_ref::<PinWait>().is_some() {
        return Some((423, "pin_wait"));
    }
    if let Some(u) = cause.downcast_ref::<UnsupportedApplet>() {
        return Some(match u {
            UnsupportedApplet::Old { .. } => (409, "old_card"),
            UnsupportedApplet::Newer(_) => (502, "card_error"),
        });
    }
    if let Some(StatusWord(sw)) = cause.downcast_ref::<StatusWord>() {
        return Some(match *sw {
            0x63C0..=0x63CF => (403, "wrong_pin"),
            SW_KEEP_HOLDING => (423, "pin_wait"),
            SW_OVER_LIMIT => (402, "card_limit"),
            _ => (502, "card_error"),
        });
    }
    if let Some(b) = cause.downcast_ref::<BridgeError>() {
        return Some(match b {
            BridgeError::NoCard(_) => (409, "no_card"),
            BridgeError::Unreachable(_) => (502, "relay_unreachable"),
            BridgeError::Http(..) => (502, "card_error"),
        });
    }
    if let Some(p) = cause.downcast_ref::<PlanError>() {
        return Some(match p {
            PlanError::Insufficient { .. } => (402, "insufficient"),
            PlanError::NoExactPlan { .. }
            | PlanError::TooManyNotes(_)
            | PlanError::TooManyInputs { .. }
            | PlanError::NoFreeSlots { .. } => (422, "no_exact_plan"),
        });
    }
    None
}

/// The HTTP status and code of `err`: SIGN's `6F00` (`sign_check_failed`: the card stored no
/// signature, so nothing was spent and the holder taps again), else the first cause
/// [`class_of`] knows.
pub fn class_of_error(err: &anyhow::Error) -> Option<(u16, &'static str)> {
    if is_sign_check_failed(err) {
        return Some((409, "sign_check_failed"));
    }
    err.chain().find_map(class_of)
}

/// `err` as the refusal an app shows: [`class_of_error`] gives the code (else `internal`, 500),
/// the detail says it plainly for the holder.
pub fn refusal(err: &anyhow::Error) -> Refusal {
    let (status, code) = class_of_error(err).unwrap_or((500, "internal"));
    let sw = err.chain().find_map(|c| c.downcast_ref::<StatusWord>()).map(|s| s.0);
    let wrong_tries = (code == "wrong_pin").then(|| sw.map_or(0, |s| (s & 0x0F) as u8));
    let wait = err.chain().find_map(|c| c.downcast_ref::<PinWait>()).map(|w| w.askings);
    let wait_s = match code {
        "wrong_pin" => wrong_tries.map(|w| wait_seconds(next_wait(w))),
        "pin_wait" => Some(wait.map_or(0, wait_seconds)),
        _ => None,
    };
    let detail = match code {
        // Said plainly, for the holder: the wrong tries in a row are the status word's low nibble
        // (`63 CX`), and the next try first owes 2^(X-1) askings.
        "wrong_pin" => {
            format!("wrong PIN; {} wrong in a row, so the next try first waits about {} s", wrong_tries.unwrap_or(0), wait_s.unwrap_or(0))
        }
        "pin_wait" => match wait {
            Some(a) => PinWait { askings: a }.to_string(),
            None => "hold the card: it owes a wait before it takes the PIN; tap again and hold it until it answers".into(),
        },
        "old_card" => OLD_CARD_MESSAGE.into(),
        "sign_check_failed" => "the card's signature failed its own check; nothing was spent, tap again".into(),
        _ => format!("{err:#}"),
    };
    let prices = err.chain().find_map(|c| c.downcast_ref::<OfflineError>()).and_then(OfflineError::suggestions);
    let (suggested_msat, below_msat, above_msat) = prices.unwrap_or_default();
    Refusal { status, code, detail, suggested_msat, below_msat, above_msat, wrong_tries, wait_s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_new_offline_codes_are_plain_strings_beside_the_old_ones() {
        let r = |e: anyhow::Error| refusal(&e.context("outer").context("outermost"));
        let k = |e: anyhow::Error| {
            let r = r(e);
            (r.status, r.code)
        };
        assert_eq!(k(OfflineError::KitExpired("k".into()).into()), (403, "offline_kit_expired"));
        assert_eq!(k(OfflineError::NoteInvalid("n".into()).into()), (403, "offline_note_invalid"));
        let pool =
            OfflineError::PoolExhausted { detail: "p".into(), price_msat: 50_000, below_msat: Some(49_000), above_msat: Some(52_000) };
        let p = r(pool.into());
        assert_eq!(
            (p.status, p.code, p.suggested_msat, p.below_msat, p.above_msat),
            (409, "offline_pool_exhausted", Some(49_000), Some(49_000), Some(52_000))
        );
        assert_eq!(k(OfflineError::Cap("c".into()).into()), (503, "offline_cap_reached"));
        assert_eq!(k(OfflineError::Undecided("u".into()).into()), (202, "undecided"));
        assert_eq!(k(BridgeError::NoCard("gone".into()).into()), (409, "no_card"));
        assert_eq!(k(PlanError::Insufficient { have: 1, need: 2 }.into()), (402, "insufficient"));
        assert_eq!(k(StatusWord(SW_OVER_LIMIT).into()), (402, "card_limit"));
        assert_eq!(k(anyhow::anyhow!("else")), (500, "internal"));
        let pin = r(StatusWord(0x63C3).into());
        assert_eq!(
            (pin.status, pin.code, pin.wrong_tries, pin.wait_s, pin.detail.as_str()),
            (403, "wrong_pin", Some(3), Some(3), "wrong PIN; 3 wrong in a row, so the next try first waits about 3 s")
        );
        let capped = r(StatusWord(0x63CF).into());
        assert_eq!((capped.wrong_tries, capped.wait_s), (Some(15), Some(12_288)), "0x4000 askings at 0.75 s");
        // 6983 is a 2.x card's word; a 4.0 card never answers it, so it is no PIN code any more.
        assert_eq!(k(StatusWord(0x6983).into()), (502, "card_error"));
    }

    #[test]
    fn a_wait_the_card_was_taken_from_is_pin_wait_with_the_seconds_left() {
        let w = refusal(&anyhow::Error::new(PinWait { askings: 33 }).context("verifying the holder's PIN"));
        assert_eq!((w.status, w.code, w.wait_s, w.wrong_tries), (423, "pin_wait", Some(25), None));
        assert!(w.detail.starts_with("hold the card"), "{}", w.detail);
        let raw = refusal(&anyhow::Error::from(StatusWord(SW_KEEP_HOLDING)));
        assert_eq!((raw.status, raw.code, raw.wait_s), (423, "pin_wait", Some(0)));
        assert_eq!((wait_seconds(0), wait_seconds(1), wait_seconds(4), wait_seconds(5)), (0, 1, 3, 4));
        assert_eq!((next_wait(0), next_wait(1), next_wait(2), next_wait(7), next_wait(15)), (0, 1, 2, 64, 0x4000));
    }

    #[test]
    fn an_old_card_and_a_failed_signature_check_say_what_to_do() {
        let old = refusal(&anyhow::Error::new(UnsupportedApplet::Old { answer: vec![3, 0] }).context("the applet's version"));
        assert_eq!((old.status, old.code, old.detail.as_str()), (409, "old_card", OLD_CARD_MESSAGE));
        let sw = || anyhow::Error::from(StatusWord(0x6F00));
        // The card library's typed 6F00 (built as `CardClient::sign` builds it), behind a context.
        let failed = refusal(&sw().context(fedimint_card_host_card::SignCheckFailed).context("card signature, slot 3"));
        assert_eq!((failed.status, failed.code), (409, "sign_check_failed"));
        assert!(failed.detail.contains("nothing was spent, tap again"), "{}", failed.detail);
        // 6F00 to any other command is only a status word.
        assert_eq!((refusal(&sw()).status, refusal(&sw()).code), (502, "card_error"));
    }
}
