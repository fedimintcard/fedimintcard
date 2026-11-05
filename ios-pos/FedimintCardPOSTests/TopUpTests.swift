import XCTest
@testable import FedimintCardPOS

/// Top-up by Lightning from the balance screen (docs/TOPUP.md).
final class TopUpFlowTests: XCTestCase {
    private var n = 0
    private let t = TopUp(topupId: "t1", amountMsat: 100_352, invoice: "lnbc1x", expiresAtMs: 1_000_000, state: "waiting")
    private func st(_ state: String, txid: String? = nil, detail: String? = nil) -> TopUpReply<TopUpStatus> {
        .ok(TopUpStatus(topupId: "t1", amountMsat: 100_352, state: state, loadTxid: txid, detail: detail))
    }

    /// Balance read (in its own NFC session) and shown.
    private func shown() -> PosFlow {
        let f = PosFlow { self.n += 1; return "ref\(self.n)" }
        f.dispatch(.cardBalanceOpened)
        guard case .startBalanceRead(let rid)? = f.dispatch(.cardPresent).first else { XCTFail("no read"); return f }
        f.dispatch(.balanceDone(readId: rid, result: .read(CardBalance(liveMsat: 5_000, notes: [], freeSlots: [], shortId: "6f94b19b"))))
        return f
    }

    private func invoiced() -> PosFlow {
        let f = shown()
        f.dispatch(.topUpPressed)
        for d in [1, 0, 0] { f.topUpPad.press(d) }
        guard case .createTopUp(let cid, _, _, _)? = f.dispatch(.createInvoicePressed).first else { XCTFail("no create"); return f }
        f.dispatch(.topUpCreated(createId: cid, result: .ok(t)))
        return f
    }

    /// Paid, card linked: returns the load effect's id.
    private func loading(_ f: PosFlow) -> Int {
        guard case .startCardSession(.topUp)? = f.dispatch(.topUpPolled(topupId: "t1", result: st("paid"))).first else { XCTFail("no NFC"); return -1 }
        guard case .loadTopUp(let lid, let tid)? = f.dispatch(.cardPresent).first else { XCTFail("no load"); return -1 }
        XCTAssertEqual(tid, "t1")
        return lid
    }

    func testTheCardFromTheBalanceReadGoesWithEveryCreate() {
        let f = shown()
        f.dispatch(.topUpPressed)
        for d in [1, 0, 0] { f.topUpPad.press(d) }
        guard case .createTopUp(let cid, _, _, let card)? = f.dispatch(.createInvoicePressed).first else { return XCTFail() }
        XCTAssertEqual(card, "6f94b19b")
        // A failed create retried, and a new invoice after expiry, keep the card.
        f.dispatch(.topUpCreated(createId: cid, result: .failed(.unreachable)))
        guard case .createTopUp(let cid2, _, _, let card2)? = f.dispatch(.retryPressed).first else { return XCTFail() }
        XCTAssertEqual(card2, "6f94b19b")
        f.dispatch(.topUpCreated(createId: cid2, result: .ok(t)))
        f.dispatch(.topUpPolled(topupId: "t1", result: st("expired")))
        guard case .createTopUp(_, _, _, let card3)? = f.dispatch(.retryPressed).first else { return XCTFail() }
        XCTAssertEqual(card3, "6f94b19b")
    }

    func testAFullCardIsRefusedBeforeAnyInvoice() {
        let f = shown()
        f.dispatch(.topUpPressed)
        for d in [1, 0, 0] { f.topUpPad.press(d) }
        guard case .createTopUp(let cid, _, _, _)? = f.dispatch(.createInvoicePressed).first else { return XCTFail() }
        f.dispatch(.topUpCreated(createId: cid, result: .refused(error: "card_full", detail: "no free slot", httpStatus: 409)))
        guard case .topUpFailed(let fl) = f.state else { return XCTFail("\(f.state)") }
        XCTAssertEqual(fl.problem, .noRoom); XCTAssertFalse(fl.paid); XCTAssertNil(fl.topUp)
        let copy = topUpErrorCopy(fl.problem, paid: fl.paid, detail: fl.detail)
        XCTAssertEqual(copy.0, "This card is full"); XCTAssertEqual(copy.1, "Spend from it first, then top up.")
        XCTAssertTrue(f.dispatch(.retryPressed).isEmpty)
        f.dispatch(.donePressed)
        XCTAssertEqual(f.state, .entering)
    }

