import Foundation

enum ErrorKind: Equatable {
    case badRequest, busy, noCard, relayUnreachable, cardError, insufficient, noExactPlan
    case declined, undecided, recovering, conflict, pinRequired, wrongPin, pinBlocked, cardChanged
    case unknownCard, opsUnreachable, opsRefused, cardLinkGone, unauthorized, `internal`, unknown
    /// The phone could not reach the till (nothing was sent).
    case tillUnreachable
    /// The till did not answer in time: the outcome may already have happened.
    case timeoutUnknown
    /// No till is paired on this phone.
    case notPaired

    static func from(_ c: ChargeErrorCode) -> ErrorKind {
        switch c {
        case .badRequest: return .badRequest
        case .busy: return .busy
        case .noCard: return .noCard
        case .relayUnreachable: return .relayUnreachable
        case .cardError: return .cardError
        case .insufficient: return .insufficient
        case .noExactPlan: return .noExactPlan
        case .declined: return .declined
        case .undecided: return .undecided
        case .recovering: return .recovering
        case .conflict: return .conflict
        case .pinRequired: return .pinRequired
        case .wrongPin: return .wrongPin
        case .pinBlocked: return .pinBlocked
        case .cardChanged: return .cardChanged
        case .unknownCard: return .unknownCard
        case .opsUnreachable: return .opsUnreachable
        case .opsRefused: return .opsRefused
        case .cardLinkGone: return .cardLinkGone
        case .unauthorized: return .unauthorized
        case .internal: return .internal
        case .unknown: return .unknown
        }
    }

    static func from(_ e: TillError) -> ErrorKind {
        switch e {
        case .notPaired: return .notPaired
        case .unreachable: return .tillUnreachable
        case .timeout, .interrupted: return .timeoutUnknown
        case .unauthorized: return .unauthorized
        case .badResponse: return .internal
        }
    }

    /// Outcome may already have happened: starts an unresolved sale.
    var needsSalesCheck: Bool { self == .undecided || self == .timeoutUnknown }

    /// Nothing moved: clears an unresolved sale. A PIN refusal comes from inside the spend, after the
    /// till has settled any earlier attempt with the same ref. `pin_required`, the ops codes, and the
    /// link/auth refusals are answered before that, so they prove nothing about an earlier attempt.
    var definitive: Bool {
        [.insufficient, .noExactPlan, .declined, .badRequest, .noCard, .cardError, .wrongPin, .pinBlocked].contains(self)
    }

    /// Whether trying the same sale again can help.
    var retryable: Bool { ![.pinBlocked, .notPaired, .unauthorized].contains(self) }
}

struct PosErrorInfo: Equatable {
    let priceMsat: Int64
    let kind: ErrorKind
    let detail: String
    let clientRef: String
    let unresolvedRef: String?
}

/// Why a balance read failed, in the terms the holder's screen needs.
enum BalanceProblem: Equatable { case cardMoved, notOurCard, tillUnreachable, relayUnreachable, busy, cardService, timeout, notPaired, other }

func balanceProblem(_ r: BalanceResult) -> BalanceProblem {
    switch r {
    case .read: return .other
    case .failed(let e):
        switch e {
        case .unreachable: return .tillUnreachable
        case .timeout, .interrupted: return .timeout
        case .unauthorized, .notPaired: return .notPaired
        case .badResponse: return .other
        }
    case .refused(let code, let detail):
        switch code {
        case .noCard, .cardChanged, .cardLinkGone: return .cardMoved
        case .unknownCard: return .notOurCard
        // A key that does not match, or an applet that is not there, is a card this till does not
        // know; anything else from the card mid-read is the card leaving the field.
        case .cardError:
            return detail.range(of: "different card|6a82|6e00|6d00", options: [.regularExpression, .caseInsensitive]) != nil
                ? .notOurCard : .cardMoved
        case .relayUnreachable: return .relayUnreachable
        case .busy, .recovering: return .busy
        case .opsUnreachable, .opsRefused: return .cardService
        case .unauthorized: return .notPaired
        default: return .other
        }
    }
}

enum CardPurpose: Equatable { case charge, balance }

enum PosState: Equatable {
    case entering
    /// The holder types the PIN. The digits are in `PosFlow.pinPad`, never here. `returnTo` is the
    /// unresolved error that asked for the PIN (cancel goes back to it); nil goes back to the amount.
    case enteringPin(priceMsat: Int64, clientRef: String, wrongPin: Bool, triesLeft: Int?, returnTo: PosErrorInfo?)
    /// The NFC sheet is up, waiting for the card. The confirmed PIN is held by the flow.
    case waitingForCard(priceMsat: Int64, clientRef: String, returnTo: PosErrorInfo?)
    case signing(priceMsat: Int64, chargeId: Int, clientRef: String, unresolvedRef: String?)
    case receipt(ChargeReceipt)
    case error(PosErrorInfo)
    case sales(returnTo: PosErrorInfo?)
    case balanceWaiting
    case balanceReading(readId: Int)
    case balanceShown(readId: Int, liveMsat: Int64)
    /// `detail` is the till's own words, kept for diagnosis and never shown.
    case balanceFailed(BalanceProblem, detail: String)

