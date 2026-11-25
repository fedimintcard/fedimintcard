import XCTest
@testable import FedimintCardPOS

// PosController's offline wiring (docs/POS-OFFLINE.md § Kit and sync, ENG-843), over the fake till
// and core of OfflineFlowTests: when a poll syncs and fetches the kit, what it publishes, and the
// local charge's order. Also what a balance read says of what the till holds back (ENG-867): the NFC
// sheet's last word and the balance screen's text. The NFC session is a fake; the real one and the
// card link stay on the owner's pass on the Max.

/// The NFC session: keeps what the controller asks of it, and a test hands it the card.
final class FakeReader: CardReader {
    var onCard: ((CardChannel) -> Void)?
    var onEnd: ((_ hadCard: Bool) -> Void)?
    private(set) var log: [String] = []

    func begin(alert: String) { log.append("begin") }
    func update(alert: String) {}
    func finish(success message: String) { log.append("success: \(message)") }
    func finish(error message: String) { log.append("error: \(message)") }
    func cancel() { log.append("cancel") }
}

/// A controller paired with the fake till, its offline side over the fake core, on a clock the test
/// moves.
@MainActor
final class ControllerRig {
    let core: FakeCore
    let till: FakeTill
    let reader: FakeReader
    let phone: OfflinePhone
    let pos: PosController
    var now = Offline.t0

    init() {
        let core = FakeCore()
        let till = FakeTill()
        let reader = FakeReader()
        self.core = core
        self.till = till
        self.reader = reader
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("offline-\(UUID().uuidString)", isDirectory: true)
        var clock: () -> Int64 = { Offline.t0 }
        phone = OfflinePhone(directory: { dir }, prepare: { try FileManager.default.createDirectory(at: $0, withIntermediateDirectories: true) },
                             open: { _ in core }, clock: { clock() })
        till.answer("/v1/status", 200, Offline.status)
        till.answer("/v1/offline/kit", 200, Offline.kit())
        pos = PosController(store: MemoryPairingStore(TillPairing(baseURL: Offline.base, token: "tok")), offline: phone,
                            transport: till, reader: reader)
        clock = { [unowned self] in self.now }
    }

    /// The store open, as `start()` opens it first.
    func open() async { await phone.start() }

    /// One status poll, to its end.
    func poll() async {
        guard let t = pos.pollNow() else { return XCTFail("no poll started") }
        await t.value
    }

    var posts: Int { till.paths.filter { $0 == "/v1/offline/charges" }.count }
    var kitFetches: Int { till.paths.filter { $0 == "/v1/offline/kit" }.count }
    /// The sync's drains; the snapshot's are `drain(0)`.
    var syncDrains: Int { core.drains.filter { $0 > 0 }.count }

    /// Types the amount, Charge, the PIN, Pay.
    func ring(_ sats: Int = 5) {
        if case .entering = pos.state {
            for c in String(sats) { pos.key(Int(String(c))!) }
            pos.send(.chargePressed)
        }
        typePin()
    }

    func typePin() {
        for d in [1, 2, 3, 4] { pos.pinKey(d) }
        pos.send(.pinConfirmed)
    }

    /// Waits for what the controller does in a task of its own.
    func until(_ what: String, timeout: TimeInterval = 5, _ done: () -> Bool) async {
        let end = Date().addingTimeInterval(timeout)
        while !done() {
            if Date() > end { return XCTFail("timed out waiting for \(what)") }
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
    }
}

/// A hold ahead for the tests that go through the phone's own clock and time zone (`.current`): it
/// ends exactly at a local midnight about ten days on, so the day it names is the one that midnight
/// starts. Where the process's zone is UTC or east of it, UTC names that same day and a controller or
/// a screen that passed UTC would pass those tests too, so they fail there instead. CI's simulator
/// runs in UTC: the iOS job sets TEST_RUNNER_TZ=America/Jamaica for it.
private func holdAhead(file: StaticString = #filePath, line: UInt = #line) -> (untilMs: Int64, day: String) {
    var cal = Calendar(identifier: .gregorian)
    cal.timeZone = .current
    let midnight = cal.startOfDay(for: Date(timeIntervalSince1970: TimeInterval(PosController.clockMs() + 10 * 86_400_000) / 1000))
    let until = Int64((midnight.timeIntervalSince1970 * 1000).rounded())
    let fmt = DateFormatter()
    fmt.locale = Locale(identifier: "en_US_POSIX")
    fmt.calendar = cal
    fmt.timeZone = .current
    fmt.dateFormat = "MMM d"
    let day = fmt.string(from: midnight)
    let inUtc = onHoldText(strandedMsat: 200_000, untilMs: until, nowMs: PosController.clockMs(), timeZone: TimeZone(identifier: "UTC")!)
    XCTAssertNotEqual(inUtc, "200 sats on hold until \(day)",
                      "the test process's time zone (\(TimeZone.current.identifier)) cannot tell the phone's zone from UTC: run with TEST_RUNNER_TZ=America/Jamaica",
                      file: file, line: line)
    return (until, day)
}

@MainActor
final class PosControllerOfflineTests: XCTestCase {
    private func status(queued: Int) -> String {
        #"{"federation_id":"fed1","federation_name":"Island","wallet_balance_msat":1000,"card_relay_ok":true,"busy":false,"offline":{"queued":\#(queued),"queued_msat":\#(queued * 5000),"double_spent":0}}"#
    }