    func testTopUpFromTheBalanceScreen() {
        let f = shown()
        f.pad.press(7) // a sale being typed is kept apart
        f.dispatch(.topUpPressed)
        XCTAssertEqual(f.state, .topUpEntering)
        XCTAssertTrue(f.dispatch(.createInvoicePressed).isEmpty) // nothing typed
        for d in [1, 0, 0] { f.topUpPad.press(d) }
        guard case .createTopUp(let cid, let msat, let ref, _)? = f.dispatch(.createInvoicePressed).first else { return XCTFail() }
        XCTAssertEqual(msat, 100_000)
        XCTAssertEqual(f.state, .topUpCreating(createId: cid, requestedMsat: 100_000, clientRef: ref))
        f.dispatch(.topUpCreated(createId: cid, result: .ok(t)))
        XCTAssertEqual(f.state, .topUpInvoice(t, clientRef: ref))
        XCTAssertEqual(f.pad.sats, 7)
    }

    func testChargeCardGoesToTheKeypad() {
        let f = shown()
        f.dispatch(.chargeCardPressed)
        XCTAssertEqual(f.state, .entering)
    }

    func testPaidOpensTheNfcSheetThenLoads() {
        let f = invoiced()
        XCTAssertTrue(f.dispatch(.topUpPolled(topupId: "t1", result: st("waiting"))).isEmpty)
        let lid = loading(f)
        f.dispatch(.topUpLoadDone(loadId: lid, result: st("loaded", txid: "abc")))
        XCTAssertEqual(f.state, .topUpLoaded(amountMsat: 100_352, loadTxid: "abc"))
        f.dispatch(.newSalePressed)
        XCTAssertEqual(f.state, .entering)
    }

    func testOtherInvoicesAndFailedPollsAreIgnored() {
        let f = invoiced()
        f.dispatch(.topUpPolled(topupId: "other", result: st("paid")))
        f.dispatch(.topUpPolled(topupId: "t1", result: .failed(.unreachable)))
        guard case .topUpInvoice = f.state else { return XCTFail("\(f.state)") }
    }

    func testExpiredOffersANewInvoiceWithANewRef() {
        let f = invoiced()
        guard case .topUpInvoice(_, let ref) = f.state else { return XCTFail() }
        f.dispatch(.topUpPolled(topupId: "t1", result: st("expired")))
        guard case .topUpFailed(let e) = f.state else { return XCTFail() }
        XCTAssertEqual(e.problem, .expired)
        guard case .createTopUp(_, let msat, let ref2, _)? = f.dispatch(.retryPressed).first else { return XCTFail() }
        XCTAssertNotEqual(ref, ref2); XCTAssertEqual(msat, 100_352)
    }

    func testAFailedCreateRetriesWithTheSameRef() {
        let f = shown()
        f.dispatch(.topUpPressed); f.topUpPad.press(5)
        guard case .createTopUp(let cid, _, let ref, _)? = f.dispatch(.createInvoicePressed).first else { return XCTFail() }
        f.dispatch(.topUpCreated(createId: cid, result: .failed(.timeout)))
        guard case .topUpFailed(let e) = f.state else { return XCTFail() }
        XCTAssertEqual(e.problem, .timeout)
        guard case .createTopUp(_, let msat, let again, _)? = f.dispatch(.retryPressed).first else { return XCTFail() }
        XCTAssertEqual(again, ref); XCTAssertEqual(msat, 5_000)
    }

    func testBadAmountShowsTheTillsWords() {
        let f = shown()
        f.dispatch(.topUpPressed); f.topUpPad.press(9)
        guard case .createTopUp(let cid, _, _, _)? = f.dispatch(.createInvoicePressed).first else { return XCTFail() }
        f.dispatch(.topUpCreated(createId: cid, result: .refused(error: "bad_amount", detail: "at most 100,000 sats", httpStatus: 400)))
        guard case .topUpFailed(let e) = f.state else { return XCTFail() }
        XCTAssertEqual(e.problem, .badAmount)
        XCTAssertEqual(topUpErrorCopy(e.problem, paid: e.paid, detail: e.detail).1, "at most 100,000 sats")
    }

