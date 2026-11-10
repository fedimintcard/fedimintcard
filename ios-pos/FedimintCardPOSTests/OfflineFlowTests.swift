import XCTest
@testable import FedimintCardPOS

// The phone as an offline till (docs/POS-OFFLINE.md, ENG-843): PosFlow and OfflinePhone over a
// fake core and a fake till. No card, no NFC: the core is faked, so APDUs never happen here.

/// The core, scripted: charges answer `outcomes` in turn (Paid by default), the store's lists are
/// plain fields. Called on OfflinePhone's queue, read by the tests after each await.
final class FakeCore: OfflineCore {
    static let phone = "0123456789abcdef0123456789abcdef"

    struct Charge: Equatable { let ref: String; let priceMsat: Int64; let pin: String; let kit: String }

    var outcomes: [CoreOutcome] = []
    private(set) var charges: [Charge] = []
    /// `stats()`, with `waiting` raised by the charges in `waiting`.
    var storeStats = OfflineStats()
    /// `signing_refs()`; `signingThrows` makes it fail.
    var signing: [String] = []
    var signingThrows = false
    /// Txids waiting to sync, oldest first.
    var waiting: [String] = []
    var refused: [RefusedOfflineCharge] = []
    private(set) var drains: [Int] = []
    private(set) var marked: [String] = []
    private(set) var dismissed: [String] = []

    func phoneId() -> String { Self.phone }

    func charge(card: BlockingCardChannel, kitJson: String, priceMsat: Int64, clientRef: String, pin: String) -> CoreOutcome {
        charges.append(Charge(ref: clientRef, priceMsat: priceMsat, pin: pin, kit: kitJson))
        if !outcomes.isEmpty { return outcomes.removeFirst() }
        return .paid(txid: "tx-\(clientRef)", priceMsat: priceMsat, tillMsat: priceMsat, overpayMsat: 0, feesMsat: 0,
                     inputs: [Note(slot: 0, msat: priceMsat)], change: [], replayed: false)
    }

    func stats() throws -> OfflineStats {
        var s = storeStats
        s.waiting += waiting.count
        return s
    }

    func signingRefs() throws -> [String] {
        if signingThrows { throw CoreFailure(description: "the store is locked") }
        return signing
    }

    func drain(max: Int) throws -> CoreDrain {
        drains.append(max)
        let txids = Array(waiting.prefix(max))
        let body = #"{"phone_id":"\#(Self.phone)","charges":["# + txids.map { #"{"txid":"\#($0)"}"# }.joined(separator: ",") + "]}"
        return CoreDrain(body: body, txids: txids, refused: refused)
    }

    /// The till's answer, read as the core reads it: `queued`/`already` synced, `refused` with
    /// `retry: false` kept as refused, anything else waits.
    func markSynced(answerJson: String) throws -> CoreSyncReport {
        marked.append(answerJson)
        guard let o = try? JSONSerialization.jsonObject(with: Data(answerJson.utf8)) as? [String: Any],
              let results = o["results"] as? [[String: Any]] else { throw CoreFailure(description: "not an answer") }
        var synced = 0, refusedNow = 0, retry = 0
        for r in results {
            guard let txid = r["txid"] as? String, waiting.contains(txid) else { continue }
            switch (r["status"] as? String, r["retry"] as? Bool ?? false) {
            case ("queued", _), ("already", _): synced += 1; waiting.removeAll { $0 == txid }
            case ("refused", false):
                refusedNow += 1; waiting.removeAll { $0 == txid }
                refused.append(RefusedOfflineCharge(txid: txid, code: r["code"] as? String ?? "", priceMsat: 5000, tsUnixMs: 1))
            default: retry += 1
            }
        }
        return CoreSyncReport(synced: synced, refused: refusedNow, retry: retry)
    }

    func dismiss(txid: String) throws -> Bool {
        dismissed.append(txid)
        let had = refused.contains { $0.txid == txid }
        refused.removeAll { $0.txid == txid }
        return had
    }
}

/// The till, by path: each route answers what the test sets, and every request is kept.
final class FakeTill: HTTPTransport {
    private let lock = NSLock()
    private var routes: [String: Result<RawResponse, TransportError>] = [:]
    /// Answers for the next requests of a path only, in turn, before its route's.
    private var once: [String: [Result<RawResponse, TransportError>]] = [:]
    private var log: [HTTPRequest] = []

    func answer(_ path: String, _ status: Int, _ body: String) { set(path, .success(RawResponse(status: status, body: Data(body.utf8)))) }
    func fail(_ path: String, _ e: TransportError) { set(path, .failure(e)) }
    /// `path`'s next request (after any already set this way) gets this; then its route answers.
    func answerOnce(_ path: String, _ status: Int, _ body: String) {
        lock.lock(); once[path, default: []].append(.success(RawResponse(status: status, body: Data(body.utf8)))); lock.unlock()
    }
    private func set(_ path: String, _ r: Result<RawResponse, TransportError>) { lock.lock(); routes[path] = r; lock.unlock() }

    var requests: [HTTPRequest] { lock.lock(); defer { lock.unlock() }; return log }
    var paths: [String] { requests.map(\.url.path) }