    private func taken(_ txid: String) -> String { #"{"ok":true,"results":[{"txid":"\#(txid)","status":"queued"}]}"# }

    private let heldAbandon = "The card already signed part of this payment. Its next tap on this phone still finishes it."

    func testChargeRefusalsReachTheNFCSheetAndPaymentScreen() async {
        let cases: [(Int, String, String, ErrorKind, String)] = [
            (502, "ops_refused", "ops refused (409): the card belongs to a different federation or mint", .wrongFederation, "Different federation or mint"),
            (409, "wrong_federation", "", .wrongFederation, "Different federation or mint"),
            (403, "offline_note_invalid", "note failed verification", .offlineNoteInvalid, "Couldn’t verify card notes"),
            (502, "ops_refused", "ops refused (409): unrelated refusal", .opsRefused, "Payment didn't go through"),
        ]
        for (status, code, detail, kind, sheet) in cases {
            let r = ControllerRig()
            await r.open()
            await r.poll()
            r.till.answer("/v1/charge", status, #"{"ok":false,"error":"\#(code)","detail":"\#(detail)"}"#)
            r.ring(5)
            r.pos.send(.cardPresent)
            await r.until("charge refusal") { if case .error = r.pos.state { return true }; return false }
            guard case .error(let info) = r.pos.state else { return XCTFail() }
            XCTAssertEqual(info.kind, kind)
            XCTAssertNil(info.unresolvedRef)
            XCTAssertEqual(r.reader.log, ["begin", "error: " + sheet])
            r.pos.stop()
        }
    }

    func testUnavailableSigningRecordsKeepTheLocalChargeOnItsCard() async {
        for code in ["offline_note_invalid", "wrong_federation"] {
            let r = ControllerRig()
            await r.open()
            await r.poll()
            r.pos.key(5)
            r.pos.send(.chargePressed)
            guard case .enteringPin(_, let ref, _, _, _) = r.pos.state else { return XCTFail() }
            r.core.signing = [ref]
            await r.phone.refresh()
            r.typePin() // This tap goes to the phone core.
            r.core.signing = []
            await r.phone.refresh()
            XCTAssertFalse(r.phone.snapshot.localOnlyRefs.contains(ref))
            r.core.signingThrows = true
            r.core.outcomes = [.refused(code: code, detail: "unchecked", suggestedMsat: nil, wrongTries: nil, waitS: nil)]
            r.reader.onCard?(MockCard())
            await r.until("the held result") { if case .error = r.pos.state { return true }; return false }
            guard case .error(let info) = r.pos.state else { return XCTFail() }
            XCTAssertEqual(info.kind, .heldOnPhone)
            XCTAssertEqual(info.unresolvedRef, ref)
            XCTAssertTrue(r.pos.phone.localOnlyRefs.contains(ref))
            XCTAssertEqual(r.reader.log, ["begin", "error: Payment not finished"])
            XCTAssertFalse(r.till.paths.contains("/v1/charge"))
            r.pos.stop()
        }
    }

    func testFederationMismatchReachesTheNFCSheetAndTheBalanceScreen() async {
        let r = ControllerRig()
        await r.open()
        r.till.answer("/v1/card/balance", 502, #"{"ok":false,"error":"ops_refused","detail":"ops refused (409): the card belongs to a different federation or mint"}"#)
        r.pos.send(.cardBalanceOpened)
        r.pos.send(.cardPresent)
        await r.until("the balance refusal") {
            if case .balanceFailed(.wrongFederation, _) = r.pos.state { return true }
            return false
        }
        XCTAssertEqual(r.reader.log, ["begin", "error: Different federation or mint"])
        r.pos.stop()
    }

    /// A balance read through the controller, the till answering `body`: what the NFC sheet was
    /// told, from its opening to its last word.
    private func sheetAfterBalanceRead(_ body: String) async -> [String] {
        let r = ControllerRig()
        await r.open()
        r.till.answer("/v1/card/balance", 200, body)
        r.pos.send(.cardBalanceOpened)
        r.pos.send(.cardPresent)
        await r.until("the balance") { if case .balanceShown = r.pos.state { return true }; return false }
        r.pos.stop()
        return r.reader.log
    }

    func testADrainThatFindsNothingStopsTheSyncUntilTheNextForeground() async {
        let r = ControllerRig()
        await r.open()
        await r.poll()
        XCTAssertEqual(r.kitFetches, 1, "no kit yet")
        XCTAssertEqual(r.syncDrains, 1, "the first poll syncs and finds nothing waiting")
        XCTAssertEqual(r.pos.phone.kit?.kitId, "k1")
        // Something waits now, and nothing asked for a sync: the polls leave it.
        r.core.waiting = ["t1"]
        await r.poll()
        await r.poll()
        XCTAssertEqual(r.syncDrains, 1)
        XCTAssertEqual(r.posts, 0)
        // The next foreground sends it.
        r.till.answer("/v1/offline/charges", 200, taken("t1"))
        r.pos.start()
        await r.until("the foreground's sync") { r.posts == 1 && !r.pos.pollInFlight }
        r.pos.stop()
        XCTAssertTrue(r.core.waiting.isEmpty)
        XCTAssertEqual(r.core.marked.count, 1)
    }

    func testAReceiptSyncsAgain() async {
        let r = ControllerRig()
        await r.open()
        await r.poll()
        r.core.waiting = ["t1"]
        await r.poll()
        XCTAssertEqual(r.posts, 0, "nothing asked for a sync")
        r.till.answer("/v1/offline/charges", 200, taken("t1"))
        r.till.answer("/v1/charge", 200, #"{"ok":true,"sale_id":7,"txid":"online","price_msat":5000}"#)
        // A sale the till takes; the card link opening is the Max's to check.
        r.ring(5)
        XCTAssertEqual(r.reader.log, ["begin"])
        r.pos.send(.cardPresent)
        await r.until("the receipt's sync") { r.posts == 1 && !r.pos.pollInFlight }
        guard case .receipt(let rc) = r.pos.state else { return XCTFail("\(r.pos.state)") }
        XCTAssertFalse(rc.offline)
        XCTAssertEqual(r.reader.log, ["begin", "success: Paid 5 sats"])
        XCTAssertTrue(r.core.waiting.isEmpty)
    }

    func testAReconnectSyncsAndTheTillsCountMovesWithThePhones() async {
        let r = ControllerRig()
        await r.open()
        await r.poll()
        // A payment the phone took while the till was out of reach.
        r.core.waiting = ["t1"]
        await r.phone.refresh()
        await r.poll()
        XCTAssertEqual(r.posts, 0)
        XCTAssertEqual(r.pos.phone.stats.waiting, 1)
        XCTAssertEqual(waitingToSyncText(till: r.pos.status, phone: r.pos.phone.stats), "1 payment waiting to sync")
        r.till.fail("/v1/status", .unreachable)
        await r.poll()
        await r.poll()
        XCTAssertEqual(r.pos.health, .offline)
        XCTAssertEqual(r.posts, 0)
        // Back: the poll's read, then the read after the sync, with the charge on the till now.
        r.till.answerOnce("/v1/status", 200, status(queued: 0))
        r.till.answer("/v1/status", 200, status(queued: 1))
        r.till.answer("/v1/offline/charges", 200, taken("t1"))
        await r.poll()
        XCTAssertEqual(r.pos.health, .ok)
        XCTAssertEqual(r.posts, 1, "what the phone took meanwhile is sent")
        XCTAssertEqual(r.pos.status?.offlineQueued, 1, "the till's status, read again after the sync")
        XCTAssertEqual(r.pos.phone.stats.waiting, 0, "published with the phone's")
        XCTAssertEqual(waitingToSyncText(till: r.pos.status, phone: r.pos.phone.stats), "1 payment waiting to sync", "the count moved once")
        XCTAssertEqual(r.kitFetches, 2, "a sync that synced asks for a new kit")
    }

    func testSettingsCountsTheDoubleSpendsInTheTillsLastStatus() async {
        let r = ControllerRig()
        await r.open()
        let line = "Offline payments lost to double spends: 2"
        XCTAssertFalse(offlinePanelLines(r.pos.phone, nowMs: r.now, till: r.pos.status).contains { $0.hasPrefix("Offline payments lost") }, "no status read yet")
        r.till.answer("/v1/status", 200, #"{"federation_id":"fed1","federation_name":"Island","wallet_balance_msat":1000,"card_relay_ok":true,"busy":false,"offline":{"queued":0,"queued_msat":0,"double_spent":2}}"#)
        await r.poll()
        XCTAssertEqual(r.pos.status?.offlineDoubleSpent, 2)
        XCTAssertTrue(offlinePanelLines(r.pos.phone, nowMs: r.now, till: r.pos.status).contains(line))
        // The till out of reach: Settings keeps the count from the last status read.
        r.till.fail("/v1/status", .unreachable)
        await r.poll()
        await r.poll()
        XCTAssertEqual(r.pos.health, .offline)
        XCTAssertTrue(offlinePanelLines(r.pos.phone, nowMs: r.now, till: r.pos.status).contains(line))
        // Forgetting the till drops its status, and the line with it.
        r.pos.forgetTill()
        XCTAssertFalse(offlinePanelLines(r.pos.phone, nowMs: r.now, till: r.pos.status).contains { $0.hasPrefix("Offline payments lost") })
    }

    func testALateAnswerFromATillThePhoneLeftIsNotShown() async {
        let r = ControllerRig()
        await r.open()
        let other = TillPairing(baseURL: "http://10.0.0.3:9877", token: "tok2")
        // The till paired first answers the poll still out when the phone pairs another one, which
        // never answers.
        r.till.fail("/v1/status", .unreachable)
        r.till.answerOnce("/v1/status", 200, #"{"federation_id":"fed1","federation_name":"Island","wallet_balance_msat":1000,"card_relay_ok":true,"busy":false,"offline":{"queued":0,"queued_msat":0,"double_spent":2}}"#)
        guard let out = r.pos.pollNow() else { return XCTFail("no poll started") }
        r.pos.pair(other)
        await out.value
        XCTAssertEqual(r.till.requests.first?.url.absoluteString, Offline.base + "/v1/status", "the poll asked the till paired when it started")
        XCTAssertNil(r.pos.status, "that till's answer is not the paired till's status")
        XCTAssertFalse(offlinePanelLines(r.pos.phone, nowMs: r.now, till: r.pos.status).contains { $0.hasPrefix("Offline payments lost") })
        XCTAssertEqual(r.kitFetches, 0, "nor is a kit fetched off it")
        await r.poll()
        XCTAssertEqual(r.till.requests.last?.url.absoluteString, other.baseURL + "/v1/status")
        XCTAssertNil(r.pos.status, "the till paired now has not answered")
        XCTAssertFalse(offlinePanelLines(r.pos.phone, nowMs: r.now, till: r.pos.status).contains { $0.hasPrefix("Offline payments lost") })
        // Forgetting the till while it answers: nothing of it comes back either.
        r.till.answerOnce("/v1/status", 200, #"{"federation_id":"fed1","federation_name":"Island","wallet_balance_msat":1000,"card_relay_ok":true,"busy":false,"offline":{"double_spent":2}}"#)
        guard let late = r.pos.pollNow() else { return XCTFail("no poll started") }
        r.pos.forgetTill()
        await late.value
        XCTAssertNil(r.pos.status)
        XCTAssertEqual(r.pos.health, .notPaired)
    }

    func testThePollFetchesTheKitOnItsScheduleNotEveryTime() async {
        let r = ControllerRig()
        await r.open()
        await r.poll()
        XCTAssertEqual(r.kitFetches, 1)
        r.now = Offline.t0 + 9 * 60_000
        await r.poll()
        await r.poll()
        XCTAssertEqual(r.kitFetches, 1, "not on every poll")
        r.now = Offline.t0 + 10 * 60_000
        await r.poll()
        XCTAssertEqual(r.kitFetches, 2, "ten minutes old")
        await r.poll()
        XCTAssertEqual(r.kitFetches, 2)
    }

    /// A held sale through the controller: Try again opens an NFC session with no card link, the
    /// phone's snapshot is published before the flow hears the core's answer (so a retap refused at
    /// the card checks stays held), and the till never sees the sale.
    func testAHeldSaleStaysOnThePhoneUntilItsCardFinishesIt() async {
        let r = ControllerRig()
        await r.open()
        await r.poll()
        r.pos.key(5)
        r.pos.send(.chargePressed)
        guard case .enteringPin(_, let ref, _, _, _) = r.pos.state else { return XCTFail("\(r.pos.state)") }
        // The core holds the sale `signing` (its card was taken away mid-SIGN before a restart).
        r.core.signing = [ref]
        await r.phone.refresh()
        r.core.outcomes = [.refused(code: "no_card", detail: "the card left", suggestedMsat: nil, wrongTries: nil, waitS: nil)]
        r.typePin()
        XCTAssertEqual(r.reader.log, ["begin"])
        r.reader.onCard?(MockCard())
        await r.until("the core's answer") { if case .error = r.pos.state { return true }; return false }
        guard case .error(let e) = r.pos.state else { return XCTFail("\(r.pos.state)") }
        XCTAssertEqual(e.kind, .noCard)
        XCTAssertEqual(e.unresolvedRef, ref, "still held")
        XCTAssertEqual(r.pos.abandonMessage(for: e), heldAbandon)
        r.pos.send(.backPressed)
        XCTAssertEqual(r.pos.state, .error(e))
        // Its card again: the sale is finished on the phone.
        r.core.outcomes = []
        r.core.signing = []
        r.pos.send(.retryPressed)
        r.typePin()
        r.reader.onCard?(MockCard())
        await r.until("the receipt") { if case .receipt = r.pos.state { return true }; return false }
        guard case .receipt(let rc) = r.pos.state else { return XCTFail("\(r.pos.state)") }
        XCTAssertTrue(rc.offline)
        XCTAssertEqual(r.reader.log, ["begin", "error: Payment didn't go through", "begin", "success: Paid offline · 5 sats"])
        XCTAssertEqual(r.core.charges.map(\.ref), [ref, ref])
        XCTAssertFalse(r.till.paths.contains("/v1/charge"), "a held sale never reaches the till's charge route")
        await r.until("the receipt's poll") { !r.pos.pollInFlight }
    }

    /// The Abandon alert's copy is the controller's, from the phone's snapshot: the online one for
    /// an unresolved sale of the till's, the held one for a sale the phone holds, whatever its
    /// retry answered.
    func testTheAbandonAlertSaysWhetherThePhoneHoldsTheSale() async {
        let r = ControllerRig()
        await r.open()
        await r.poll()
        // The till's sale, its reply lost.
        r.till.fail("/v1/charge", .timeout)
        r.ring(5)
        r.pos.send(.cardPresent)
        await r.until("the till's answer") { if case .error = r.pos.state { return true }; return false }
        guard case .error(let online) = r.pos.state else { return XCTFail("\(r.pos.state)") }
        XCTAssertEqual(online.kind, .timeoutUnknown)
        XCTAssertEqual(online.unresolvedRef, online.clientRef)
        XCTAssertEqual(r.pos.abandonMessage(for: online),
                       "If the payment did go through, the customer will have paid without a receipt. Check Sales first.")
        r.pos.send(.abandonPressed)
        XCTAssertEqual(r.pos.state, .entering)
        // A sale the phone holds (its card was taken away mid-SIGN), whose retry answers no_card.
        r.pos.key(5)
        r.pos.send(.chargePressed)
        guard case .enteringPin(_, let ref, _, _, _) = r.pos.state else { return XCTFail("\(r.pos.state)") }
        r.core.signing = [ref]
        await r.phone.refresh()
        r.core.outcomes = [.refused(code: "no_card", detail: "the card left", suggestedMsat: nil, wrongTries: nil, waitS: nil)]
        r.typePin()
        r.reader.onCard?(MockCard())
        await r.until("the core's answer") { if case .error = r.pos.state { return true }; return false }
        guard case .error(let held) = r.pos.state else { return XCTFail("\(r.pos.state)") }
        XCTAssertEqual(held.kind, .noCard)
        XCTAssertEqual(held.unresolvedRef, ref)
        XCTAssertEqual(r.pos.abandonMessage(for: held), heldAbandon)
        r.pos.stop()
    }

    /// A 500 `internal` and a gateway's 5xx (a body that is not the till's JSON) from the charge
    /// route do not say whether the till took the sale. Through the parse, the flow and the
    /// controller the sale stays unresolved, as after a lost reply: no Back, the online Abandon
    /// alert, and Try again sends the same `client_ref` to the till again.
    func testA500AndAGatewayErrorFromTheChargeRouteLeaveTheSaleUnresolved() async {
        let answers: [(Int, String)] = [
            (500, #"{"ok":false,"error":"internal","detail":"the till service stopped before answering"}"#),
            (502, "<html><body><h1>502 Bad Gateway</h1></body></html>"),
        ]
        for (status, body) in answers {
            let r = ControllerRig()
            await r.open()
            await r.poll()
            r.till.answer("/v1/charge", status, body)
            func onError() -> Bool { if case .error = r.pos.state { return true }; return false }
            func chargeRefs() -> [String?] {
                r.till.requests.filter { $0.url.path == "/v1/charge" }.map { req in
                    req.body.flatMap { try? JSONSerialization.jsonObject(with: $0) as? [String: Any] }?["client_ref"] as? String
                }
            }
            r.ring(5)
            r.pos.send(.cardPresent)
            await r.until("the till's answer") { onError() }
            guard case .error(let e) = r.pos.state else { return XCTFail("\(status): \(r.pos.state)") }
            XCTAssertEqual(e.kind, .timeoutUnknown, "\(status)")
            XCTAssertEqual(e.unresolvedRef, e.clientRef, "\(status)")
            XCTAssertNotNil(r.pos.flow.unresolvedRef, "\(status)")
            XCTAssertEqual(errorCopy(e.kind, e.detail, unresolved: true).0, "Outcome unknown", "\(status)")
            XCTAssertEqual(r.pos.abandonMessage(for: e),
                           "If the payment did go through, the customer will have paid without a receipt. Check Sales first.", "\(status)")
            XCTAssertEqual(r.reader.log, ["begin", "error: Payment didn't go through"], "\(status)")
            r.pos.send(.backPressed)
            XCTAssertEqual(r.pos.state, .error(e), "\(status): no Back while the outcome is unknown")
            // Try again: the PIN, the card, and the same sale to the till's charge route.
            r.pos.send(.retryPressed)
            r.typePin()
            r.pos.send(.cardPresent)
            await r.until("the retry's answer") { chargeRefs().count == 2 && onError() }
            XCTAssertEqual(chargeRefs(), [e.clientRef, e.clientRef], "\(status): Try again never starts a new sale")
            XCTAssertEqual(r.pos.flow.unresolvedRef, e.clientRef, "\(status)")
            r.pos.stop()
        }
    }

    /// Charges another till does not know wait for the till they were taken for, and the polls of
    /// the next minute do not post them again; the sync stays wanted and goes a minute on.
    func testChargesKeptForTheirTillArePostedOnceAMinute() async {
        let r = ControllerRig()
        await r.open()
        r.core.waiting = ["t1", "t2"]
        r.till.answer("/v1/offline/charges", 200, OfflineFlowTests.answer([("t1", "refused", "pool_entry_unknown", false),
                                                                           ("t2", "refused", "pool_entry_unknown", false)]))
        await r.poll()
        XCTAssertEqual(r.posts, 1)
        XCTAssertEqual(r.pos.phone.syncStopped, "2 payments wait for the till they were taken for")
        XCTAssertEqual(r.pos.phone.stats.waiting, 2)
        XCTAssertTrue(r.pos.phone.refused.isEmpty)
        r.now = Offline.t0 + 30_000
        await r.poll()
        XCTAssertEqual(r.posts, 1, "two polls within the minute post once")
        r.now = Offline.t0 + OfflinePhone.syncRetryAfterMs
        await r.poll()
        XCTAssertEqual(r.posts, 2, "still wanted: a minute on, the sync asks again")
        XCTAssertEqual(r.core.waiting, ["t1", "t2"])
    }

    /// ENG-867: the NFC sheet's last word on a balance read names what the till holds back, after
    /// the balance. With no date to wait for (`stranded_until_ms` null), it gives none.
    func testTheSheetSaysWhatIsOnHoldAfterTheBalance() async {
        let log = await sheetAfterBalanceRead(#"{"ok":true,"live_msat":1000000,"stranded_msat":200000,"stranded_until_ms":null,"notes":[],"free_slots":[]}"#)
        XCTAssertEqual(log, ["begin", "success: 1,000 sats on this card. 200 sats on hold"])
    }

    /// A hold ahead of the phone's clock: the sheet names the day, in the phone's time zone. This one
    /// ends exactly at a local midnight about ten days on, so the day is the one that midnight starts.
    func testTheSheetNamesTheDayOfAHoldAhead() async {
        let hold = holdAhead()
        let log = await sheetAfterBalanceRead(#"{"ok":true,"live_msat":1000000,"stranded_msat":200000,"stranded_until_ms":\#(hold.untilMs),"notes":[],"free_slots":[]}"#)
        XCTAssertEqual(log, ["begin", "success: 1,000 sats on this card. 200 sats on hold until \(hold.day)"])
    }

    /// Due by the phone's clock (the time is past): the notes come back within a tap or two, so the
    /// sheet gives no date.
    func testTheSheetGivesNoDateForAHoldThatIsDue() async {
        let due = PosController.clockMs() - 60_000
        let log = await sheetAfterBalanceRead(#"{"ok":true,"live_msat":1000000,"stranded_msat":200000,"stranded_until_ms":\#(due),"notes":[],"free_slots":[]}"#)
        XCTAssertEqual(log, ["begin", "success: 1,000 sats on this card. 200 sats on hold"])
    }

    /// A till from before these fields sends neither: the sheet says the balance and nothing else.
    func testTheSheetSaysOnlyTheBalanceWhenNothingIsOnHold() async {
        let log = await sheetAfterBalanceRead(#"{"ok":true,"live_msat":1000000,"notes":[],"free_slots":[]}"#)
        XCTAssertEqual(log, ["begin", "success: 1,000 sats on this card"])
    }
}

/// What the balance screen says of what a card has on hold (ENG-867), read from `text()`, where
/// `body` reads it: `body` needs the controller in the environment, which a test has not, so none
/// evaluates it. The NFC sheet's four cases above, on the phone's own clock and time zone.
@MainActor
final class BalanceScreenTextTests: XCTestCase {
    private func screen(held: Int64 = 0, until: Int64? = nil) -> BalanceScreen {
        BalanceScreen(liveMsat: 1_000_000, strandedMsat: held, strandedUntilMs: until)
    }

    func testTheScreenSaysWhatIsOnHoldAfterTheBalance() {
        let text = screen(held: 200_000).text()
        XCTAssertEqual(text.line, "200 sats on hold")
        XCTAssertEqual(text.label, "Card balance: 1,000 sats on this card. 200 sats on hold")
    }

    /// A hold ahead of the phone's clock: the line names the day, in the phone's time zone.
    func testTheScreenNamesTheDayOfAHoldAhead() {
        let hold = holdAhead()
        let text = screen(held: 200_000, until: hold.untilMs).text()
        XCTAssertEqual(text.line, "200 sats on hold until \(hold.day)")
        XCTAssertEqual(text.label, "Card balance: 1,000 sats on this card. 200 sats on hold until \(hold.day)")
    }

    /// Due by the phone's clock (the time is past): no date.
    func testTheScreenGivesNoDateForAHoldThatIsDue() {
        let text = screen(held: 200_000, until: PosController.clockMs() - 60_000).text()
        XCTAssertEqual(text.line, "200 sats on hold")
        XCTAssertEqual(text.label, "Card balance: 1,000 sats on this card. 200 sats on hold")
    }

    /// Nothing on hold: no line at all, and the label says the balance only.
    func testTheScreenSaysOnlyTheBalanceWhenNothingIsOnHold() {
        let text = screen().text()
        XCTAssertNil(text.line)
        XCTAssertEqual(text.label, "Card balance: 1,000 sats on this card")
    }
}
