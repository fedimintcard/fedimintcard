import Foundation

/// The one copy for a till this phone cannot reach, on the sale and the balance screens alike.
let tillUnreachableCopy = ("Till service unreachable", "Check that the till is running and this phone is on its network.")

/// Title and detail for a failed balance read, in words the card holder can act on.
func balanceErrorCopy(_ p: BalanceProblem) -> (String, String) {
    switch p {
    case .cardMoved: return ("The card moved away", "Hold the card still against the top of the phone until the balance shows.")
    case .notOurCard: return ("Not a card for this till", "This card isn't set up here. Try another card.")
    case .tillUnreachable: return tillUnreachableCopy
    case .relayUnreachable: return ("The till lost the card", "Hold the card to the phone and try again.")
    case .busy: return ("The till is busy", "Try again in a moment.")
    case .cardService: return ("Card service unreachable", "Check the network, then try again.")
    case .timeout: return ("The till took too long", "Hold the card to the phone and try again.")
    case .notPaired: return ("This phone isn't paired", "Pair with the till in Settings, then try again.")
    case .other: return ("Couldn't read the card", "Hold the card to the phone and try again.")
    }
}

/// Title and detail copy for a charge error. `suggestedMsat` is the price the till (or the phone's
/// kit) says the card can pay instead, for `offline_no_change_room` and `offline_pool_exhausted`.
/// `unresolved`: an earlier attempt at this sale has no known outcome, so a till out of reach is
/// a lost reply, and the phone does not take the sale itself.
func errorCopy(_ kind: ErrorKind, _ detail: String, suggestedMsat: Int64? = nil, unresolved: Bool = false) -> (String, String) {
    let unreachable = tillUnreachableCopy.0
    switch kind {
    case .noCard: return ("Card not detected", "Hold the card to the top of the phone and keep it there")
    case .insufficient: return ("Card balance too low", "")
    case .noExactPlan: return ("Card needs a top up", "")
    // Not a full card: offline, change needs a fresh slot the till cannot key without the registry.
    case .offlineNoChangeRoom:
        let tip = suggestedMsat.map { "Try \(formatSats($0)) sats" } ?? "Try another amount"
        return ("No room for change offline", "This card can't take change while offline. " + tip)
    case .declined: return ("Card declined", detail)
    case .relayUnreachable: return ("The till lost the card", "Hold the card to the phone and try again")
    case .cardLinkGone: return ("The card moved away", "Hold the card to the phone and try again")
    case .busy: return ("Till is busy", "Try again in a moment")
    case .recovering: return ("Till is recovering", "Try again in a few seconds")
    case .cardError: return ("Card error", detail)
    case .undecided: return ("Payment pending", "Waiting for the federation. This screen updates when the sale is recorded; Try again re-checks without charging twice.")
    case .timeoutUnknown: return ("Outcome unknown", "The till did not answer. The card may have been charged. This screen updates when the sale is recorded; Try again re-checks without charging twice.")
    case .tillUnreachable where unresolved:
        return (unreachable, "The last reply was lost, so this sale can't be taken on the phone. Try again once the till answers.")
    case .tillUnreachable: return tillUnreachableCopy
    // Why the phone did not take the card itself (docs/POS-OFFLINE.md).
    case .offlineNoKit: return (unreachable, "No offline kit yet. Reach the till once while online.")
    case .offlineKitExpired: return (unreachable, "The offline kit has expired. Reach the till to renew it.")
    case .offlineCapReached: return (unreachable, "Offline limit reached. Reach the till so waiting payments can sync.")
    case .offlineNoteInvalid: return (unreachable, "This card can't be checked without the till. It pays online only.")
    case .offlinePoolExhausted:
        let tip = suggestedMsat.map { "Try \(formatSats($0)) sats" } ?? "Try another amount"
        return ("Amount not available offline", tip + ", or reach the till for a new kit.")
    case .heldOnPhone:
        return ("Payment not finished", "The card was taken away while it signed. Hold the same card to the phone and press Try again. It won't be charged twice.")
    case .offlineUnsupportedCard: return ("This card pays online only", "It can't be taken without the till.")
    case .cardNotGenuine: return ("Card declined", "This isn't a genuine Fedimint Card.")
    case .cardLimit: return ("Card limit reached", "This card has reached its daily limit.")
    case .unauthorized: return ("The till refused this phone", "Pair with the till again in Settings")
    case .notPaired: return ("No till paired", "Open Settings and pair with the till")
    case .wrongPin: return (wrongPinCopy(parseTriesLeft(detail)), "Ask the card holder to enter it again")
    case .pinBlocked: return ("PIN blocked", "This card's PIN is blocked. The card can't pay.")
    case .pinRequired: return ("Something went wrong", "The app sent the charge without the holder's PIN. This is an app bug; the card was not touched.")
    case .conflict: return ("Something went wrong", detail)
    case .cardChanged: return ("Card changed", "The card was used elsewhere. Hold it to the phone and try again.")
    case .unknownCard: return ("Card not recognised", "This card is not set up for this till")
    case .opsUnreachable: return ("Card service unreachable", "Check the network, then try again")
    case .opsRefused: return ("Card service refused the till", "Check the till token")
    case .badRequest, .internal, .unknown: return ("Something went wrong", detail)
    }
}

/// The Abandon alert's message: a sale held on this phone is still finished by its card's next
/// tap here (the owner's decision, ENG-843); any other unresolved sale may have gone through.
func abandonCopy(_ kind: ErrorKind) -> String {
    kind == .heldOnPhone
        ? "The card already signed part of this payment. Its next tap on this phone still finishes it."
        : "If the payment did go through, the customer will have paid without a receipt. Check Sales first."
}

/// The line over the PIN pad after a refused PIN. Never includes the PIN.
func wrongPinCopy(_ triesLeft: Int?) -> String {
    switch triesLeft {
    case nil: return "Wrong PIN"
    case 1: return "Wrong PIN — 1 try left"
    case let n?: return "Wrong PIN — \(n) tries left"
    }
}