    func send(_ req: HTTPRequest) async throws -> RawResponse {
        guard let r = record(req) else { return RawResponse(status: 404, body: Data(#"{"ok":false,"error":"not_found"}"#.utf8)) }
        return try r.get()
    }

    private func record(_ req: HTTPRequest) -> Result<RawResponse, TransportError>? {
        lock.lock()
        defer { lock.unlock() }
        log.append(req)
        if let next = once[req.url.path]?.first {
            once[req.url.path]?.removeFirst()
            return next
        }
        return routes[req.url.path]
    }
}

/// The card in the NFC session: the fake core never asks it anything.
final class SilentCard: BlockingCardChannel {
    func transmit(_ apdu: Data) throws -> Data { Data([0x90, 0x00]) }
}

enum Offline {
    static let base = "http://10.0.0.2:9877"
    static let day: Int64 = 86_400_000
    static let t0: Int64 = 1_800_000_000_000

    static func kit(id: String = "k1", phone: String = FakeCore.phone, fed: String = "fed1", issued: Int64 = t0,
                    expires: Int64 = t0 + 7 * day, capCount: Int = 200, capMsat: Int64 = 100_000_000, ok: Bool = true) -> String {
        #"{"ok":\#(ok),"kit":{"version":1,"kit_id":"\#(id)","phone_id":"\#(phone)","federation_id":"\#(fed)","mint_instance_id":1,"tbs_pks":{"1024":"aa"},"fee_consensus":"00","fee_headroom":{"base_msat":100,"ppm":0},"surplus_allowed":false,"cert_key":"04","cap_msat":\#(capMsat),"cap_count":\#(capCount),"issued_at_ms":\#(issued),"expires_at_ms":\#(expires),"pool":[{"amount_msat":1024,"blinded":["aa","bb"]},{"amount_msat":2048,"blinded":["cc"]}]}}"#
    }

    static let status = #"{"federation_id":"fed1","federation_name":"Island","wallet_balance_msat":1000,"card_relay_ok":true,"busy":false}"#

    static func info(issued: Int64 = t0, expires: Int64 = t0 + 7 * day, capCount: Int = 200, capMsat: Int64 = 100_000_000) -> KitInfo {
        KitInfo(kitId: "k1", phoneId: FakeCore.phone, federationId: "fed1", capMsat: capMsat, capCount: capCount,
                issuedAtMs: issued, expiresAtMs: expires, poolEntries: 3, fetchedAtMs: issued)
    }

    /// A snapshot with a valid kit (unless `kit` says otherwise) at `t0`.
    static func ready(_ kit: KitInfo? = info(), stats: OfflineStats = OfflineStats(), localOnly: Set<String> = []) -> OfflineSnapshot {
        OfflineSnapshot(enabled: true, disabledReason: nil, kit: kit, stats: stats, localOnlyRefs: localOnly, refused: [])
    }
}

/// Runs the flow's effects as PosController does, without the NFC session: the till's charge
/// over `till`, the phone's over `phone`. Card-session effects are only kept.
@MainActor
final class Runner {
    let flow: PosFlow
    let core: FakeCore
    let till: FakeTill
    let phone: OfflinePhone
    let client: TillClient
    private(set) var sessions: [PosEffect] = []
    private var n = 0
    var now = Offline.t0

    init() {
        var refs = 0
        flow = PosFlow { refs += 1; return "ref\(refs)" }
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("offline-\(UUID().uuidString)", isDirectory: true)
        let core = FakeCore()
        let till = FakeTill()
        self.core = core
        self.till = till
        var clockNow: () -> Int64 = { Offline.t0 }
        phone = OfflinePhone(directory: { dir }, prepare: { try FileManager.default.createDirectory(at: $0, withIntermediateDirectories: true) },
                             open: { _ in core }, clock: { clockNow() })
        client = TillClient(pairing: { TillPairing(baseURL: Offline.base, token: "tok") }, transport: till)
        till.answer("/v1/status", 200, Offline.status)
        till.answer("/v1/offline/kit", 200, Offline.kit())
        clockNow = { [unowned self] in self.now }
        flow.clock = { [unowned self] in self.now }
    }

    /// The store open, and a kit from the till.
    func ready() async {
        phone.tillChanged(to: Offline.base)
        await phone.start()
        let kept = await phone.refreshKit(client: client, federationId: "fed1", tillURL: Offline.base)
        XCTAssertTrue(kept, "the kit is kept")
    }

    func send(_ e: PosEvent) async {
        flow.offline = phone.snapshot
        for fx in flow.dispatch(e) { await run(fx) }
    }

    private func run(_ fx: PosEffect) async {
        switch fx {
        case .startCharge(let cid, let price, let ref, let pin):
            let r = await client.charge(priceMsat: price, clientRef: ref, pin: pin, cardLink: "0123456789abcdef")
            await send(.chargeDone(chargeId: cid, result: r))
        case .startLocalCharge(let cid, let price, let ref, let pin):
            let r = await phone.charge(card: SilentCard(), priceMsat: price, clientRef: ref, pin: pin) { [unowned self] in
                _ = await self.phone.refreshKit(client: self.client, federationId: "fed1", tillURL: Offline.base)
            }
            await send(.chargeDone(chargeId: cid, result: r))
        default:
            sessions.append(fx)
        }
    }

    /// Amount, Charge, PIN, Pay: the NFC session is asked for.
    func ring(_ sats: Int = 5, pin: String = "1234") async {
        if case .entering = flow.state {
            for c in String(sats) { flow.pad.press(Int(String(c))!) }
            await send(.chargePressed)
        }
        for c in pin { flow.pinPad.press(Int(String(c))!) }
        await send(.pinConfirmed)
    }

    var lastSession: CardPurpose? {
        for fx in sessions.reversed() { if case .startCardSession(let p) = fx { return p } }
        return nil
    }

    var error: PosErrorInfo? { if case .error(let e) = flow.state { return e }; return nil }
}

@MainActor
final class OfflineFlowTests: XCTestCase {
    private var n = 0
    private func flow(_ sats: Int = 5, offline: OfflineSnapshot = Offline.ready()) -> PosFlow {
        let f = PosFlow { self.n += 1; return "ref\(self.n)" }
        f.offline = offline
        f.clock = { Offline.t0 }
        for c in String(sats) { f.pad.press(Int(String(c))!) }
        return f
    }
    private func toWaiting(_ f: PosFlow, pin: String = "1234") -> [PosEffect] {
        if case .entering = f.state { f.dispatch(.chargePressed) }
        for c in pin { f.pinPad.press(Int(String(c))!) }
        return f.dispatch(.pinConfirmed)
    }
    private func pinText(_ p: Pin?) -> String? { p?.withBytes { String(decoding: $0, as: UTF8.self) } }

    // MARK: where the phone takes the card

    func testTheTillOutOfReachAtLinkOpenTakesTheCardOnThePhone() {
        let f = flow()
        guard case .startCardSession(.charge)? = toWaiting(f).first else { return XCTFail("the till is asked first") }
        let fx = f.dispatch(.linkFailed(.unreachable))
        guard case .startLocalCharge(let cid, 5000, "ref1", let pin)? = fx.first, fx.count == 1 else { return XCTFail("\(fx)") }
        XCTAssertEqual(pinText(pin), "1234", "the PIN typed for the till goes to the phone's charge")
        XCTAssertEqual(f.state, .signing(priceMsat: 5000, chargeId: cid, clientRef: "ref1", unresolvedRef: nil))
        XCTAssertFalse(f.holdsPin, "the local charge owns the PIN")
        // The link's quick timeout is the till not serving too.
        let g = flow()
        _ = toWaiting(g)
        guard case .startLocalCharge? = g.dispatch(.linkFailed(.timeout)).first else { return XCTFail() }
    }

    func testARefusedPhoneNeverFallsBack() {
        let f = flow()
        _ = toWaiting(f)
        let fx = f.dispatch(.linkFailed(.unauthorized))
        guard case .failCardSession("The till refused this phone")? = fx.first, fx.count == 1 else { return XCTFail("\(fx)") }
        XCTAssertEqual(f.state, .error(PosErrorInfo(priceMsat: 5000, kind: .unauthorized, detail: "", clientRef: "ref1", unresolvedRef: nil)))
        XCTAssertFalse(f.holdsPin)
        let g = flow()
        _ = toWaiting(g)
        guard case .failCardSession("Can't reach the till")? = g.dispatch(.linkFailed(.notPaired)).first else { return XCTFail() }
        XCTAssertEqual(errorInfo(g)?.kind, .notPaired)
        XCTAssertNil(errorInfo(g)?.unresolvedRef)
    }

    func testBalanceAndTopUpNeverFallBack() {
        let f = flow()
        f.dispatch(.cardBalanceOpened)
        let fx = f.dispatch(.linkFailed(.unreachable))
        guard case .endCardSession? = fx.first, fx.count == 1 else { return XCTFail("\(fx)") }
        XCTAssertEqual(f.state, .balanceFailed(.tillUnreachable, detail: ""))

        let g = flow()
        g.dispatch(.cardBalanceOpened)
        guard case .startBalanceRead(let rid)? = g.dispatch(.cardPresent).first else { return XCTFail() }
        g.dispatch(.balanceDone(readId: rid, result: .read(CardBalance(liveMsat: 1, notes: [], freeSlots: []))))
        g.dispatch(.topUpPressed)
        g.topUpPad.press(1)
        guard case .createTopUp(let cid, _, _, _)? = g.dispatch(.createInvoicePressed).first else { return XCTFail() }
        let t = TopUp(topupId: "t1", amountMsat: 1000, invoice: "lnbc", expiresAtMs: 1, state: "waiting")
        g.dispatch(.topUpCreated(createId: cid, result: .ok(t)))
        g.dispatch(.topUpPolled(topupId: "t1", result: .ok(TopUpStatus(topupId: "t1", amountMsat: 1000, state: "paid", loadTxid: nil, detail: nil))))
        let fx2 = g.dispatch(.linkFailed(.unreachable))
        guard case .endCardSession? = fx2.first, fx2.count == 1 else { return XCTFail("\(fx2)") }
        guard case .topUpFailed(let fl) = g.state else { return XCTFail("\(g.state)") }
        XCTAssertEqual(fl.problem, .tillUnreachable)
    }

    func testASaleWithAnEarlierAttemptUnresolvedIsNotTakenOnThePhone() {
        let f = flow()
        _ = toWaiting(f)
        guard case .startCharge(let id, _, let ref, _)? = f.dispatch(.cardPresent).first else { return XCTFail() }
        f.dispatch(.chargeDone(chargeId: id, result: .failed(.timeout)))
        XCTAssertEqual(f.unresolvedRef, ref)
        f.dispatch(.retryPressed)
        guard case .startCardSession(.charge)? = toWaiting(f).first else { return XCTFail() }
        let fx = f.dispatch(.linkFailed(.unreachable))
        guard case .failCardSession("Can't reach the till")? = fx.first, fx.count == 1 else { return XCTFail("\(fx)") }
        guard let e = errorInfo(f) else { return XCTFail("\(f.state)") }
        XCTAssertEqual(e.kind, .tillUnreachable)
        XCTAssertEqual(e.unresolvedRef, ref, "still unresolved: its reply may only have been lost")
        let (title, detail) = errorCopy(e.kind, e.detail, unresolved: e.unresolvedRef != nil)
        XCTAssertEqual(title, "Till service unreachable")
        XCTAssertEqual(detail, "The last reply was lost, so this sale can't be taken on the phone. Try again once the till answers.")
    }

    func testNeverAfterTheChargeWasSent() {
        let f = flow()
        _ = toWaiting(f)
        guard case .startCharge(let id, _, let ref, _)? = f.dispatch(.cardPresent).first else { return XCTFail() }
        let fx = f.dispatch(.chargeDone(chargeId: id, result: .failed(.timeout)))
        XCTAssertTrue(fx.isEmpty, "\(fx)")
        guard let e = errorInfo(f) else { return XCTFail() }
        XCTAssertEqual(e.kind, .timeoutUnknown)
        XCTAssertEqual(e.unresolvedRef, ref)
        XCTAssertEqual(errorCopy(e.kind, "").0, "Outcome unknown")
        // An unreachable till after the link opened: the charge route may not have been reached,
        // and still nothing is taken on the phone.
        let g = flow()
        _ = toWaiting(g)
        guard case .startCharge(let id2, _, _, _)? = g.dispatch(.cardPresent).first else { return XCTFail() }
        XCTAssertTrue(g.dispatch(.chargeDone(chargeId: id2, result: .failed(.unreachable))).isEmpty)
        XCTAssertEqual(errorInfo(g)?.kind, .tillUnreachable)
    }

    /// Answers to a first `POST /v1/charge` that leave nothing unresolved and yet do not prove that
    /// nothing moved: a 500 (the till's worker dropped a job it had taken), a 5xx the app cannot
    /// read (a proxy that gave up first), a code the app does not know, no answer at all. The till
    /// may have taken the sale, and only its own `client_ref` guard stops a second charge, so the
    /// retry's link failing never takes the card on the phone.
    func testARetryOfASaleTheTillsChargeRouteHadIsNeverTakenOnThePhone() {
        let answers: [ChargeResult] = [
            .declined(code: .internal, detail: "the till service stopped before answering", httpStatus: 500),
            .failed(.badResponse("bad json")),
            .declined(code: .unknown, detail: "", httpStatus: 500),
            .failed(.unreachable),
        ]
        for answer in answers {
            let f = flow()
            _ = toWaiting(f)
            guard case .startCharge(let id, _, let ref, _)? = f.dispatch(.cardPresent).first else { return XCTFail("\(answer)") }
            f.dispatch(.chargeDone(chargeId: id, result: answer))
            guard let first = errorInfo(f) else { return XCTFail("\(answer): \(f.state)") }
            XCTAssertNil(first.unresolvedRef, "\(answer): the online path's screen is as it was")
            XCTAssertTrue(first.kind.retryable, "\(answer)")
            f.dispatch(.retryPressed)
            guard case .startCardSession(.charge)? = toWaiting(f).first else { return XCTFail("\(answer): the till is asked first") }
            let fx = f.dispatch(.linkFailed(.unreachable))
            guard case .failCardSession("Can't reach the till")? = fx.first, fx.count == 1 else { return XCTFail("\(answer): \(fx)") }
            guard let e = errorInfo(f) else { return XCTFail("\(answer): \(f.state)") }
            XCTAssertEqual(e.kind, .tillUnreachable, "\(answer)")
            XCTAssertEqual(e.clientRef, ref, "\(answer)")
            XCTAssertNil(e.unresolvedRef, "\(answer)")
            XCTAssertEqual(errorCopy(e.kind, e.detail).1, tillUnreachableCopy.1, "\(answer)")
            XCTAssertFalse(f.holdsPin, "\(answer)")
            // Nor on any later retry.
            f.dispatch(.retryPressed)
            _ = toWaiting(f)
            guard case .failCardSession? = f.dispatch(.linkFailed(.unreachable)).first else { return XCTFail("\(answer): \(f.state)") }
        }
    }

    /// A refusal from the till's charge route proves nothing about the attempt before it: a till on
    /// ops answers `no_card`, `card_error`, `wrong_pin` and `pin_blocked` from its card attach,
    /// before it looks at an earlier attempt with the same `client_ref`. So once `POST /v1/charge`
    /// was sent for a sale, a retry whose link fails never takes the card on the phone, whatever
    /// the till answered, with a kit that would take it.
    func testARetryAfterAnyAnswerFromTheTillsChargeRouteNeverFallsBack() {
        let answers: [ChargeResult] = [
            .declined(code: .noCard, detail: "", httpStatus: 409),
            .declined(code: .insufficient, detail: "", httpStatus: 402),
            .declined(code: .wrongPin, detail: "wrong PIN; 2 tries left before the card blocks", httpStatus: 403),
        ]
        for answer in answers {
            let f = flow(offline: Offline.ready())
            _ = toWaiting(f)
            guard case .startCharge(let id, _, let ref, _)? = f.dispatch(.cardPresent).first else { return XCTFail("\(answer)") }
            f.dispatch(.chargeDone(chargeId: id, result: answer))
            if case .error = f.state { f.dispatch(.retryPressed) }
            guard case .startCardSession(.charge)? = toWaiting(f).first else { return XCTFail("\(answer): \(f.state)") }
            XCTAssertEqual(f.offline.availability(priceMsat: 5000, nowMs: Offline.t0), .ready, "\(answer): the kit would take it")
            let fx = f.dispatch(.linkFailed(.unreachable))
            guard case .failCardSession("Can't reach the till")? = fx.first, fx.count == 1 else { return XCTFail("\(answer): \(fx)") }
            XCTAssertFalse(fx.contains { if case .startLocalCharge = $0 { return true }; return false }, "\(answer)")
            guard let e = errorInfo(f) else { return XCTFail("\(answer): \(f.state)") }
            XCTAssertEqual(e.kind, .tillUnreachable, "\(answer)")
            XCTAssertEqual(e.clientRef, ref, "\(answer)")
            XCTAssertNil(e.unresolvedRef, "\(answer)")
            XCTAssertFalse(f.holdsPin, "\(answer)")
        }
    }

    func testNoKitExpiredKitAndCapsSayWhichAndTakeNothing() {
        let cases: [(OfflineSnapshot, ErrorKind, String)] = [
            (Offline.ready(nil), .offlineNoKit, "No offline kit yet. Reach the till once while online."),
            (OfflineSnapshot.disabled, .offlineNoKit, "No offline kit yet. Reach the till once while online."),
            (Offline.ready(Offline.info(expires: Offline.t0)), .offlineKitExpired, "The offline kit has expired. Reach the till to renew it."),
            (Offline.ready(Offline.info(capCount: 2), stats: OfflineStats(waiting: 1, signing: 1)), .offlineCapReached,
             "Offline limit reached. Reach the till so waiting payments can sync."),
            (Offline.ready(Offline.info(capMsat: 9_000), stats: OfflineStats(waiting: 1, waitingMsat: 5_000)), .offlineCapReached,
             "Offline limit reached. Reach the till so waiting payments can sync."),
        ]
        for (snapshot, kind, detail) in cases {
            let f = flow(offline: snapshot)
            _ = toWaiting(f)
            let fx = f.dispatch(.linkFailed(.unreachable))
            guard case .failCardSession("Can't reach the till")? = fx.first, fx.count == 1 else { return XCTFail("\(kind): \(fx)") }
            guard let e = errorInfo(f) else { return XCTFail("\(kind)") }
            XCTAssertEqual(e.kind, kind)
            XCTAssertNil(e.unresolvedRef)
            XCTAssertTrue(e.kind.retryable, "\(kind)")
            XCTAssertFalse(f.holdsPin)
            XCTAssertEqual(errorCopy(e.kind, e.detail).0, "Till service unreachable")
            XCTAssertEqual(errorCopy(e.kind, e.detail).1, detail)
        }
        // A kit issued "later" than the phone's clock is judged from its issue.
        XCTAssertEqual(Offline.ready(Offline.info(issued: Offline.t0 + 10, expires: Offline.t0 + 5)).availability(priceMsat: 1, nowMs: Offline.t0),
                       .kitExpired)
        XCTAssertEqual(Offline.ready(Offline.info(capCount: 2), stats: OfflineStats(waiting: 1)).availability(priceMsat: 1, nowMs: Offline.t0), .ready)
    }

    // MARK: the phone's charge, end to end over the fake core

    func testPaidOfflineGivesTheReceiptAndTheTillIsNeverCharged() async {
        let r = Runner()
        await r.ready()
        r.till.fail("/v1/charge", .unreachable)
        await r.ring(5)
        XCTAssertEqual(r.lastSession, .charge)
        await r.send(.linkFailed(.unreachable))
        guard case .receipt(let rc) = r.flow.state else { return XCTFail("\(r.flow.state)") }
        XCTAssertTrue(rc.offline)
        XCTAssertEqual(rc.txid, "tx-ref1")
        XCTAssertEqual(receiptHeadline(rc), "Paid offline · 5 sats")
        XCTAssertEqual(r.core.charges, [FakeCore.Charge(ref: "ref1", priceMsat: 5000, pin: "1234", kit: Offline.kit())])
        XCTAssertFalse(r.till.paths.contains("/v1/charge"))
    }

    func testAHeldSaleIsRetriedOnThePhoneOnlyWithTheTillHealthy() async {
        let r = Runner()
        await r.ready()
        r.core.outcomes = [.refused(code: "undecided", detail: "the card was taken away while it signed", suggestedMsat: nil, triesLeft: nil)]
        r.core.signing = ["ref1"]
        await r.ring(5)
        await r.send(.linkFailed(.unreachable))
        guard let e = r.error else { return XCTFail("\(r.flow.state)") }
        XCTAssertEqual(e.kind, .heldOnPhone)
        XCTAssertEqual(e.unresolvedRef, "ref1")
        XCTAssertEqual(errorCopy(e.kind, e.detail, unresolved: true).0, "Payment not finished")
        XCTAssertEqual(errorCopy(e.kind, e.detail, unresolved: true).1,
                       "The card was taken away while it signed. Hold the same card to the phone and press Try again. It won't be charged twice.")
        XCTAssertEqual(abandonCopy(held: r.phone.snapshot.localOnlyRefs.contains(e.clientRef)),
                       "The card already signed part of this payment. Its next tap on this phone still finishes it.")
        XCTAssertTrue(r.phone.snapshot.localOnlyRefs.contains("ref1"))
        await r.send(.backPressed)
        XCTAssertNotNil(r.error, "Back is not offered while the sale is held")

        // The till answers again: Try again still goes to the phone, the same sale.
        r.till.answer("/v1/charge", 200, #"{"ok":true,"txid":"online","price_msat":5000}"#)
        r.core.signing = []
        await r.send(.retryPressed)
        await r.ring()
        XCTAssertEqual(r.lastSession, .chargeLocal)
        await r.send(.cardPresent)
        guard case .receipt(let rc) = r.flow.state else { return XCTFail("\(r.flow.state)") }
        XCTAssertTrue(rc.offline)
        XCTAssertEqual(r.core.charges.map(\.ref), ["ref1", "ref1"])
        XCTAssertFalse(r.till.paths.contains("/v1/charge"), "a held sale never reaches the till's charge route")
        XCTAssertFalse(r.phone.snapshot.localOnlyRefs.contains("ref1"), "finished: the sale is the till's again")
    }

    func testAChargeThatThrewIsRetriedOnThePhoneWithTheSameRef() async {
        let r = Runner()
        await r.ready()
        r.core.outcomes = [.threw("charge: the core failed unexpectedly (boom); nothing more is known")]
        await r.ring(5)
        await r.send(.linkFailed(.unreachable))
        guard let e = r.error else { return XCTFail("\(r.flow.state)") }
        XCTAssertEqual(e.kind, .heldOnPhone, "a throw has an unknown outcome")
        XCTAssertTrue(r.core.signing.isEmpty)
        XCTAssertTrue(r.phone.snapshot.localOnlyRefs.contains("ref1"), "kept local-only without signing_refs()")
        r.till.answer("/v1/charge", 200, #"{"ok":true,"txid":"online","price_msat":5000}"#)
        await r.send(.retryPressed)
        await r.ring()
        XCTAssertEqual(r.lastSession, .chargeLocal)
        await r.send(.cardPresent)
        XCTAssertEqual(r.core.charges.map(\.ref), ["ref1", "ref1"])
        XCTAssertFalse(r.till.paths.contains("/v1/charge"))
        guard case .receipt = r.flow.state else { return XCTFail("\(r.flow.state)") }
    }

    func testASigningRefsFailureKeepsTheSaleLocal() async {
        let r = Runner()
        await r.ready()
        r.core.signingThrows = true
        await r.ring(5)
        r.core.outcomes = [.refused(code: "no_card", detail: "gone", suggestedMsat: nil, triesLeft: nil)]
        await r.send(.linkFailed(.unreachable))
        XCTAssertTrue(r.phone.snapshot.localOnlyRefs.contains("ref1"), "fails closed: the last set plus this sale")
    }

    /// A sale held on the phone, and the card left again, or a retap the core refuses at its card
    /// checks: those come before it reaches the held charge, so `signing_refs()` still lists the
    /// sale. It stays held: no Back, Try again stays on the phone, and Abandon's alert says the
    /// card's next tap here still finishes it.
    private func heldSale() async -> Runner {
        let r = Runner()
        await r.ready()
        r.core.outcomes = [.refused(code: "undecided", detail: "the card was taken away while it signed", suggestedMsat: nil, triesLeft: nil)]
        r.core.signing = ["ref1"]
        await r.ring(5)
        await r.send(.linkFailed(.unreachable))
        XCTAssertEqual(r.error?.kind, .heldOnPhone)
        return r
    }

    private let heldAbandon = "The card already signed part of this payment. Its next tap on this phone still finishes it."

    func testAHeldSaleStaysHeldWhenTheCoreRefusesItsRetryAtTheCardChecks() async {
        let r = await heldSale()
        let refusals: [(String, ErrorKind)] = [
            ("no_card", .noCard), ("card_error", .cardError), ("pin_blocked", .pinBlocked),
            ("offline_unsupported_card", .offlineUnsupportedCard), ("card_not_genuine", .cardNotGenuine),
        ]
        for (code, kind) in refusals {
            r.core.outcomes = [.refused(code: code, detail: "d", suggestedMsat: nil, triesLeft: nil)]
            await r.send(.retryPressed)
            await r.ring()
            XCTAssertEqual(r.lastSession, .chargeLocal, code)
            await r.send(.cardPresent)
            guard let e = r.error else { return XCTFail("\(code): \(r.flow.state)") }
            XCTAssertEqual(e.kind, kind, code)
            XCTAssertEqual(e.unresolvedRef, "ref1", "\(code): still held")
            await r.send(.backPressed)
            XCTAssertEqual(r.error, e, "\(code): no Back while the sale is held")
            XCTAssertEqual(abandonCopy(held: r.phone.snapshot.localOnlyRefs.contains(e.clientRef)), heldAbandon, code)
        }
        XCTAssertEqual(r.core.charges.map(\.ref), Array(repeating: "ref1", count: 1 + refusals.count))
        XCTAssertFalse(r.till.paths.contains("/v1/charge"))
        // The same card's next tap finishes it.
        r.core.outcomes = []
        r.core.signing = []
        await r.send(.retryPressed)
        await r.ring()
        await r.send(.cardPresent)
        guard case .receipt(let rc) = r.flow.state else { return XCTFail("\(r.flow.state)") }
        XCTAssertTrue(rc.offline)
    }

    func testAWrongPinOnAHeldSaleGoesBackToTheHeldSale() async {
        let r = await heldSale()
        let held = PosErrorInfo(priceMsat: 5000, kind: .heldOnPhone, detail: "", clientRef: "ref1", unresolvedRef: "ref1")
        r.core.outcomes = [.refused(code: "wrong_pin", detail: "d", suggestedMsat: nil, triesLeft: 2)]
        await r.send(.retryPressed)
        await r.ring()
        await r.send(.cardPresent)
        XCTAssertEqual(r.flow.state, .enteringPin(priceMsat: 5000, clientRef: "ref1", wrongPin: true, triesLeft: 2, returnTo: held))
        XCTAssertEqual(r.flow.unresolvedRef, "ref1")
        await r.send(.cancelPressed)
        XCTAssertEqual(r.error, held, "Cancel goes back to the held sale, never to a new one")
        await r.send(.backPressed)
        XCTAssertEqual(r.error, held)
        XCTAssertEqual(abandonCopy(held: r.phone.snapshot.localOnlyRefs.contains("ref1")), heldAbandon)
        // The last try gone: the PIN is blocked, and the sale is still held.
        r.core.outcomes = [.refused(code: "wrong_pin", detail: "d", suggestedMsat: nil, triesLeft: 0)]
        await r.send(.retryPressed)
        await r.ring()
        await r.send(.cardPresent)
        XCTAssertEqual(r.error, PosErrorInfo(priceMsat: 5000, kind: .pinBlocked, detail: "", clientRef: "ref1", unresolvedRef: "ref1"))
        XCTAssertFalse(r.till.paths.contains("/v1/charge"))
    }

    /// A sale nothing holds on the phone keeps the online screens: Back, and the online alert.
    func testASaleNothingHoldsReadsAsBefore() async {
        let r = Runner()
        await r.ready()
        r.core.outcomes = [.refused(code: "no_card", detail: "gone", suggestedMsat: nil, triesLeft: nil)]
        await r.ring(5)
        await r.send(.linkFailed(.unreachable))
        XCTAssertEqual(r.error, PosErrorInfo(priceMsat: 5000, kind: .noCard, detail: "gone", clientRef: "ref1", unresolvedRef: nil))
        XCTAssertFalse(r.phone.snapshot.localOnlyRefs.contains("ref1"))
        await r.send(.backPressed)
        XCTAssertEqual(r.flow.state, .entering)
        // An unresolved sale of the till's: its Abandon alert is the online one.
        let f = flow()
        _ = toWaiting(f)
        guard case .startCharge(let id, _, _, _)? = f.dispatch(.cardPresent).first else { return XCTFail() }
        f.dispatch(.chargeDone(chargeId: id, result: .failed(.timeout)))
        guard let e = errorInfo(f) else { return XCTFail() }
        XCTAssertEqual(abandonCopy(held: f.offline.localOnlyRefs.contains(e.clientRef)),
                       "If the payment did go through, the customer will have paid without a receipt. Check Sales first.")
    }

    func testEveryCoreAnswerMapsToItsScreen() async {
        let cases: [(CoreOutcome, ErrorKind, String)] = [
            (.refused(code: "offline_kit_expired", detail: "d", suggestedMsat: nil, triesLeft: nil), .offlineKitExpired, "Till service unreachable"),
            (.refused(code: "offline_note_invalid", detail: "d", suggestedMsat: nil, triesLeft: nil), .offlineNoteInvalid, "Till service unreachable"),
            (.refused(code: "offline_cap_reached", detail: "d", suggestedMsat: nil, triesLeft: nil), .offlineCapReached, "Till service unreachable"),
            (.refused(code: "offline_not_ready", detail: "d", suggestedMsat: nil, triesLeft: nil), .offlineNoKit, "Till service unreachable"),
            (.refused(code: "offline_pool_exhausted", detail: "d", suggestedMsat: 12_000, triesLeft: nil), .offlinePoolExhausted, "Amount not available offline"),
            (.refused(code: "offline_unsupported_card", detail: "d", suggestedMsat: nil, triesLeft: nil), .offlineUnsupportedCard, "This card pays online only"),
            (.refused(code: "card_not_genuine", detail: "d", suggestedMsat: nil, triesLeft: nil), .cardNotGenuine, "Card declined"),
            (.refused(code: "card_limit", detail: "d", suggestedMsat: nil, triesLeft: nil), .cardLimit, "Card limit reached"),
            (.refused(code: "offline_no_change_room", detail: "d", suggestedMsat: 7_000, triesLeft: nil), .offlineNoChangeRoom, "No room for change offline"),
            (.refused(code: "insufficient", detail: "d", suggestedMsat: nil, triesLeft: nil), .insufficient, "Card balance too low"),
            (.refused(code: "no_card", detail: "d", suggestedMsat: nil, triesLeft: nil), .noCard, "Card not detected"),
            (.refused(code: "undecided", detail: "d", suggestedMsat: nil, triesLeft: nil), .heldOnPhone, "Payment not finished"),
            (.refused(code: "something_new", detail: "the card said 6F00", suggestedMsat: nil, triesLeft: nil), .cardError, "Card error"),
        ]
        for (out, kind, title) in cases {
            let r = Runner()
            await r.ready()
            r.core.outcomes = [out]
            await r.ring(5)
            await r.send(.linkFailed(.unreachable))
            guard let e = r.error else { return XCTFail("\(kind): \(r.flow.state)") }
            XCTAssertEqual(e.kind, kind, "\(out)")
            XCTAssertEqual(errorCopy(e.kind, e.detail, suggestedMsat: e.suggestedMsat, unresolved: e.unresolvedRef != nil).0, title, "\(out)")
            if kind == .offlinePoolExhausted {
                XCTAssertEqual(e.suggestedMsat, 12_000)
                XCTAssertEqual(errorCopy(e.kind, e.detail, suggestedMsat: e.suggestedMsat).1, "Try 12 sats, or reach the till for a new kit.")
            }
            if kind == .cardError { XCTAssertEqual(e.detail, "the card said 6F00") }
        }
    }

    func testAWrongPinOnThePhoneAsksAgainWithTheTriesLeft() async {
        let r = Runner()
        await r.ready()
        r.core.outcomes = [.refused(code: "wrong_pin", detail: "wrong PIN; 2 tries left before the card blocks", suggestedMsat: nil, triesLeft: 2)]
        await r.ring(5)
        await r.send(.linkFailed(.unreachable))
        XCTAssertEqual(r.flow.state, .enteringPin(priceMsat: 5000, clientRef: "ref1", wrongPin: true, triesLeft: 2, returnTo: nil))
        // The tries come from the core's count, whatever its words.
        XCTAssertEqual(OfflinePhone.result(.refused(code: "wrong_pin", detail: "", suggestedMsat: nil, triesLeft: 1), durationMs: 0),
                       .declined(code: .wrongPin, detail: "wrong PIN; 1 tries left before the card blocks", httpStatus: 0))
        XCTAssertEqual(parseTriesLeft("wrong PIN; 1 tries left before the card blocks"), 1)
    }

    func testTheCoreAnswerAsTheTillsWouldBe() {
        let paid = OfflinePhone.result(.paid(txid: "t", priceMsat: 5000, tillMsat: 5004, overpayMsat: 4, feesMsat: 10,
                                             inputs: [Note(slot: 1, msat: 8192)], change: [Note(slot: 2, msat: 3000)], replayed: false), durationMs: 900)
        XCTAssertEqual(paid, .success(ChargeReceipt(saleId: 0, txid: "t", priceMsat: 5000, receivedMsat: 5004, overpaidMsat: 4, feesMsat: 10,
                                                    inputs: [Note(slot: 1, msat: 8192)], change: [Note(slot: 2, msat: 3000)],
                                                    collectionPending: true, warnings: [], durationMs: 900, offline: true)))
        XCTAssertEqual(OfflinePhone.result(.threw("boom"), durationMs: 0),
                       .declined(code: .undecided, detail: "the phone's offline core failed: boom", httpStatus: 0))
        XCTAssertEqual(OfflinePhone.result(.refused(code: "offline_pool_exhausted", detail: "d", suggestedMsat: 0, triesLeft: nil), durationMs: 0),
                       .declined(code: .offlinePoolExhausted, detail: "d", httpStatus: 0), "a zero price is no suggestion")
    }

    func testTheTillsOwnOfflineCodesReadAsBefore() {
        // The till answers these for its own offline path: not this phone's kit or queue.
        for code: ChargeErrorCode in [.offlineCapReached, .offlineNotReady, .offlineNoteInvalid, .offlineKitExpired, .offlinePoolExhausted] {
            let f = flow()
            _ = toWaiting(f)
            guard case .startCharge(let id, _, _, _)? = f.dispatch(.cardPresent).first else { return XCTFail() }
            f.dispatch(.chargeDone(chargeId: id, result: .declined(code: code, detail: "the till's words", httpStatus: 503)))
            XCTAssertEqual(errorInfo(f)?.kind, .unknown, code.rawValue)
            XCTAssertEqual(errorCopy(.unknown, "the till's words").1, "the till's words")
        }
        for (code, kind): (ChargeErrorCode, ErrorKind) in [(.offlineUnsupportedCard, .offlineUnsupportedCard), (.cardNotGenuine, .cardNotGenuine),
                                                           (.cardLimit, .cardLimit), (.undecided, .undecided)] {
            let f = flow()
            _ = toWaiting(f)
            guard case .startCharge(let id, _, _, _)? = f.dispatch(.cardPresent).first else { return XCTFail() }
            f.dispatch(.chargeDone(chargeId: id, result: .declined(code: code, detail: "", httpStatus: 409)))
            XCTAssertEqual(errorInfo(f)?.kind, kind, code.rawValue)
        }
    }

    // MARK: sync

    func testASyncMarksWhatTheTillTook() async {
        let r = Runner()
        await r.ready()
        r.core.waiting = ["t1", "t2"]
        r.till.answer("/v1/offline/charges", 200, #"{"ok":true,"results":[{"txid":"t1","status":"queued"},{"txid":"t2","status":"already"}]}"#)
        let out = await r.phone.sync(client: r.client, tillURL: Offline.base)
        XCTAssertEqual(out, OfflinePhone.SyncOutcome(synced: 2, drainedEmpty: true))
        XCTAssertEqual(r.core.marked.count, 1)
        let posts = r.till.requests.filter { $0.url.path == "/v1/offline/charges" }
        XCTAssertEqual(posts.count, 1)
        XCTAssertEqual(posts.first?.method, "POST")
        XCTAssertEqual(posts.first?.body.map { String(decoding: $0, as: UTF8.self) },
                       #"{"phone_id":"\#(FakeCore.phone)","charges":[{"txid":"t1"},{"txid":"t2"}]}"#)
        XCTAssertEqual(r.core.drains, [0, 50, 50, 0], "the snapshot's drains (0) list refused charges only")
        XCTAssertTrue(r.phone.kitRefreshDue(), "a sync that synced asks for a new kit")
    }

    func testAFailedPostMarksNothing() async {
        for fail in ["503", "unreachable", "timeout"] {
            let r = Runner()
            await r.ready()
            r.core.waiting = ["t1"]
            switch fail {
            case "503": r.till.answer("/v1/offline/charges", 503, #"{"ok":false,"error":"offline_not_ready"}"#)
            case "unreachable": r.till.fail("/v1/offline/charges", .unreachable)
            default: r.till.fail("/v1/offline/charges", .timeout)
            }
            let out = await r.phone.sync(client: r.client, tillURL: Offline.base)
            XCTAssertEqual(out, OfflinePhone.SyncOutcome(synced: 0, drainedEmpty: false), fail)
            XCTAssertTrue(r.core.marked.isEmpty, fail)
            XCTAssertEqual(r.core.waiting, ["t1"], fail)
            XCTAssertFalse(r.phone.kitRefreshDue(), fail)
        }
    }

    func testASyncThatMovesNothingStops() async {
        let r = Runner()
        await r.ready()
        r.core.waiting = ["t1"]
        r.till.answer("/v1/offline/charges", 200, #"{"ok":true,"results":[{"txid":"t1","status":"refused","code":"internal","retry":true}]}"#)
        let out = await r.phone.sync(client: r.client, tillURL: Offline.base)
        XCTAssertEqual(out, OfflinePhone.SyncOutcome(synced: 0, drainedEmpty: false))
        XCTAssertEqual(r.till.paths.filter { $0 == "/v1/offline/charges" }.count, 1, "one round, not twenty")
        // Another till paired meanwhile: nothing is sent to it (a minute on, past the stop's wait:
        // the sync drains, and stops at the pairing).
        r.now = Offline.t0 + OfflinePhone.syncRetryAfterMs
        r.phone.tillChanged(to: "http://10.0.0.9:9877")
        _ = await r.phone.sync(client: r.client, tillURL: Offline.base)
        XCTAssertEqual(syncDrains(r), 2)
        XCTAssertEqual(r.till.paths.filter { $0 == "/v1/offline/charges" }.count, 1)
    }

    /// A round that moved nothing says so in Settings, and the syncs of the next minute neither
    /// drain nor post.
    func testARoundTheTillTookNoneOfSaysSoAndWaitsAMinute() async {
        let r = Runner()
        await r.ready()
        r.core.waiting = ["t1", "t2"]
        r.till.answer("/v1/offline/charges", 200, Self.answer([("t1", "refused", "internal", true), ("t2", "refused", "internal", true)]))
        let out = await r.phone.sync(client: r.client, tillURL: Offline.base)
        XCTAssertEqual(out, OfflinePhone.SyncOutcome(synced: 0, drainedEmpty: false))
        XCTAssertEqual(r.phone.snapshot.syncStopped, "the till took none of 2 (it asked for them again later)")
        XCTAssertEqual(offlinePanelLines(r.phone.snapshot, nowMs: r.now).last, "Sync stopped: the till took none of 2 (it asked for them again later)")
        XCTAssertEqual(posts(r).count, 1)
        let drains = syncDrains(r)
        r.now = Offline.t0 + OfflinePhone.syncRetryAfterMs - 1
        let quiet = await r.phone.sync(client: r.client, tillURL: Offline.base)
        XCTAssertEqual(quiet, OfflinePhone.SyncOutcome())
        XCTAssertEqual(posts(r).count, 1, "not again within the minute")
        XCTAssertEqual(syncDrains(r), drains, "nor drained")
        r.now = Offline.t0 + OfflinePhone.syncRetryAfterMs
        r.till.answer("/v1/offline/charges", 200, Self.answer([("t1", "queued", nil, nil), ("t2", "queued", nil, nil)]))
        let again = await r.phone.sync(client: r.client, tillURL: Offline.base)
        XCTAssertEqual(again, OfflinePhone.SyncOutcome(synced: 2, drainedEmpty: true))
        XCTAssertEqual(posts(r).count, 2)
        XCTAssertNil(r.phone.snapshot.syncStopped, "a drain that finds nothing clears the line")
        XCTAssertEqual(offlinePanelLines(r.phone.snapshot, nowMs: r.now).last, "0 waiting to sync · 0 held for their cards · 0 synced")
    }

    /// Every HTTP failure, and a 200 the core refuses, stops the sync with its reason in Settings
    /// and marks nothing; the next post is a minute on.
    func testAFailedPostSaysWhyAndWaitsAMinute() async {
        let cases: [(String, (FakeTill) -> Void)] = [
            ("the till did not answer", { $0.fail("/v1/offline/charges", .unreachable) }),
            ("the till took too long", { $0.fail("/v1/offline/charges", .timeout) }),
            ("the connection broke off", { $0.fail("/v1/offline/charges", .interrupted) }),
            ("the till refused the sync (HTTP 503)", { $0.answer("/v1/offline/charges", 503, #"{"ok":false,"error":"offline_not_ready"}"#) }),
            ("the till refused the sync (HTTP 403)", { $0.answer("/v1/offline/charges", 403, #"{"ok":false,"error":"unauthorized"}"#) }),
            // The core's words for an answer it refuses (the fake core's here).
            ("not an answer", { $0.answer("/v1/offline/charges", 200, #"{"ok":false,"error":"internal"}"#) }),
        ]
        for (why, set) in cases {
            let r = Runner()
            await r.ready()
            r.core.waiting = ["t1"]
            set(r.till)
            let out = await r.phone.sync(client: r.client, tillURL: Offline.base)
            XCTAssertEqual(out, OfflinePhone.SyncOutcome(synced: 0, drainedEmpty: false), why)
            XCTAssertEqual(r.core.waiting, ["t1"], why)
            XCTAssertEqual(r.phone.snapshot.syncStopped, why)
            XCTAssertEqual(offlinePanelLines(r.phone.snapshot, nowMs: r.now).last, "Sync stopped: \(why)")
            r.now = Offline.t0 + OfflinePhone.syncRetryAfterMs - 1
            _ = await r.phone.sync(client: r.client, tillURL: Offline.base)
            XCTAssertEqual(posts(r).count, 1, why)
            r.now = Offline.t0 + OfflinePhone.syncRetryAfterMs
            _ = await r.phone.sync(client: r.client, tillURL: Offline.base)
            XCTAssertEqual(posts(r).count, 2, why)
        }
    }

    /// Till A is out of reach, the phone takes sales with A's kit, and the operator pairs till B
    /// ("Unpair anyway?": they stay on the phone). B does not know them: its refusals are never
    /// final, it is not asked again within the minute, and the charges sync once the phone is
    /// paired with A again.
    func testChargesAnotherTillDoesNotKnowWaitForTheTillTheyWereTakenFor() async {
        let r = Runner()
        await r.ready()
        r.core.waiting = ["t1", "t2"]
        let tillB = "http://10.0.0.9:9877"
        let clientB = TillClient(pairing: { TillPairing(baseURL: tillB, token: "tok") }, transport: r.till)
        r.phone.tillChanged(to: tillB)
        r.till.answer("/v1/offline/charges", 200,
                      Self.answer([("t1", "refused", "pool_entry_unknown", false), ("t2", "refused", "pool_entry_unknown", false)]))
        let out = await r.phone.sync(client: clientB, tillURL: tillB)
        XCTAssertEqual(out, OfflinePhone.SyncOutcome(synced: 0, drainedEmpty: false))
        XCTAssertEqual(r.core.waiting, ["t1", "t2"], "not refused for good")
        XCTAssertTrue(r.core.refused.isEmpty)
        XCTAssertTrue(r.phone.snapshot.refused.isEmpty)
        XCTAssertEqual(r.phone.snapshot.stats.waiting, 2)
        XCTAssertEqual(posts(r).count, 1)
        XCTAssertEqual(r.phone.snapshot.syncStopped, "2 payments wait for the till they were taken for")
        XCTAssertEqual(offlinePanelLines(r.phone.snapshot, nowMs: r.now).last, "Sync stopped: 2 payments wait for the till they were taken for")
        // B's answer would not change: within the minute a sync neither drains nor posts.
        let drains = syncDrains(r)
        r.now = Offline.t0 + OfflinePhone.syncRetryAfterMs - 1
        let quiet = await r.phone.sync(client: clientB, tillURL: tillB)
        XCTAssertEqual(quiet, OfflinePhone.SyncOutcome())
        XCTAssertEqual(posts(r).count, 1)
        XCTAssertEqual(syncDrains(r), drains)
        // A minute on it asks again, and B still does not know them.
        r.now = Offline.t0 + OfflinePhone.syncRetryAfterMs
        _ = await r.phone.sync(client: clientB, tillURL: tillB)
        XCTAssertEqual(posts(r).count, 2)
        XCTAssertEqual(r.core.waiting, ["t1", "t2"])
        XCTAssertTrue(r.phone.snapshot.refused.isEmpty)
        // Paired with A again, which takes them.
        r.phone.tillChanged(to: Offline.base)
        r.now = Offline.t0 + 2 * OfflinePhone.syncRetryAfterMs
        r.till.answer("/v1/offline/charges", 200, Self.answer([("t1", "queued", nil, nil), ("t2", "queued", nil, nil)]))
        let back = await r.phone.sync(client: r.client, tillURL: Offline.base)
        XCTAssertEqual(back, OfflinePhone.SyncOutcome(synced: 2, drainedEmpty: true))
        XCTAssertTrue(r.core.waiting.isEmpty)
        XCTAssertNil(r.phone.snapshot.syncStopped)
        XCTAssertEqual(posts(r).map(\.url.absoluteString), [tillB, tillB, Offline.base].map { $0 + "/v1/offline/charges" })
    }

    /// What a till does know syncs; the rest of the batch waits for the till it was taken for.
    func testABatchAnotherTillPartlyKnowsSyncsWhatItKnows() async {
        let r = Runner()
        await r.ready()
        r.core.waiting = ["t1", "t2"]
        r.till.answer("/v1/offline/charges", 200, Self.answer([("t1", "queued", nil, nil), ("t2", "refused", "bad_charge", false)]))
        let out = await r.phone.sync(client: r.client, tillURL: Offline.base)
        XCTAssertEqual(out, OfflinePhone.SyncOutcome(synced: 1, drainedEmpty: false))
        XCTAssertEqual(r.core.waiting, ["t2"])
        XCTAssertTrue(r.phone.snapshot.refused.isEmpty)
        XCTAssertEqual(posts(r).count, 1, "the next drain would start with t2 again")
        XCTAssertEqual(r.phone.snapshot.syncStopped, "1 payment waits for the till it was taken for")
        XCTAssertTrue(r.phone.kitRefreshDue(), "a sync that synced asks for a new kit")
    }

    /// Only the till that issued a charge's pool entries says they were reused or are past their
    /// redeem window, and a card signature that does not verify fails at every till: final.
    func testAReusedOrExpiredPoolEntryIsStillFinal() async {
        let r = Runner()
        await r.ready()
        r.core.waiting = ["t1", "t2", "t3"]
        r.till.answer("/v1/offline/charges", 200, Self.answer([("t1", "refused", "pool_entry_reused", false), ("t2", "refused", "pool_entry_expired", false),
                                                               ("t3", "refused", "bad_signature", false)]))
        let out = await r.phone.sync(client: r.client, tillURL: Offline.base)
        XCTAssertEqual(out, OfflinePhone.SyncOutcome(synced: 0, drainedEmpty: true), "a refusal is progress")
        XCTAssertTrue(r.core.waiting.isEmpty)
        XCTAssertEqual(r.phone.snapshot.refused.map(\.code), ["pool_entry_reused", "pool_entry_expired", "bad_signature"])
        XCTAssertNil(r.phone.snapshot.syncStopped)
        XCTAssertEqual(posts(r).count, 1)
    }

    func testOnlyTheRefusalsOfATillThatDoesNotKnowTheChargeWait() throws {
        let answer = Self.answer([
            ("t1", "refused", "pool_entry_unknown", false), ("t2", "refused", "bad_note", false), ("t3", "refused", "bad_charge", nil),
            ("t4", "refused", "pool_entry_reused", false), ("t5", "refused", "pool_entry_expired", false), ("t6", "refused", "bad_signature", false),
            ("t7", "refused", "internal", true), ("t8", "queued", nil, nil), ("t9", "already", nil, nil),
        ])
        let (kept, n) = OfflinePhone.keepForTheirTill(answer)
        XCTAssertEqual(n, 3)
        let o = try XCTUnwrap(try JSONSerialization.jsonObject(with: Data(kept.utf8)) as? [String: Any])
        XCTAssertEqual(o["ok"] as? Bool, true)
        let results = try XCTUnwrap(o["results"] as? [[String: Any]])
        XCTAssertEqual(results.map { $0["txid"] as? String }, (1...9).map { "t\($0)" })
        XCTAssertEqual(results.map { $0["code"] as? String },
                       ["pool_entry_unknown", "bad_note", "bad_charge", "pool_entry_reused", "pool_entry_expired", "bad_signature", "internal", nil, nil])
        XCTAssertEqual(results.map { ($0["retry"] as? Bool).map { "\($0)" } ?? "absent" },
                       ["true", "true", "true", "false", "false", "false", "true", "absent", "absent"])
        // Nothing to keep, or not an answer: the till's words go to the core as they came.
        for a in [
            Self.answer([("t6", "refused", "bad_signature", false)]),
            Self.answer([("t1", "refused", "pool_entry_unknown", true)]),
            #"{"ok":true}"#,
            #"{"ok":false,"error":"internal"}"#,
            "not json",
        ] {
            let (same, none) = OfflinePhone.keepForTheirTill(a)
            XCTAssertEqual(same, a)
            XCTAssertEqual(none, 0, a)
        }
    }

    func testRefusedChargesAreListedUntilDismissed() async {
        let r = Runner()
        await r.ready()
        r.core.waiting = ["t1"]
        r.till.answer("/v1/offline/charges", 200, #"{"ok":true,"results":[{"txid":"t1","status":"refused","code":"pool_entry_reused","retry":false}]}"#)
        _ = await r.phone.sync(client: r.client, tillURL: Offline.base)
        XCTAssertEqual(r.phone.snapshot.refused, [RefusedOfflineCharge(txid: "t1", code: "pool_entry_reused", priceMsat: 5000, tsUnixMs: 1)])
        XCTAssertEqual(refusedChargeText(priceMsat: 5000, code: "pool_entry_reused"), "5 sats · pool_entry_reused: the till refused it after the card paid.")
        await r.phone.dismiss(txid: "t1")
        XCTAssertEqual(r.core.dismissed, ["t1"])
        XCTAssertTrue(r.phone.snapshot.refused.isEmpty)
    }

    // MARK: the kit

    func testOnlyAKitForThisPhoneAndFederationIsKept() async {
        let r = Runner()
        r.phone.tillChanged(to: Offline.base)
        await r.phone.start()
        XCTAssertTrue(r.phone.kitRefreshDue(), "no kit yet")
        let refused: [(Int, String)] = [
            (200, Offline.kit(ok: false)),
            (200, Offline.kit(phone: "ffffffffffffffffffffffffffffffff")),
            (200, Offline.kit(fed: "another")),
            (503, #"{"ok":false,"error":"offline_not_ready"}"#),
            (200, "not json"),
        ]
        for (status, body) in refused {
            r.till.answer("/v1/offline/kit", status, body)
            let kept = await r.phone.refreshKit(client: r.client, federationId: "fed1", tillURL: Offline.base)
            XCTAssertFalse(kept, body)
            XCTAssertNil(r.phone.snapshot.kit, body)
        }
        r.till.answer("/v1/offline/kit", 200, Offline.kit())
        let kept = await r.phone.refreshKit(client: r.client, federationId: "fed1", tillURL: Offline.base)
        XCTAssertTrue(kept)
        XCTAssertEqual(r.phone.snapshot.kit?.kitId, "k1")
        XCTAssertEqual(r.phone.snapshot.kit?.poolEntries, 3)
        let kitReq = r.till.requests.last { $0.url.path == "/v1/offline/kit" }
        XCTAssertEqual(kitReq?.url.query, "phone=\(FakeCore.phone)")
        // A failed fetch keeps the kit there was.
        r.till.fail("/v1/offline/kit", .unreachable)
        let again = await r.phone.refreshKit(client: r.client, federationId: "fed1", tillURL: Offline.base)
        XCTAssertFalse(again)
        XCTAssertEqual(r.phone.snapshot.kit?.kitId, "k1")
        // It survives the app: a new process reads it back.
        let reopened = OfflinePhone(directory: { try XCTUnwrap(r.phone.storeDirectory) }, prepare: { _ in }, open: { _ in r.core }, clock: { Offline.t0 })
        reopened.tillChanged(to: Offline.base)
        await reopened.start()
        XCTAssertEqual(reopened.snapshot.kit?.kitId, "k1")
    }

    func testAFailedFetchWaitsAMinuteNotEveryPoll() async {
        let r = Runner()
        r.phone.tillChanged(to: Offline.base)
        await r.phone.start()
        r.till.answer("/v1/offline/kit", 404, #"{"ok":false,"error":"not_found"}"#)
        XCTAssertTrue(r.phone.kitRefreshDue())
        let kept = await r.phone.refreshKit(client: r.client, federationId: "fed1", tillURL: Offline.base)
        XCTAssertFalse(kept)
        XCTAssertFalse(r.phone.kitRefreshDue(), "a till without the route is not asked at every poll")
        r.now = Offline.t0 + 59_000
        XCTAssertFalse(r.phone.kitRefreshDue())
        r.now = Offline.t0 + 60_000
        XCTAssertTrue(r.phone.kitRefreshDue())
        r.till.answer("/v1/offline/kit", 200, Offline.kit(issued: r.now))
        let again = await r.phone.refreshKit(client: r.client, federationId: "fed1", tillURL: Offline.base)
        XCTAssertTrue(again)
        XCTAssertFalse(r.phone.kitRefreshDue())
    }

    func testTheKitIsFetchedAgainOnItsScheduleNotEveryPoll() async {
        let r = Runner()
        await r.ready()
        XCTAssertFalse(r.phone.kitRefreshDue(), "a fresh kit")
        r.now = Offline.t0 + 9 * 60_000
        XCTAssertFalse(r.phone.kitRefreshDue())
        r.now = Offline.t0 + 10 * 60_000
        XCTAssertTrue(r.phone.kitRefreshDue(), "ten minutes old")
        _ = await r.phone.refreshKit(client: r.client, federationId: "fed1", tillURL: Offline.base)
        XCTAssertFalse(r.phone.kitRefreshDue())
        // A local refusal that a new kit may cure asks for one.
        for code in ["offline_pool_exhausted", "offline_kit_expired", "offline_cap_reached"] {
            r.core.outcomes = [.refused(code: code, detail: "d", suggestedMsat: nil, triesLeft: nil)]
            _ = await r.phone.charge(card: SilentCard(), priceMsat: 5000, clientRef: "s-\(code)", pin: Pin(bytes: Array("1234".utf8)))
            XCTAssertTrue(r.phone.kitRefreshDue(), code)
            _ = await r.phone.refreshKit(client: r.client, federationId: "fed1", tillURL: Offline.base)
            XCTAssertFalse(r.phone.kitRefreshDue(), code)
        }
        r.core.outcomes = [.refused(code: "insufficient", detail: "d", suggestedMsat: nil, triesLeft: nil)]
        _ = await r.phone.charge(card: SilentCard(), priceMsat: 5000, clientRef: "s-ins", pin: nil)
        XCTAssertFalse(r.phone.kitRefreshDue())
    }

    func testAHeldSaleOnAnExpiredKitRenewsTheKitFirst() async {
        let r = Runner()
        await r.ready()
        r.core.outcomes = [.refused(code: "undecided", detail: "d", suggestedMsat: nil, triesLeft: nil)]
        r.core.signing = ["ref1"]
        await r.ring(5)
        await r.send(.linkFailed(.unreachable))
        XCTAssertEqual(r.error?.kind, .heldOnPhone)
        // A week later the till answers with a new kit.
        r.now = Offline.t0 + 8 * Offline.day
        let fresh = Offline.kit(id: "k2", issued: r.now, expires: r.now + 7 * Offline.day)
        r.till.answer("/v1/offline/kit", 200, fresh)
        await r.send(.retryPressed)
        await r.ring()
        XCTAssertEqual(r.lastSession, .chargeLocal, "a held sale skips the till and the kit gate")
        await r.send(.cardPresent)
        XCTAssertEqual(r.core.charges.last?.kit, fresh, "the core finishes it with the renewed kit")
        guard case .receipt = r.flow.state else { return XCTFail("\(r.flow.state)") }
    }

    func testAKitFromAnotherTillGoes() async {
        let r = Runner()
        await r.ready()
        r.phone.tillChanged(to: nil)
        XCTAssertNotNil(r.phone.snapshot.kit, "a forgotten till's kit stays")
        r.phone.tillChanged(to: Offline.base)
        XCTAssertNotNil(r.phone.snapshot.kit, "the same till again")
        r.phone.tillChanged(to: "http://10.0.0.9:9877")
        XCTAssertNil(r.phone.snapshot.kit, "another till: its pool is not this one")
        XCTAssertNil(r.phone.kitTillURL)
        // A kit fetched for a till no longer paired is not kept.
        let kept = await r.phone.refreshKit(client: r.client, federationId: "fed1", tillURL: Offline.base)
        XCTAssertFalse(kept)
    }

    func testAStoreThatWillNotOpenTurnsOfflineOffAndSaysWhy() async {
        struct Nope: Error, CustomStringConvertible { var description: String { "the offline store could not be kept out of backups" } }
        let phone = OfflinePhone(directory: { FileManager.default.temporaryDirectory }, prepare: { _ in throw Nope() },
                                 open: { _ in XCTFail("never opened"); return FakeCore() }, clock: { Offline.t0 })
        await phone.start()
        XCTAssertFalse(phone.snapshot.enabled)
        XCTAssertEqual(phone.snapshot.disabledReason, "the offline store could not be kept out of backups")
        XCTAssertEqual(phone.snapshot.availability(priceMsat: 1, nowMs: Offline.t0), .noKit)
        XCTAssertEqual(offlinePanelLines(phone.snapshot, nowMs: Offline.t0),
                       ["Offline payments are off on this phone: the offline store could not be kept out of backups"])
        let r = await phone.charge(card: SilentCard(), priceMsat: 1000, clientRef: "x", pin: Pin(bytes: Array("1234".utf8)))
        XCTAssertEqual(r, .declined(code: .offlineNotReady, detail: "the offline store could not be kept out of backups", httpStatus: 0))
    }

    func testTheStoreOpensOnceAndAFailureIsTriedAgain() async {
        var opened = 0
        var failFirst = true
        let core = FakeCore()
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("offline-\(UUID().uuidString)", isDirectory: true)
        let phone = OfflinePhone(directory: { dir },
                                 prepare: { d in
                                     if failFirst { failFirst = false; throw CoreFailure(description: "locked") }
                                     try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
                                 },
                                 open: { _ in opened += 1; return core }, clock: { Offline.t0 })
        await phone.start()
        XCTAssertFalse(phone.snapshot.enabled)
        XCTAssertEqual(phone.snapshot.disabledReason, "locked")
        // The next foreground tries again; callers at once share one open.
        async let a: Void = phone.start()
        async let b: Void = phone.start()
        _ = await (a, b)
        await phone.start()
        XCTAssertEqual(opened, 1)
        XCTAssertTrue(phone.snapshot.enabled)
        XCTAssertNil(phone.snapshot.disabledReason)
    }

    func testAKeptKitFromAnotherTillIsDroppedAtStart() async {
        let r = Runner()
        await r.ready()
        let other = OfflinePhone(directory: { try XCTUnwrap(r.phone.storeDirectory) }, prepare: { _ in }, open: { _ in r.core }, clock: { Offline.t0 })
        other.tillChanged(to: "http://10.0.0.9:9877")
        await other.start()
        XCTAssertNil(other.snapshot.kit)
        let same = OfflinePhone(directory: { try XCTUnwrap(r.phone.storeDirectory) }, prepare: { _ in }, open: { _ in r.core }, clock: { Offline.t0 })
        await same.start()
        XCTAssertNil(same.snapshot.kit, "the dropped kit's file is gone")
    }

    func testThePinIsWipedOnEveryPath() async {
        let r = Runner()
        await r.ready()
        let pin = Pin(bytes: Array("1234".utf8))
        _ = await r.phone.charge(card: SilentCard(), priceMsat: 5000, clientRef: "a", pin: pin)
        XCTAssertTrue(pin.isWiped)
        XCTAssertEqual(r.core.charges.last?.pin, "1234")
        let off = OfflinePhone(directory: { FileManager.default.temporaryDirectory }, prepare: { _ in throw CoreFailure(description: "x") },
                               open: { _ in FakeCore() })
        await off.start()
        let pin2 = Pin(bytes: Array("1234".utf8))
        _ = await off.charge(card: SilentCard(), priceMsat: 5000, clientRef: "b", pin: pin2)
        XCTAssertTrue(pin2.isWiped)
    }

    private func errorInfo(_ f: PosFlow) -> PosErrorInfo? { if case .error(let e) = f.state { return e }; return nil }

    /// The sync's posts to the till, in order.
    private func posts(_ r: Runner) -> [HTTPRequest] { r.till.requests.filter { $0.url.path == "/v1/offline/charges" } }
    /// The sync's drains; the snapshot's are `drain(0)`.
    private func syncDrains(_ r: Runner) -> Int { r.core.drains.filter { $0 > 0 }.count }

    /// The till's 200 to `POST /v1/offline/charges`: per charge its txid, status, and `code` and
    /// `retry` when not nil.
    static func answer(_ results: [(String, String, String?, Bool?)]) -> String {
        var items: [String] = []
        for (txid, status, code, retry) in results {
            var item = #"{"txid":"\#(txid)","status":"\#(status)""#
            if let code { item += #","code":"\#(code)","detail":"d""# }
            if let retry { item += #","retry":\#(retry)"# }
            items.append(item + "}")
        }
        return #"{"ok":true,"results":["# + items.joined(separator: ",") + "]}"
    }
}

/// The card's APDUs, as the core makes them: off the main thread, each waiting for the card.
final class NFCApduTransportTests: XCTestCase {
    final class SlowCard: CardChannel {
        func transmit(_ apdu: Data) async throws -> Data {
            try await Task.sleep(nanoseconds: 5_000_000_000)
            return Data([0x90, 0x00])
        }
    }

    private func offMain(_ t: NFCApduTransport, _ apdu: Data = Data([0x00, 0xa4, 0x04, 0x00])) -> Result<Data, Error> {
        let done = expectation(description: "transmit")
        var out: Result<Data, Error>!
        DispatchQueue.global().async {
            out = Result { try t.transmit(apdu) }
            done.fulfill()
        }
        wait(for: [done], timeout: 10)
        return out
    }

    func testAnApduWaitsForTheCardsAnswer() throws {
        let card = MockCard()
        card.responses["00a40400"] = Data([0x02, 0x00, 0x90, 0x00])
        let t = NFCApduTransport(card)
        XCTAssertEqual(try offMain(t).get(), Data([0x02, 0x00, 0x90, 0x00]))
        XCTAssertEqual(card.calls, [Data([0x00, 0xa4, 0x04, 0x00])])
    }

    func testTheMainThreadIsRefused() {
        let card = MockCard()
        let t = NFCApduTransport(card)
        XCTAssertThrowsError(try t.transmit(Data([0x00, 0xa4, 0x04, 0x00])))
        XCTAssertTrue(card.calls.isEmpty, "the card is never asked from the main thread")
    }

    func testACardThatLeftIsLost() {
        let card = MockCard()
        card.failFrom = 1
        guard case .failure = offMain(NFCApduTransport(card)) else { return XCTFail() }
    }

    func testAnUnansweredApduIsLostAfterTheTimeout() {
        let t = NFCApduTransport(SlowCard(), timeout: 0.2)
        let started = Date()
        guard case .failure(let e) = offMain(t) else { return XCTFail() }
        XCTAssertLessThan(Date().timeIntervalSince(started), 3)
        XCTAssertTrue("\(e)".contains("did not answer"), "\(e)")
        XCTAssertTrue(t.isCancelled, "a card that did not answer is not asked again")
    }

    func testTheSessionEndingLosesTheApduWaitingAndEveryLaterOne() {
        let t = NFCApduTransport(SlowCard())
        DispatchQueue.global().asyncAfter(deadline: .now() + 0.2) { t.cancel() }
        let started = Date()
        guard case .failure = offMain(t) else { return XCTFail() }
        XCTAssertLessThan(Date().timeIntervalSince(started), 3)
        let card = MockCard()
        let u = NFCApduTransport(card)
        u.cancel()
        guard case .failure = offMain(u) else { return XCTFail() }
        XCTAssertTrue(card.calls.isEmpty)
    }
}
