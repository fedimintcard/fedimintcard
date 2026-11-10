//! One vocabulary for why a charge did not happen (docs/TILL-API.md): the till's API codes and
//! HTTP statuses, which a phone's offline charge answers too. The till's `classify` asks
//! [`class_of`] first for every cause in an error's chain, then looks at its own errors (the
//! registry's, the federation's).

use fedimint_card_host_card::v3::SW_OVER_LIMIT;
use fedimint_card_host_card::{BridgeError, SW_PIN_BLOCKED, StatusWord};

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
    /// `wrong_pin`: tries left before the card blocks (`63 CX`).
    pub tries_left: Option<u8>,
}

/// The HTTP status and code of one cause of an error, when it is one the offline charge raises:
/// [`OfflineError`], the card's status word (`wrong_pin`, `pin_blocked`, `card_limit`, else
/// `card_error`), the card's transport ([`BridgeError`]: `no_card` when the card left), and the
/// planner's [`PlanError`].
pub fn class_of(cause: &(dyn std::error::Error + 'static)) -> Option<(u16, &'static str)> {
    if let Some(o) = cause.downcast_ref::<OfflineError>() {
        return Some(o.class());
    }
    if let Some(StatusWord(sw)) = cause.downcast_ref::<StatusWord>() {
        return Some(match *sw {
            0x63C0..=0x63CF => (403, "wrong_pin"),
            SW_PIN_BLOCKED => (423, "pin_blocked"),
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
            PlanError::NoExactPlan { .. } | PlanError::TooManyNotes(_) | PlanError::TooManyInputs { .. } | PlanError::NoFreeSlots { .. } => {
                (422, "no_exact_plan")
            }
        });
    }
    None
}

/// `err` as the refusal an app shows: the first cause [`class_of`] knows gives the code (else
/// `internal`, 500), the detail says it plainly for the holder.
pub fn refusal(err: &anyhow::Error) -> Refusal {
    let (status, code) = err.chain().find_map(class_of).unwrap_or((500, "internal"));
    let sw = err.chain().find_map(|c| c.downcast_ref::<StatusWord>()).map(|s| s.0);
    let tries_left = (code == "wrong_pin").then(|| sw.map_or(0, |s| (s & 0x0F) as u8));
    let detail = match code {
        // Said plainly, for the holder: the tries left are the status word's low nibble (`63 CX`).
        "wrong_pin" => format!("wrong PIN; {} tries left before the card blocks", tries_left.unwrap_or(0)),
        "pin_blocked" => "the card's PIN is blocked after too many wrong tries; the card cannot pay".into(),
        _ => format!("{err:#}"),
    };
    let prices = err.chain().find_map(|c| c.downcast_ref::<OfflineError>()).and_then(OfflineError::suggestions);
    let (suggested_msat, below_msat, above_msat) = prices.unwrap_or_default();
    Refusal { status, code, detail, suggested_msat, below_msat, above_msat, tries_left }
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
        let pool = OfflineError::PoolExhausted { detail: "p".into(), price_msat: 50_000, below_msat: Some(49_000), above_msat: Some(52_000) };
        let p = r(pool.into());
        assert_eq!((p.status, p.code, p.suggested_msat, p.below_msat, p.above_msat), (409, "offline_pool_exhausted", Some(49_000), Some(49_000), Some(52_000)));
        assert_eq!(k(OfflineError::Cap("c".into()).into()), (503, "offline_cap_reached"));
        assert_eq!(k(OfflineError::Undecided("u".into()).into()), (202, "undecided"));
        assert_eq!(k(BridgeError::NoCard("gone".into()).into()), (409, "no_card"));
        assert_eq!(k(PlanError::Insufficient { have: 1, need: 2 }.into()), (402, "insufficient"));
        assert_eq!(k(StatusWord(SW_OVER_LIMIT).into()), (402, "card_limit"));
        assert_eq!(k(anyhow::anyhow!("else")), (500, "internal"));
        let pin = r(StatusWord(0x63C2).into());
        assert_eq!((pin.code, pin.tries_left, pin.detail.as_str()), ("wrong_pin", Some(2), "wrong PIN; 2 tries left before the card blocks"));
        let blocked = r(StatusWord(SW_PIN_BLOCKED).into());
        assert_eq!((blocked.status, blocked.code, blocked.tries_left), (423, "pin_blocked", None));
    }
}