    func testCardFullStaysPaidAndTapsAgain() {
        let f = invoiced()
        let lid = loading(f)
        f.dispatch(.topUpLoadDone(loadId: lid, result: .refused(error: "card_full", detail: "no free slots", httpStatus: 409)))
        guard case .topUpFailed(let e) = f.state else { return XCTFail() }
        XCTAssertEqual(e.problem, .cardFull); XCTAssertTrue(e.paid)
        XCTAssertEqual(topUpErrorCopy(e.problem, paid: e.paid, detail: e.detail).0, "This card is full")
        guard case .startCardSession(.topUp)? = f.dispatch(.retryPressed).first else { return XCTFail() }
        guard case .topUpWaitingForCard = f.state else { return XCTFail() }
    }

    func testCancellingThePaidTapKeepsTheTopUp() {
        let f = invoiced()
        f.dispatch(.topUpPolled(topupId: "t1", result: st("paid")))
        guard case .endCardSession? = f.dispatch(.cancelPressed).first else { return XCTFail() }
        guard case .topUpFailed(let e) = f.state else { return XCTFail() }
        XCTAssertEqual(e.problem, .notLoaded); XCTAssertTrue(e.paid)
        // the NFC sheet's own Cancel ends the session the same way
        f.dispatch(.retryPressed)
        f.dispatch(.cardSessionEnded)
        guard case .topUpFailed(let e2) = f.state else { return XCTFail() }
        XCTAssertEqual(e2.problem, .notLoaded)
        f.dispatch(.donePressed)
        XCTAssertEqual(f.state, .entering)
    }

    func testLinkFailureWhilePaid() {
        let f = invoiced()
        f.dispatch(.topUpPolled(topupId: "t1", result: st("paid")))
        guard case .endCardSession? = f.dispatch(.linkFailed(.unreachable)).first else { return XCTFail() }
        guard case .topUpFailed(let e) = f.state else { return XCTFail() }
        XCTAssertEqual(e.problem, .tillUnreachable); XCTAssertTrue(e.paid)
    }

    func testTheTillGivingUpHasNoRetry() {
        let f = invoiced()
        f.dispatch(.topUpPolled(topupId: "t1", result: st("failed", detail: "gateway refused")))
        guard case .topUpFailed(let e) = f.state else { return XCTFail() }
        XCTAssertEqual(e.problem, .failed)
        XCTAssertTrue(f.dispatch(.retryPressed).isEmpty)
    }

    func testStaleLoadRepliesAreIgnored() {
        let f = invoiced()
        let lid = loading(f)
        f.dispatch(.topUpLoadDone(loadId: lid + 99, result: st("loaded", txid: "x")))
        guard case .topUpLoading = f.state else { return XCTFail() }
        XCTAssertTrue(f.dispatch(.cancelPressed).isEmpty) // not while the card is being loaded
    }

    func testOnlyFromTheBalanceScreen() {
        let f = PosFlow()
        f.dispatch(.topUpPressed); f.dispatch(.chargeCardPressed)
        XCTAssertEqual(f.state, .entering)
    }

    func testTimeLeftAndQrText() {
        XCTAssertEqual(timeLeft(expiresAtMs: 600_000, nowMs: 0), "10:00")
        XCTAssertEqual(timeLeft(expiresAtMs: 1_000, nowMs: 1), "0:01")
        XCTAssertEqual(timeLeft(expiresAtMs: 1_000, nowMs: 5_000), "0:00")
        XCTAssertEqual(invoiceQrText("lnbc1x"), "LIGHTNING:LNBC1X")
        XCTAssertNotNil(InvoiceQR.image(invoiceQrText("lnbc1x")))
    }
}

/// Waiting top-ups (docs/TOPUP.md § Waiting top-ups).
final class WaitingTopUpFlowTests: XCTestCase {
    private let w1 = WaitingTopUp(topupId: "w1", amountMsat: 100_352, forCard: nil, createdAtMs: 1)
    private let w2 = WaitingTopUp(topupId: "w2", amountMsat: 65_536, forCard: "6f94b19b", createdAtMs: 2)

