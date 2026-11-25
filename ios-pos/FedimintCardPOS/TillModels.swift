import Foundation

struct TillStatus: Equatable {
    let federationId: String
    let federationName: String?
    let walletBalanceMsat: Int64
    let cardRelayOk: Bool
    let busy: Bool
    /// Offline charges queued on the till and not yet sent (`offline.queued`); 0 on a till without the field.
    var offlineQueued: Int64 = 0
    var offlineQueuedMsat: Int64 = 0
    /// Offline charges the registry refused as double spends (`offline.double_spent`); nil when the
    /// till's status has no such field, so Settings shows the count only for a till that reports it.
    var offlineDoubleSpent: Int64? = nil
}

struct Note: Equatable { let slot: Int; let msat: Int64 }
struct CardNote: Equatable { let slot: Int; let msat: Int64; let status: String }

/// What the card holds, read off the card itself. `liveMsat` is what the card can pay.
struct CardBalance: Equatable {
    let liveMsat: Int64
    let notes: [CardNote]
    let freeSlots: [Int]
    /// The card's id on the registry (`short_id`), when the till is on ops: a top-up started from
    /// the balance screen sends it so the till fits the amount to this card's free slots.
    var shortId: String? = nil
    /// Notes the till holds back as stranded (`stranded_msat`, docs/TILL-API.md): on the card, not
    /// in `liveMsat`, held for a sale that was never settled. Most are freed by the registry's
    /// release after the grace; a double spend's still-live inputs stay held. 0 from a till from
    /// before these fields.
    var strandedMsat: Int64 = 0
    /// When the last of them may be released, by the registry's clock (`stranded_until_ms`); nil
    /// when there is no date to wait for, or the till does not say.
    var strandedUntilMs: Int64? = nil
}

struct ChargeReceipt: Equatable {
    let saleId: Int64
    let txid: String
    let priceMsat: Int64
    let receivedMsat: Int64
    let overpaidMsat: Int64
    let feesMsat: Int64
    let inputs: [Note]
    let change: [Note]
    let collectionPending: Bool
    let warnings: [String]
    let durationMs: Int64
    /// The till took this charge with no registry or federation reachable (`"offline": true`).
    var offline: Bool = false
}

struct Sale: Equatable, Identifiable {
    let id: Int64
    let tsUnixMs: Int64
    let priceMsat: Int64
    let receivedMsat: Int64
    let overpaidMsat: Int64
    let feesMsat: Int64
    let txid: String
    let clientRef: String?
}

struct SalesList: Equatable {
    let count: Int64
    let totalPriceMsat: Int64
    let totalReceivedMsat: Int64
    let sales: [Sale]
}

/// Error codes the till answers with (docs/TILL-API.md, docs/POS-LINK.md).
enum ChargeErrorCode: String, CaseIterable {
    case badRequest = "bad_request"
    case busy
    case noCard = "no_card"
    case relayUnreachable = "relay_unreachable"
    case cardError = "card_error"
    case insufficient
    /// The live notes do not pay; with the notes held for a sale the registry refused they would. Nothing ran.
    case notesHeld = "notes_held"
    case noExactPlan = "no_exact_plan"
    case declined
    case undecided
    case recovering
    case conflict
    case pinRequired = "pin_required"
    case wrongPin = "wrong_pin"
    /// The card is serving its PIN wait; tap again to finish it (`wait_s`).
    case pinWait = "pin_wait"
    /// An applet before 4.0: needs a reset and a new set-up.
    case oldCard = "old_card"
    /// The card refused the signature; nothing was spent.
    case signCheckFailed = "sign_check_failed"
    case cardChanged = "card_changed"
    case unknownCard = "unknown_card"
    case opsUnreachable = "ops_unreachable"
    case opsRefused = "ops_refused"
    case wrongFederation = "wrong_federation"
    /// The till is on a registry and has no token yet; nothing ran (docs/TILL-API.md).
    case opsNotConnected = "ops_not_connected"
    /// Offline, the price needs change and the card has no slot for it; `suggested_msat` may say what it can pay.
    case offlineNoChangeRoom = "offline_no_change_room"
    case cardLinkGone = "card_link_gone"
    case unauthorized
    /// A token with the pay scope; nothing ran (docs/TILL-API.md).
    case forbidden
    case `internal`
    // The offline charge's codes (docs/POS-OFFLINE.md § Refusals): the phone's own, and the till's
    // offline path's.
    /// A phone's offline kit is past its expiry; nothing was asked of the card.
    case offlineKitExpired = "offline_kit_expired"
    /// The kit has run out of a tier the price needs; `suggested_msat` is a price it pays.
    case offlinePoolExhausted = "offline_pool_exhausted"
    /// The card's notes do not check offline and the rest do not pay; nothing was signed.
    case offlineNoteInvalid = "offline_note_invalid"
    /// The caps on what waits to sync.
    case offlineCapReached = "offline_cap_reached"
    /// No kit (a phone), or no certificate key yet (a till).
    case offlineNotReady = "offline_not_ready"
    /// An applet 2.x card, taken only online.
    case offlineUnsupportedCard = "offline_unsupported_card"
    /// Not a card the registry certified (a clone).
    case cardNotGenuine = "card_not_genuine"
    /// The card's daily limit.
    case cardLimit = "card_limit"
    case unknown = ""

    static func fromWire(_ s: String?, detail: String? = nil) -> ChargeErrorCode {
        // The Card 4.0 till :11 wraps this refusal in its registry error.
        if s == "ops_refused", detail == "ops refused (409): the card belongs to a different federation or mint" {
            return .wrongFederation
        }
        guard let s, !s.isEmpty else { return .unknown }
        return ChargeErrorCode(rawValue: s) ?? .unknown
    }
}
