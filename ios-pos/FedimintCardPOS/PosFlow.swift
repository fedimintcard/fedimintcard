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

enum CardPurpose: Equatable { case charge, balance, topUp }

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

    // Top-up by Lightning (docs/TOPUP.md). The amount is typed on `PosFlow.topUpPad`.
    case topUpEntering
    /// Asking the till for the invoice; `clientRef` makes a repeat answer the same top-up.
    case topUpCreating(createId: Int, requestedMsat: Int64, clientRef: String)
    /// The invoice is on screen and the till is polled until it is paid.
    case topUpInvoice(TopUp, clientRef: String)
    /// Paid: the NFC sheet is up, waiting for the card.
    case topUpWaitingForCard(TopUp)
    case topUpLoading(loadId: Int, TopUp)
    case topUpLoaded(amountMsat: Int64, loadTxid: String)
    case topUpFailed(TopUpFailure)

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
    /// On the balance screen: charge this card (the amount keypad, then the charge).
    case chargeCardPressed
    /// On the balance screen: top up this card.
    case topUpPressed
    /// On the top-up keypad.
    case createInvoicePressed
    case topUpCreated(createId: Int, result: TopUpReply<TopUp>)
    /// A poll of the invoice on screen.
    case topUpPolled(topupId: String, result: TopUpReply<TopUpStatus>)
    case topUpLoadDone(loadId: Int, result: TopUpReply<TopUpStatus>)
}

/// A failed top-up step. `topUp` is nil when the invoice was never made (Try again asks for it
/// again with `clientRef`); `paid` when the holder has paid and the card still waits.
struct TopUpFailure: Equatable {
    let problem: TopUpProblem
    let detail: String
    let requestedMsat: Int64
    let clientRef: String
    var topUp: TopUp? = nil
    var paid: Bool = false
}

enum PosEffect {
    case startCardSession(CardPurpose)
    case endCardSession
    /// `pin` is handed over to the request, which wipes it once the body is built.
    case startCharge(chargeId: Int, priceMsat: Int64, clientRef: String, pin: Pin?)
    case startBalanceRead(readId: Int)
    /// `POST /v1/topup`.
    case createTopUp(createId: Int, amountMsat: Int64, clientRef: String)
    /// `POST /v1/topup/{id}/load` over the card now linked.
    case loadTopUp(loadId: Int, topupId: String)
}

/// Pure state machine for one till session. Call from the main thread.
final class PosFlow {
    /// How long the holder sees their balance before the amount comes back.
    static let balanceShownSeconds = 15

    let pad = AmountPad()
    /// The top-up amount: separate from `pad`, which keeps the sale being typed.
    let topUpPad = AmountPad()
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

    private func createTopUp(_ msat: Int64, _ ref: String) -> [PosEffect] {
        let cid = id()
        state = .topUpCreating(createId: cid, requestedMsat: msat, clientRef: ref)
        return [.createTopUp(createId: cid, amountMsat: msat, clientRef: ref)]
    }

    /// Paid: the NFC sheet asks for the card.
    private func waitForCard(_ t: TopUp) -> [PosEffect] {
        state = .topUpWaitingForCard(t)
        return [.startCardSession(.topUp)]
    }

    private func paidFailure(_ p: TopUpProblem, _ detail: String, _ t: TopUp) -> PosState {
        .topUpFailed(TopUpFailure(problem: p, detail: detail, requestedMsat: t.amountMsat, clientRef: "", topUp: t, paid: p != .expired))
    }

