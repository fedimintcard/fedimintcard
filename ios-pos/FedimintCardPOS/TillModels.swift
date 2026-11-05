import Foundation

struct TillStatus: Equatable {
    let federationId: String
    let federationName: String?
    let walletBalanceMsat: Int64
    let cardRelayOk: Bool
    let busy: Bool
}

struct Note: Equatable { let slot: Int; let msat: Int64 }
struct CardNote: Equatable { let slot: Int; let msat: Int64; let status: String }

struct CardBalance: Equatable {
    let liveMsat: Int64
    let notes: [CardNote]
    let freeSlots: [Int]
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
    case noExactPlan = "no_exact_plan"
    case declined
    case undecided
    case recovering
    case conflict
    case pinRequired = "pin_required"
    case wrongPin = "wrong_pin"
    case pinBlocked = "pin_blocked"
    case cardChanged = "card_changed"
    case unknownCard = "unknown_card"
    case opsUnreachable = "ops_unreachable"
    case opsRefused = "ops_refused"
    case cardLinkGone = "card_link_gone"
    case unauthorized
    case `internal`
    case unknown = ""

    static func fromWire(_ s: String?) -> ChargeErrorCode {
        guard let s, !s.isEmpty else { return .unknown }
        return ChargeErrorCode(rawValue: s) ?? .unknown
    }
}

/// Tries left from a `wrong_pin` detail ("wrong PIN; 2 tries left before the card blocks"), or nil.
func parseTriesLeft(_ detail: String) -> Int? {
    guard let re = try? NSRegularExpression(pattern: #"(\d+)\s+tr(?:y|ies)\s+left"#, options: .caseInsensitive),
          let m = re.firstMatch(in: detail, range: NSRange(detail.startIndex..., in: detail)),
          let r = Range(m.range(at: 1), in: detail) else { return nil }
    return Int(detail[r])
}
