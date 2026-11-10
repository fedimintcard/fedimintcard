import Foundation

enum ErrorKind: Equatable {
    case badRequest, busy, noCard, relayUnreachable, cardError, insufficient, noExactPlan
    case declined, undecided, recovering, conflict, pinRequired, wrongPin, pinBlocked, cardChanged
    case unknownCard, opsUnreachable, opsRefused, cardLinkGone, unauthorized, `internal`, unknown
    /// Offline, the price needs change and the card has no slot for it (not a full card).
    case offlineNoChangeRoom
    /// The phone could not reach the till (nothing was sent).
    case tillUnreachable
    /// The till did not answer in time: the outcome may already have happened.
    case timeoutUnknown
    /// No till is paired on this phone.
    case notPaired
    // The phone cannot take the card itself (docs/POS-OFFLINE.md): no kit yet (or offline is off),
    // the kit expired, the caps on what waits to sync reached; the card's notes do not check
    // offline; the kit has run out of a tier the price needs.
    case offlineNoKit, offlineKitExpired, offlineCapReached, offlineNoteInvalid, offlinePoolExhausted
    /// An applet 2.x card: it pays online only.
    case offlineUnsupportedCard
    /// Not a card the registry certified (a clone).
    case cardNotGenuine
    /// The card's daily limit.
    case cardLimit
    /// The card was taken away while it signed a charge on this phone: the sale is held here, and
    /// only that card's next tap on this phone finishes it.
    case heldOnPhone

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
        case .offlineNoChangeRoom: return .offlineNoChangeRoom
        case .cardLinkGone: return .cardLinkGone
        case .unauthorized: return .unauthorized
        case .internal: return .internal
        case .offlineKitExpired: return .offlineKitExpired
        case .offlinePoolExhausted: return .offlinePoolExhausted
        case .offlineNoteInvalid: return .offlineNoteInvalid
        case .offlineCapReached: return .offlineCapReached
        case .offlineNotReady: return .offlineNoKit
        case .offlineUnsupportedCard: return .offlineUnsupportedCard
        case .cardNotGenuine: return .cardNotGenuine
        case .cardLimit: return .cardLimit
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
    var needsSalesCheck: Bool { self == .undecided || self == .timeoutUnknown || self == .heldOnPhone }

    /// Nothing moved: clears an unresolved sale. A till without ops refuses the PIN from inside the
    /// spend, after it has settled any earlier attempt with the same ref; a till on ops answers its
    /// card attach's `no_card`, `card_error`, `wrong_pin` and `pin_blocked` before it looks at one,
    /// so no refusal lets a sale sent to the till fall back to the phone (`PosFlow.sentToTill`).
    /// `pin_required`, the ops codes and the link/auth refusals come before the earlier attempt on
    /// every till, so they prove nothing about it.
    var definitive: Bool {
        [.insufficient, .noExactPlan, .offlineNoChangeRoom, .declined, .badRequest, .noCard, .cardError, .wrongPin, .pinBlocked].contains(self)
    }

    /// Whether trying the same sale again can help.
    var retryable: Bool { ![.pinBlocked, .notPaired, .unauthorized, .offlineUnsupportedCard, .cardNotGenuine, .cardLimit].contains(self) }

    /// Kinds whose copy speaks of this phone's own kit and queue. The till answers some of their
    /// codes for its own offline path; from the till they read as before (`unknown`, its words).
    var phoneOnly: Bool {
        [.offlineNoKit, .offlineKitExpired, .offlineCapReached, .offlineNoteInvalid, .offlinePoolExhausted, .heldOnPhone].contains(self)
    }
}