    /// The balance read for card 6f94b19b, and the read id its fetch carries.
    private func shownAsking() -> (PosFlow, Int) {
        let f = PosFlow { "ref" }
        f.dispatch(.cardBalanceOpened)
        guard case .startBalanceRead(let rid)? = f.dispatch(.cardPresent).first else { XCTFail("no read"); return (f, -1) }
        let fx = f.dispatch(.balanceDone(readId: rid, result: .read(CardBalance(liveMsat: 5_000, notes: [], freeSlots: [], shortId: "6f94b19b"))))
        guard case .fetchWaiting(let frid, let sid)? = fx.first else { XCTFail("no fetch"); return (f, -1) }
        XCTAssertEqual(frid, rid); XCTAssertEqual(sid, "6f94b19b")
        return (f, rid)
    }

    func testABalanceReadAsksWithoutDelayingTheBalance() {
        let (f, rid) = shownAsking()
        XCTAssertEqual(f.state, .balanceShown(readId: rid, liveMsat: 5_000))
        f.dispatch(.waitingFetched(readId: rid, topUps: [w1, w2]))
        XCTAssertEqual(f.state, .balanceShown(readId: rid, liveMsat: 5_000, waiting: w1)) // the oldest
    }

    func testAReadWithoutACardIdAsksForNothing() {
        let f = PosFlow { "ref" }
        f.dispatch(.cardBalanceOpened)
        guard case .startBalanceRead(let rid)? = f.dispatch(.cardPresent).first else { return XCTFail() }
        let fx = f.dispatch(.balanceDone(readId: rid, result: .read(CardBalance(liveMsat: 5_000, notes: [], freeSlots: [], shortId: nil))))
        XCTAssertTrue(fx.isEmpty)
    }

    func testNoneOrAFailedFetchShowsNoButtonAndStaleRepliesAreIgnored() {
        let (f, rid) = shownAsking()
        f.dispatch(.waitingFetched(readId: rid, topUps: nil))
        XCTAssertEqual(f.state, .balanceShown(readId: rid, liveMsat: 5_000))
        f.dispatch(.waitingFetched(readId: rid, topUps: []))
        XCTAssertEqual(f.state, .balanceShown(readId: rid, liveMsat: 5_000))
        f.dispatch(.waitingFetched(readId: rid + 99, topUps: [w1]))
        XCTAssertEqual(f.state, .balanceShown(readId: rid, liveMsat: 5_000))
        f.dispatch(.donePressed)
        f.dispatch(.waitingFetched(readId: rid, topUps: [w1]))
        XCTAssertEqual(f.state, .entering)
        XCTAssertTrue(f.dispatch(.loadWaitingPressed).isEmpty)
    }

    func testLoadingAWaitingTopUpTakesThePaidPath() {
        let (f, rid) = shownAsking()
        f.dispatch(.waitingFetched(readId: rid, topUps: [w1]))
        guard case .startCardSession(.topUp)? = f.dispatch(.loadWaitingPressed).first else { return XCTFail("no NFC") }
        guard case .topUpWaitingForCard(let t) = f.state else { return XCTFail("\(f.state)") }
        XCTAssertEqual(t.topupId, "w1"); XCTAssertEqual(t.amountMsat, 100_352)
        guard case .loadTopUp(let lid, let tid)? = f.dispatch(.cardPresent).first else { return XCTFail("no load") }
        XCTAssertEqual(tid, "w1")
        f.dispatch(.topUpLoadDone(loadId: lid, result: .ok(TopUpStatus(topupId: "w1", amountMsat: 100_352, state: "loaded", loadTxid: "tx", detail: nil))))
        XCTAssertEqual(f.state, .topUpLoaded(amountMsat: 100_352, loadTxid: "tx"))
    }

    func testAFullCardKeepsTheWaitingTopUpPaid() {
        let (f, rid) = shownAsking()
        f.dispatch(.waitingFetched(readId: rid, topUps: [w2]))
        f.dispatch(.loadWaitingPressed)
        guard case .loadTopUp(let lid, _)? = f.dispatch(.cardPresent).first else { return XCTFail() }
        f.dispatch(.topUpLoadDone(loadId: lid, result: .refused(error: "card_full", detail: "", httpStatus: 409)))
        guard case .topUpFailed(let fl) = f.state else { return XCTFail() }
        XCTAssertEqual(fl.problem, .cardFull); XCTAssertTrue(fl.paid)
        guard case .startCardSession(.topUp)? = f.dispatch(.retryPressed).first else { return XCTFail("no retry tap") }
    }
}