    /// The top-up transitions; nil when `e` is not one of them in `s`.
    private func topUp(_ e: PosEvent, _ s: PosState) -> [PosEffect]? {
        switch (e, s) {
        case (.topUpPressed, .balanceShown):
            topUpPad.clear(); state = .topUpEntering
        case (.chargeCardPressed, .balanceShown):
            state = .entering
        case (.createInvoicePressed, .topUpEntering):
            return topUpPad.isChargeable ? createTopUp(topUpPad.msat, newRef()) : []
        case (.cancelPressed, .topUpEntering), (.cancelPressed, .topUpInvoice):
            state = .entering
        case (.topUpCreated(let cid, let r), .topUpCreating(let cur, let msat, let ref)) where cid == cur:
            if case .ok(let t) = r { state = .topUpInvoice(t, clientRef: ref) }
            else { state = .topUpFailed(TopUpFailure(problem: topUpProblem(r), detail: r.refusedDetail, requestedMsat: msat, clientRef: ref)) }
        case (.topUpPolled(let tid, let r), .topUpInvoice(let t0, let ref)) where tid == t0.topupId:
            guard case .ok(let st) = r else { return [] } // a failed poll: poll again
            let t = TopUp(topupId: t0.topupId, amountMsat: st.amountMsat, invoice: t0.invoice, expiresAtMs: t0.expiresAtMs, state: st.state)
            switch st.state {
            case "paid": return waitForCard(t)
            case "loaded": state = .topUpLoaded(amountMsat: st.amountMsat, loadTxid: st.loadTxid ?? "")
            case "expired": state = .topUpFailed(TopUpFailure(problem: .expired, detail: "", requestedMsat: t.amountMsat, clientRef: ref, topUp: t))
            case "failed": state = .topUpFailed(TopUpFailure(problem: .failed, detail: st.detail ?? "", requestedMsat: t.amountMsat, clientRef: ref, topUp: t))
            default: break
            }
        case (.cardPresent, .topUpWaitingForCard(let t)):
            let lid = id()
            state = .topUpLoading(loadId: lid, t)
            return [.loadTopUp(loadId: lid, topupId: t.topupId)]
        // Paid: cancelling the tap (here or on the NFC sheet) must not lose the top-up quietly.
        case (.cancelPressed, .topUpWaitingForCard(let t)):
            state = paidFailure(.notLoaded, "", t); return [.endCardSession]
        case (.cardSessionEnded, .topUpWaitingForCard(let t)):
            state = paidFailure(.notLoaded, "", t)
        case (.linkFailed(let te), .topUpWaitingForCard(let t)):
            state = paidFailure(topUpProblem(TopUpReply<TopUpStatus>.failed(te == .timeout ? .unreachable : te)), "", t)
            return [.endCardSession]
        case (.topUpLoadDone(let lid, let r), .topUpLoading(let cur, let t)) where lid == cur:
            switch r {
            case .ok(let st) where st.state == "loaded": state = .topUpLoaded(amountMsat: st.amountMsat, loadTxid: st.loadTxid ?? "")
            case .ok(let st): state = paidFailure(st.state == "failed" ? .failed : .other, st.detail ?? "", t)
            default: state = paidFailure(topUpProblem(r), r.refusedDetail, t)
            }
        case (.retryPressed, .topUpFailed(let f)):
            if f.problem == .failed || f.problem == .notPaired { return [] }
            // A new invoice: a new ref, so the till makes a new top-up for the same amount.
            if f.problem == .expired { return createTopUp(f.requestedMsat, newRef()) }
            guard let t = f.topUp else { return createTopUp(f.requestedMsat, f.clientRef) }
            return waitForCard(t)
        case (.donePressed, .topUpFailed), (.backPressed, .topUpFailed), (.donePressed, .topUpLoaded), (.newSalePressed, .topUpLoaded):
            state = .entering
        case (.backgrounded, .topUpWaitingForCard(let t)):
            state = paidFailure(.notLoaded, "", t); return [.endCardSession]
        default:
            return nil
        }
        return []
    }

    private func balance() -> [PosEffect] {
        state = .balanceWaiting
        return [.startCardSession(.balance)]
    }

    @discardableResult
    func dispatch(_ e: PosEvent) -> [PosEffect] {
        let s = state
        if let fx = topUp(e, s) { return fx }
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