struct PosErrorInfo: Equatable {
    let priceMsat: Int64
    let kind: ErrorKind
    let detail: String
    let clientRef: String
    let unresolvedRef: String?
    /// A price the card can pay instead (`offline_no_change_room`), when the till said one.
    var suggestedMsat: Int64? = nil
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

/// `chargeLocal`: a sale the phone takes itself, with no card link to the till.
enum CardPurpose: Equatable { case charge, chargeLocal, balance, topUp }

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
    /// `waiting`: a paid top-up the till holds for this card, offered once the till answers.
    case balanceShown(readId: Int, liveMsat: Int64, waiting: WaitingTopUp? = nil)
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
    /// The card link to the till could not be opened; nothing was sent. The NFC session is still
    /// up: the phone may take the card itself.
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
    /// The till's paid top-ups for the card just read (nil: none, or the till could not say).
    case waitingFetched(readId: Int, topUps: [WaitingTopUp]?)
    /// "Load waiting top-up" on the balance screen.
    case loadWaitingPressed
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
    /// The phone takes the card itself, with the card in the NFC session (docs/POS-OFFLINE.md);
    /// answers `chargeDone` as `startCharge` does. `pin` is wiped before the core is called.
    case startLocalCharge(chargeId: Int, priceMsat: Int64, clientRef: String, pin: Pin?)
    /// Ends the NFC session, `message` on its sheet.
    case failCardSession(message: String)
    case startBalanceRead(readId: Int)
    /// `POST /v1/topup`.
    case createTopUp(createId: Int, amountMsat: Int64, clientRef: String, shortId: String?)
    /// `POST /v1/topup/{id}/load` over the card now linked.
    case loadTopUp(loadId: Int, topupId: String)
    /// `GET /v1/topups/waiting` for the card just read; answers `.waitingFetched`.
    case fetchWaiting(readId: Int, shortId: String)
}

/// Pure state machine for one till session. Call from the main thread.
final class PosFlow {
    /// How long the holder sees their balance before the amount comes back.
    static let balanceShownSeconds = 15

    let pad = AmountPad()
    /// The top-up amount: separate from `pad`, which keeps the sale being typed.
    let topUpPad = AmountPad()
    /// The card of the last balance read, and the card a top-up started from it is for: sent with
    /// every create so the till fits the amount to that card's free slots.
    private var shownCard: String?
    private(set) var topUpCard: String?
    let pinPad = PinPad()
    private var heldPin: Pin?
    private(set) var state: PosState = .entering
    private var nextId = 0
    private let newRef: () -> String
    /// The phone's offline side as it is now; the controller sets it before every event.
    var offline: OfflineSnapshot = .disabled
    var clock: () -> Int64 = { Int64(Date().timeIntervalSince1970 * 1000) }
    /// The NFC session asked for at the PIN is for a charge on the phone.
    private var localSession = false
    /// The charge in progress is on the phone.
    private var localChargeId: Int?
    /// Sales sent to the till's charge route: never taken on the phone, whatever the till answered
    /// since or their unresolved state. The route had the sale, and nothing it answers later proves
    /// the earlier attempt was not taken: a till on ops answers `no_card`, `card_error`, `wrong_pin`
    /// and `pin_blocked` from its card attach before it looks at an earlier attempt with the same
    /// `client_ref`, and a 500, a 5xx the app cannot read or a code it does not know leave nothing
    /// unresolved. Only the till's own `client_ref` guard stops a second charge
    /// (docs/POS-OFFLINE.md). Dropped on success, when the sale is done.
    private var sentToTill: Set<String> = []

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

    /// A refusal's kind. On the phone, `undecided` is a sale held here. From the till, the codes
    /// whose copy speaks of this phone's kit and queue read as before: the till's own words.
    private static func kind(_ code: ChargeErrorCode, local: Bool) -> ErrorKind {
        let k = ErrorKind.from(code)
        if local { return k == .undecided ? .heldOnPhone : k }
        return k.phoneOnly ? .unknown : k
    }

    private func back(to r: PosErrorInfo?) -> PosState { r.map { .error($0) } ?? .entering }

    /// `held`: the sale is held on this phone, so it stays unresolved whatever `kind` says.
    private func errorFrom(price: Int64, ref: String, prevUnresolved: String?, kind: ErrorKind, detail: String, suggestedMsat: Int64? = nil,
                           held: Bool = false) -> PosState {
        let unresolved: String?
        if held || kind.needsSalesCheck { unresolved = ref } else if kind.definitive { unresolved = nil } else { unresolved = prevUnresolved }
        return .error(PosErrorInfo(priceMsat: price, kind: kind, detail: detail, clientRef: ref, unresolvedRef: unresolved, suggestedMsat: suggestedMsat))
    }

