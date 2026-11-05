import Foundation

/// A top-up the till made (`POST /v1/topup`, docs/TOPUP.md): the invoice is for `amountMsat`.
struct TopUp: Equatable {
    let topupId: String
    let amountMsat: Int64
    let invoice: String
    let expiresAtMs: Int64
    let state: String
}

/// A paid top-up the till holds that is not on a card yet (`GET /v1/topups/waiting`, docs/TOPUP.md
/// § Waiting top-ups): the card was lifted, the app closed, or the card was full.
struct WaitingTopUp: Equatable {
    let topupId: String
    let amountMsat: Int64
    let forCard: String?
    let createdAtMs: Int64

    /// The top-up the load path takes: paid, no invoice to show.
    var asTopUp: TopUp { TopUp(topupId: topupId, amountMsat: amountMsat, invoice: "", expiresAtMs: 0, state: "paid") }
}

/// `GET /v1/topup/{id}` and the reply to `POST /v1/topup/{id}/load`.
struct TopUpStatus: Equatable {
    let topupId: String
    let amountMsat: Int64
    let state: String
    let loadTxid: String?
    let detail: String?
}

/// A till reply to a top-up route.
enum TopUpReply<T: Equatable>: Equatable {
    case ok(T)
    /// `{"ok":false,"error":…}`: `error` is the wire code (`card_full`, `expired`, …).
    case refused(error: String, detail: String, httpStatus: Int)
    case failed(TillError)

    var refusedDetail: String { if case .refused(_, let d, _) = self { return d }; return "" }
}

/// Why a top-up step failed, in the terms the screen needs.
enum TopUpProblem: Equatable {
    /// Unpaid at expiry: nothing moved.
    case expired
    /// The card has no room for the notes; the top-up stays paid.
    case cardFull
    /// The card has no free slot, found before any invoice was made: nothing was paid.
    case noRoom
    /// Paid, and the card step was cancelled: the top-up waits in the till until a tap loads it.
    case notLoaded
    case cardMoved, notOurCard, tillUnreachable, timeout, cardService, badAmount, busy, notPaired
    /// The till gave up on this top-up (`state: failed`); any eCash received stays in the till.
    case failed
    case other
}

func topUpProblem<T>(_ r: TopUpReply<T>) -> TopUpProblem {
    switch r {
    case .ok: return .other
    case .failed(let e):
        switch e {
        case .unreachable: return .tillUnreachable
        case .timeout, .interrupted: return .timeout
        case .notPaired, .unauthorized: return .notPaired
        case .badResponse: return .other
        }
    case .refused(let code, let detail, _):
        switch code {
        case "expired": return .expired
        case "card_full": return .cardFull
        case "no_card", "card_link_gone", "card_changed", "card_lost": return .cardMoved
        case "card_error":
            return detail.range(of: "different card|6a82|6e00|6d00", options: [.regularExpression, .caseInsensitive]) != nil
                ? .notOurCard : .cardMoved
        case "unknown_card": return .notOurCard
        case "ops_not_connected", "ops_unreachable", "ops_refused": return .cardService
        case "bad_amount": return .badAmount
        case "busy", "recovering": return .busy
        case "relay_unreachable": return .tillUnreachable
        case "unauthorized": return .notPaired
        default: return .other
        }
    }
}

/// Title and detail for a failed top-up step. `paid`: the holder has paid and the top-up waits for the card.
func topUpErrorCopy(_ p: TopUpProblem, paid: Bool, detail: String) -> (String, String) {
    let waiting = paid ? " The top-up is paid and waiting." : ""
    switch p {
    case .expired: return ("The invoice expired", "Nothing was paid. Make a new invoice to try again.")
    case .notLoaded: return ("Not loaded yet", "The top-up is paid and waiting. Tap the card to load it; Done leaves the payment with the till.")
    case .cardFull: return ("This card is full", "Spend from it, then tap again.\(waiting)")
    case .noRoom: return ("This card is full", "Spend from it first, then top up.")
    case .cardMoved: return ("The card moved away", "Hold the card still until it's loaded.\(waiting)")
    case .notOurCard: return ("Not a card for this till", "Try the holder's card.\(waiting)")
    case .tillUnreachable: return ("Can't reach the till", "Check that the till is running, then try again.\(waiting)")
    case .timeout: return ("The till took too long", "Try again: the same top-up is never loaded twice.\(waiting)")
    case .cardService: return ("Card service unreachable", "Check the network, then try again.\(waiting)")
    case .badAmount: return ("That amount can't go on a card", detail.isEmpty ? "Try a different amount." : detail)
    case .busy: return ("The till is busy", "Try again in a moment.\(waiting)")
    case .notPaired: return ("Till refused this phone", "Pair with the till again in Settings.")
    case .failed: return ("The top-up failed", (detail.isEmpty ? "" : detail + " ") + "Any payment received stays with the till.")
    case .other: return ("Something went wrong", (detail + waiting).trimmingCharacters(in: .whitespaces))
    }
}

/// `m:ss` left until `expiresAtMs`; 0:00 once past.
func timeLeft(expiresAtMs: Int64, nowMs: Int64) -> String {
    let s = (max(0, expiresAtMs - nowMs) + 999) / 1000
    return "\(s / 60):" + String(format: "%02d", s % 60)
}

/// What a wallet scans: `LIGHTNING:` and the invoice, upper case (the QR's alphanumeric mode).
func invoiceQrText(_ invoice: String) -> String { "LIGHTNING:" + invoice.uppercased() }