    var isBalance: Bool {
        switch self {
        case .balanceWaiting, .balanceReading, .balanceShown, .balanceFailed: return true
        default: return false
        }
    }
}

enum PosEvent: Equatable {
    case chargePressed
    case pinConfirmed
    /// The app left the foreground: any PIN typed or held is dropped.
    case backgrounded
    case cancelPressed
    /// A card is connected in the NFC session and its link to the till is open.
    case cardPresent
    /// The NFC session ended (cancelled, timed out) before a card was linked.
    case cardSessionEnded
    /// The card link to the till could not be opened; nothing was sent.
    case linkFailed(TillError)
    case chargeDone(chargeId: Int, result: ChargeResult)
    case retryPressed
    case newSalePressed
    case backPressed
    case abandonPressed
    case salesOpened
    case cardBalanceOpened
    case balanceDone(readId: Int, result: BalanceResult)
    case balanceExpired(readId: Int)
    case donePressed
    case saleFound(Sale)
}

enum PosEffect {
    case startCardSession(CardPurpose)
    case endCardSession
    /// `pin` is handed over to the request, which wipes it once the body is built.
    case startCharge(chargeId: Int, priceMsat: Int64, clientRef: String, pin: Pin?)
    case startBalanceRead(readId: Int)
}

/// Pure state machine for one till session. Call from the main thread.
final class PosFlow {
    /// How long the holder sees their balance before the amount comes back.
    static let balanceShownSeconds = 15

    let pad = AmountPad()
    let pinPad = PinPad()
    private var heldPin: Pin?
    private(set) var state: PosState = .entering
    private var nextId = 0
    private let newRef: () -> String

    init(newRef: @escaping () -> String = { UUID().uuidString.lowercased() }) { self.newRef = newRef }

    /// The client_ref of a sale whose outcome is unknown, while we are waiting to learn it.
    var unresolvedRef: String? {
        switch state {
        case .error(let e): return e.unresolvedRef
        case .signing(_, _, _, let u): return u
        case .sales(let r): return r?.unresolvedRef
        case .enteringPin(_, _, _, _, let r): return r?.unresolvedRef
        case .waitingForCard(_, _, let r): return r?.unresolvedRef
        default: return nil
        }
    }

    /// True while any PIN digits are typed on the pad or held for the card.
    var holdsPin: Bool { heldPin != nil || pinPad.length > 0 }

    private func dropPin() {
        pinPad.clear()
        heldPin?.wipe()
        heldPin = nil
    }

    private func askPin(_ price: Int64, _ ref: String, wrongPin: Bool = false, triesLeft: Int? = nil, returnTo: PosErrorInfo? = nil) {
        dropPin()
        state = .enteringPin(priceMsat: price, clientRef: ref, wrongPin: wrongPin, triesLeft: triesLeft, returnTo: returnTo)
    }

    private func id() -> Int { nextId += 1; return nextId }

    private func back(to r: PosErrorInfo?) -> PosState { r.map { .error($0) } ?? .entering }

    private func errorFrom(price: Int64, ref: String, prevUnresolved: String?, kind: ErrorKind, detail: String) -> PosState {
        let unresolved: String?
        if kind.needsSalesCheck { unresolved = ref } else if kind.definitive { unresolved = nil } else { unresolved = prevUnresolved }
        return .error(PosErrorInfo(priceMsat: price, kind: kind, detail: detail, clientRef: ref, unresolvedRef: unresolved))
    }

    private func balance() -> [PosEffect] {
        state = .balanceWaiting
        return [.startCardSession(.balance)]
    }