    /// The screen of a sale held on this phone, for Cancel on its PIN screen to go back to.
    private static func heldInfo(_ price: Int64, _ ref: String) -> PosErrorInfo {
        PosErrorInfo(priceMsat: price, kind: .heldOnPhone, detail: "", clientRef: ref, unresolvedRef: ref)
    }

    private func createTopUp(_ msat: Int64, _ ref: String) -> [PosEffect] {
        let cid = id()
        state = .topUpCreating(createId: cid, requestedMsat: msat, clientRef: ref)
        return [.createTopUp(createId: cid, amountMsat: msat, clientRef: ref, shortId: topUpCard)]
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
            topUpPad.clear(); topUpCard = shownCard; state = .topUpEntering
        case (.chargeCardPressed, .balanceShown):
            state = .entering
        case (.waitingFetched(let rid, let list), .balanceShown(let cur, let live, _)) where rid == cur:
            state = .balanceShown(readId: cur, liveMsat: live, waiting: list?.first)
        case (.waitingFetched, _):
            return [] // a stale reply: the balance it belonged to is gone
        case (.loadWaitingPressed, .balanceShown(_, _, let w?)):
            topUpCard = shownCard
            return waitForCard(w.asTopUp)
        case (.createInvoicePressed, .topUpEntering):
            return topUpPad.isChargeable ? createTopUp(topUpPad.msat, newRef()) : []
        case (.cancelPressed, .topUpEntering), (.cancelPressed, .topUpInvoice):
            state = .entering
        case (.topUpCreated(let cid, let r), .topUpCreating(let cur, let msat, let ref)) where cid == cur:
            if case .ok(let t) = r { state = .topUpInvoice(t, clientRef: ref) }
            else {
                // A full card refused before any invoice exists: nothing was paid, nothing to retry.
                let p = topUpProblem(r)
                state = .topUpFailed(TopUpFailure(problem: p == .cardFull ? .noRoom : p, detail: r.refusedDetail, requestedMsat: msat, clientRef: ref))
            }
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
            if f.problem == .failed || f.problem == .notPaired || f.problem == .noRoom { return [] }
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
            // A sale held on this phone, or whose charge here threw, is retried here only, whatever
            // the till's health: the card may have signed part of a transaction only its next tap
            // here finishes, and the till would take the sale again (docs/POS-OFFLINE.md).
            localSession = offline.localOnlyRefs.contains(ref)
            return [.startCardSession(localSession ? .chargeLocal : .charge)]

        case (.cancelPressed, .enteringPin(_, _, _, _, let returnTo)):
            dropPin(); state = back(to: returnTo)

        case (.cardPresent, .waitingForCard(let price, let ref, let returnTo)):
            let cid = id()
            state = .signing(priceMsat: price, chargeId: cid, clientRef: ref, unresolvedRef: returnTo?.unresolvedRef)
            let pin = heldPin
            heldPin = nil // the request owns it now, and wipes it
            if localSession {
                localChargeId = cid
                return [.startLocalCharge(chargeId: cid, priceMsat: price, clientRef: ref, pin: pin)]
            }
            // The till's charge route has the sale now: nothing it answers later proves this attempt
            // was not taken, so the sale never falls back to the phone.
            sentToTill.insert(ref)
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
            // Nothing reached the till's charge route in this attempt: an unresolved sale stays
            // unresolved.
            let unresolved = returnTo?.unresolvedRef
            if te == .unreachable || te == .timeout {
                // The till is not serving. The phone takes the card itself, in the same NFC session,
                // only for a sale no attempt was ever sent to the till's charge route for (an
                // unresolved one's reply may only have been lost, and a refusal proves nothing about
                // the attempt before it), while the kit is valid and under its caps
                // (docs/POS-OFFLINE.md).
                let sent = unresolved != nil || sentToTill.contains(ref)
                let avail = offline.availability(priceMsat: price, nowMs: clock())
                if !sent && avail == .ready {
                    let cid = id()
                    localChargeId = cid
                    state = .signing(priceMsat: price, chargeId: cid, clientRef: ref, unresolvedRef: nil)
                    let pin = heldPin
                    heldPin = nil // the local charge owns it now, and wipes it
                    return [.startLocalCharge(chargeId: cid, priceMsat: price, clientRef: ref, pin: pin)]
                }
                dropPin()
                // The unreachable screen says why: a sale the till's charge route had is the till's
                // (an unresolved one's copy is the lost reply); otherwise what the phone lacks.
                let kind: ErrorKind
                switch (sent, avail) {
                case (true, _), (false, .ready): kind = .tillUnreachable
                case (false, .noKit): kind = .offlineNoKit
                case (false, .kitExpired): kind = .offlineKitExpired
                case (false, .capsReached): kind = .offlineCapReached
                }
                state = .error(PosErrorInfo(priceMsat: price, kind: kind, detail: "", clientRef: ref, unresolvedRef: unresolved))
                return [.failCardSession(message: "Can't reach the till")]
            }
            // Refused (401, 403) or not paired: never taken on the phone. The pairing is dead, and
            // the kit could not be renewed either.
            dropPin()
            let kind = ErrorKind.from(te) == .timeoutUnknown ? .tillUnreachable : ErrorKind.from(te)
            state = .error(PosErrorInfo(priceMsat: price, kind: kind, detail: "", clientRef: ref, unresolvedRef: unresolved))
            return [.failCardSession(message: te == .unauthorized ? "The till refused this phone" : "Can't reach the till")]
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
            case .read(let b):
                shownCard = b.shortId; state = .balanceShown(readId: cur, liveMsat: b.liveMsat)
                // The balance shows at once; a paid top-up waiting for this card is offered when
                // the till answers.
                if let sid = b.shortId { return [.fetchWaiting(readId: cur, shortId: sid)] }
            case .refused(_, let d): state = .balanceFailed(balanceProblem(r), detail: d)
            case .failed(let te):
                if case .badResponse(let d) = te { state = .balanceFailed(balanceProblem(r), detail: d) }
                else { state = .balanceFailed(balanceProblem(r), detail: "") }
            }
        case (.balanceExpired(let rid), .balanceShown(let cur, _, _)) where rid == cur:
            state = .entering
        case (.donePressed, .balanceShown), (.donePressed, .balanceFailed), (.backPressed, .balanceFailed):
            state = .entering
        case (.retryPressed, .balanceFailed):
            return balance()