final class TopUpClientTests: XCTestCase {
    let pairing = TillPairing(baseURL: "http://10.0.0.2:9877", token: "deadbeef")
    private func client(_ status: Int, _ body: String) -> (TillClient, MockTransport) {
        let t = MockTransport()
        t.reply = .success(RawResponse(status: status, body: Data(body.utf8)))
        return (TillClient(pairing: { self.pairing }, transport: t), t)
    }

    func testCreateTopUpOk() async throws {
        let (c, t) = client(200, #"{"ok":true,"topup_id":"t1","amount_msat":100352,"invoice":"lnbc1","expires_at_ms":99,"state":"waiting"}"#)
        let r = await c.createTopUp(amountMsat: 100_000, clientRef: "r1")
        XCTAssertEqual(r, .ok(TopUp(topupId: "t1", amountMsat: 100_352, invoice: "lnbc1", expiresAtMs: 99, state: "waiting")))
        let req = try XCTUnwrap(t.requests.first)
        XCTAssertEqual(req.method, "POST"); XCTAssertEqual(req.url.absoluteString, "http://10.0.0.2:9877/v1/topup")
        XCTAssertEqual(req.headers["Authorization"], "Bearer deadbeef")
        let o = try JSONSerialization.jsonObject(with: try XCTUnwrap(req.body)) as! [String: Any]
        XCTAssertEqual(o["amount_msat"] as? Int, 100_000); XCTAssertEqual(o["client_ref"] as? String, "r1")
    }

    func testCreateTopUpSendsTheCard() async throws {
        let ok = #"{"ok":true,"topup_id":"t1","amount_msat":98304,"invoice":"lnbc1","expires_at_ms":99,"state":"waiting"}"#
        let (c, t) = client(200, ok)
        _ = await c.createTopUp(amountMsat: 100_000, clientRef: "r1", shortId: "6f94b19b")
        let o = try JSONSerialization.jsonObject(with: try XCTUnwrap(t.requests.first?.body)) as! [String: Any]
        XCTAssertEqual(o["short_id"] as? String, "6f94b19b")
        let (c2, t2) = client(200, ok)
        _ = await c2.createTopUp(amountMsat: 100_000, clientRef: "r1")
        let o2 = try JSONSerialization.jsonObject(with: try XCTUnwrap(t2.requests.first?.body)) as! [String: Any]
        XCTAssertNil(o2["short_id"])
    }

    func testCardBalanceShortId() {
        guard case .read(let b) = TillParse.cardBalance(status: 200, body: Data(#"{"ok":true,"live_msat":5,"notes":[],"free_slots":[1,4],"short_id":"6f94b19b"}"#.utf8)) else { return XCTFail() }
        XCTAssertEqual(b.shortId, "6f94b19b")
        guard case .read(let n) = TillParse.cardBalance(status: 200, body: Data(#"{"ok":true,"live_msat":5,"notes":[],"free_slots":[]}"#.utf8)) else { return XCTFail() }
        XCTAssertNil(n.shortId)
    }

    func testCreateTopUpRefusedAndBad() async {
        let (c, _) = client(400, #"{"ok":false,"error":"bad_amount","detail":"too much"}"#)
        let r = await c.createTopUp(amountMsat: 1, clientRef: "r")
        XCTAssertEqual(r, .refused(error: "bad_amount", detail: "too much", httpStatus: 400))
        XCTAssertEqual(topUpProblem(r), .badAmount)
        let (c2, _) = client(200, #"{"topup_id":"t1","amount_msat":1}"#)
        guard case .failed(.badResponse) = await c2.createTopUp(amountMsat: 1, clientRef: "r") else { return XCTFail() }
    }

    func testCreateTopUpUnreachable() async {
        let t = MockTransport(); t.reply = .failure(.unreachable)
        let r = await TillClient(pairing: { self.pairing }, transport: t).createTopUp(amountMsat: 1, clientRef: "r")
        XCTAssertEqual(r, .failed(.unreachable)); XCTAssertEqual(topUpProblem(r), .tillUnreachable)
    }

    func testTopUpStatus() async throws {
        let (c, t) = client(200, #"{"topup_id":"t 1","amount_msat":5,"state":"paid","load_txid":null}"#)
        let r = await c.topUpStatus("t 1")
        XCTAssertEqual(r, .ok(TopUpStatus(topupId: "t 1", amountMsat: 5, state: "paid", loadTxid: nil, detail: nil)))
        let req = try XCTUnwrap(t.requests.first)
        XCTAssertEqual(req.method, "GET"); XCTAssertEqual(req.url.absoluteString, "http://10.0.0.2:9877/v1/topup/t%201")
    }

    func testLoadSendsTheLinkAndParses() async throws {
        let (c, t) = client(200, #"{"topup_id":"t1","amount_msat":5,"state":"loaded","load_txid":"abc"}"#)
        let r = await c.loadTopUp("t1", cardLink: "L9")
        guard case .ok(let s) = r else { return XCTFail("\(r)") }
        XCTAssertEqual(s.loadTxid, "abc")
        let req = try XCTUnwrap(t.requests.first)
        XCTAssertEqual(req.url.absoluteString, "http://10.0.0.2:9877/v1/topup/t1/load")
        XCTAssertEqual(req.headers["X-Card-Link"], "L9"); XCTAssertGreaterThanOrEqual(req.timeout, 120)
    }

    func testLoadErrorsSort() async {
        func p(_ code: String, _ s: Int = 409, _ d: String = "") async -> TopUpProblem {
            let (c, _) = client(s, #"{"ok":false,"error":"\#(code)","detail":"\#(d)"}"#)
            return topUpProblem(await c.loadTopUp("t1", cardLink: nil))
        }
        let full = await p("card_full"); XCTAssertEqual(full, .cardFull)
        let exp = await p("expired", 410); XCTAssertEqual(exp, .expired)
        let gone = await p("card_link_gone"); XCTAssertEqual(gone, .cardMoved)
        let lost = await p("card_lost"); XCTAssertEqual(lost, .cardMoved)
        let other = await p("card_error", 502, "a different card"); XCTAssertEqual(other, .notOurCard)
        let ops = await p("ops_not_connected", 503); XCTAssertEqual(ops, .cardService)
        let notPaid = await p("not_paid"); XCTAssertEqual(notPaid, .other)
    }

    func testWaitingTopUpsParsesAndAsksForTheCard() async throws {
        let (c, t) = client(200, #"{"ok":true,"topups":[{"topup_id":"w1","amount_msat":100352,"for_card":null,"created_at_ms":7},{"topup_id":"w2","amount_msat":65536,"for_card":"6f94b19b","created_at_ms":9}]}"#)
        let r = await c.waitingTopUps(shortId: "6f94b19b")
        XCTAssertEqual(r, [WaitingTopUp(topupId: "w1", amountMsat: 100_352, forCard: nil, createdAtMs: 7),
                           WaitingTopUp(topupId: "w2", amountMsat: 65_536, forCard: "6f94b19b", createdAtMs: 9)])
        let req = try XCTUnwrap(t.requests.first)
        XCTAssertEqual(req.method, "GET"); XCTAssertEqual(req.url.absoluteString, "http://10.0.0.2:9877/v1/topups/waiting?short_id=6f94b19b")
    }

    func testWaitingTopUpsIsNilOnAnyError() async {
        let notFound = client(404, #"{"ok":false,"error":"not_found","detail":""}"#).0
        let r404 = await notFound.waitingTopUps(shortId: "x")
        XCTAssertNil(r404)
        let rJunk = await client(200, "not json").0.waitingTopUps(shortId: "x")
        XCTAssertNil(rJunk)
        let rNoList = await client(200, #"{"ok":true}"#).0.waitingTopUps(shortId: "x")
        XCTAssertNil(rNoList)
        let rEmpty = await client(200, #"{"ok":true,"topups":[]}"#).0.waitingTopUps(shortId: "x")
        XCTAssertEqual(rEmpty, [])
    }
}