    @discardableResult
    func dispatch(_ e: PosEvent) -> [PosEffect] {
        let s = state
        switch (e, s) {
        case (.chargePressed, .entering):
            guard pad.isChargeable else { return [] }
            askPin(pad.msat, newRef())

        case (.pinConfirmed, .enteringPin(let price, let ref, _, _, let returnTo)):
            guard let pin = pinPad.take() else { return [] }
            heldPin?.wipe()
            heldPin = pin
            state = .waitingForCard(priceMsat: price, clientRef: ref, returnTo: returnTo)
            return [.startCardSession(.charge)]

        case (.cancelPressed, .enteringPin(_, _, _, _, let returnTo)):
            dropPin(); state = back(to: returnTo)

        case (.cardPresent, .waitingForCard(let price, let ref, let returnTo)):
            let cid = id()
            state = .signing(priceMsat: price, chargeId: cid, clientRef: ref, unresolvedRef: returnTo?.unresolvedRef)
            let pin = heldPin
            heldPin = nil // the request owns it now, and wipes it
            return [.startCharge(chargeId: cid, priceMsat: price, clientRef: ref, pin: pin)]

        case (.cardPresent, .balanceWaiting):
            let rid = id()
            state = .balanceReading(readId: rid)
            return [.startBalanceRead(readId: rid)]

        case (.cancelPressed, .waitingForCard(_, _, let returnTo)):
            dropPin(); state = back(to: returnTo); return [.endCardSession]
        case (.cardSessionEnded, .waitingForCard(_, _, let returnTo)):
            dropPin(); state = back(to: returnTo)

        case (.linkFailed(let te), .waitingForCard(let price, let ref, let returnTo)):
            // Nothing reached the till's charge route: an unresolved sale stays unresolved.
            dropPin()
            let kind = ErrorKind.from(te) == .timeoutUnknown ? .tillUnreachable : ErrorKind.from(te)
            state = .error(PosErrorInfo(priceMsat: price, kind: kind, detail: "", clientRef: ref, unresolvedRef: returnTo?.unresolvedRef))
            return [.endCardSession]
        case (.linkFailed(let te), .balanceWaiting):
            state = .balanceFailed(balanceProblem(.failed(te == .timeout ? .unreachable : te)), detail: "")
            return [.endCardSession]

        case (.backgrounded, _):
            dropPin()
            switch s {
            case .enteringPin(_, _, _, _, let r): state = back(to: r)
            case .waitingForCard(_, _, let r): state = back(to: r); return [.endCardSession]
            case .balanceWaiting, .balanceReading: state = .entering; return [.endCardSession]
            case .balanceShown, .balanceFailed: state = .entering
            default: break
            }

        case (.cardBalanceOpened, .entering):
            return balance()
        case (.cancelPressed, .balanceWaiting), (.cancelPressed, .balanceReading):
            state = .entering; return [.endCardSession]
        case (.cardSessionEnded, .balanceWaiting):
            state = .entering
        case (.balanceDone(let rid, let r), .balanceReading(let cur)) where rid == cur:
            switch r {
            case .read(let b): state = .balanceShown(readId: cur, liveMsat: b.liveMsat)
            case .refused(_, let d): state = .balanceFailed(balanceProblem(r), detail: d)
            case .failed(let te):
                if case .badResponse(let d) = te { state = .balanceFailed(balanceProblem(r), detail: d) }
                else { state = .balanceFailed(balanceProblem(r), detail: "") }
            }
        case (.balanceExpired(let rid), .balanceShown(let cur, _)) where rid == cur:
            state = .entering
        case (.donePressed, .balanceShown), (.donePressed, .balanceFailed), (.backPressed, .balanceFailed):
            state = .entering
        case (.retryPressed, .balanceFailed):
            return balance()

        case (.chargeDone(let cid, let r), .signing(let price, let cur, let ref, let unresolved)) where cid == cur:
            dropPin()
            switch r {
            case .success(let receipt):
                state = .receipt(receipt)
            case .declined(let code, let detail, _) where code == .wrongPin:
                // Nothing moved; the same ref is a fresh attempt (TILL-API, wrong_pin).
                let left = parseTriesLeft(detail)
                if left == 0 {
                    state = .error(PosErrorInfo(priceMsat: price, kind: .pinBlocked, detail: "", clientRef: ref, unresolvedRef: nil))
                } else {
                    askPin(price, ref, wrongPin: true, triesLeft: left)
                }
            case .declined(let code, let detail, _):
                state = errorFrom(price: price, ref: ref, prevUnresolved: unresolved, kind: .from(code), detail: detail)
            case .failed(let te):
                var detail = ""
                if case .badResponse(let d) = te { detail = d }
                state = errorFrom(price: price, ref: ref, prevUnresolved: unresolved, kind: .from(te), detail: detail)
            }

        // Every attempt carries the holder's PIN, and the last one was wiped once sent.
        case (.retryPressed, .error(let info)):
            if info.unresolvedRef != nil { askPin(info.priceMsat, info.clientRef, returnTo: info) }
            else if info.kind.retryable { askPin(info.priceMsat, info.clientRef) }
        case (.backPressed, .error(let info)):
            if info.unresolvedRef == nil { state = .entering }
        case (.abandonPressed, .error(let info)) where info.unresolvedRef != nil:
            pad.clear(); state = .entering
        case (.backPressed, .sales(let r)):
            state = back(to: r)
        case (.newSalePressed, .receipt):
            pad.clear(); state = .entering
        case (.salesOpened, .entering):
            state = .sales(returnTo: nil)
        case (.salesOpened, .error(let info)):
            state = .sales(returnTo: info)

        case (.saleFound(let sale), _):
            if let ref = unresolvedRef, sale.clientRef == ref {
                dropPin()
                state = .receipt(ChargeReceipt(saleId: sale.id, txid: sale.txid, priceMsat: sale.priceMsat,
                                               receivedMsat: sale.receivedMsat, overpaidMsat: sale.overpaidMsat,
                                               feesMsat: sale.feesMsat, inputs: [], change: [], collectionPending: false,
                                               warnings: [], durationMs: 0))
                if case .waitingForCard = s { return [.endCardSession] }
            }

        default:
            break
        }
        return []
    }
}