        case (.chargeDone(let cid, let r), .signing(let price, let cur, let ref, let unresolved)) where cid == cur:
            dropPin()
            let local = cid == localChargeId
            localChargeId = nil
            // A sale the core still holds `signing` stays unresolved whatever this tap answered: the
            // core refuses at its card checks (`no_card`, `wrong_pin`, `pin_blocked`, an applet or a
            // certificate it does not take) before it reaches the held charge, which that card's
            // next tap here still finishes. The controller refreshes the snapshot before this event.
            let held = local && offline.localOnlyRefs.contains(ref)
            switch r {
            case .success(let receipt):
                sentToTill.remove(ref)
                state = .receipt(receipt)
            case .declined(let code, let detail, _, _) where code == .wrongPin:
                // The same ref is a fresh attempt (TILL-API, wrong_pin). A sale sent to the till
                // stays in `sentToTill`: the refusal proves nothing about an earlier attempt.
                let left = parseTriesLeft(detail)
                if left == 0 {
                    state = .error(PosErrorInfo(priceMsat: price, kind: .pinBlocked, detail: "", clientRef: ref, unresolvedRef: held ? ref : nil))
                } else {
                    // Cancel goes back to a held sale, never to a new one.
                    askPin(price, ref, wrongPin: true, triesLeft: left, returnTo: held ? Self.heldInfo(price, ref) : nil)
                }
            case .declined(let code, let detail, _, let suggested):
                let kind = Self.kind(code, local: local)
                state = errorFrom(price: price, ref: ref, prevUnresolved: unresolved, kind: kind, detail: detail, suggestedMsat: suggested, held: held)
            case .failed(let te):
                var detail = ""
                if case .badResponse(let d) = te { detail = d }
                state = errorFrom(price: price, ref: ref, prevUnresolved: unresolved, kind: .from(te), detail: detail, held: held)
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
