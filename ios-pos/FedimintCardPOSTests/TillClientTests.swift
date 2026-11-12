import XCTest
@testable import FedimintCardPOS

final class MockTransport: HTTPTransport {
    var reply: Result<RawResponse, TransportError> = .success(RawResponse(status: 200, body: Data("{}".utf8)))
    private(set) var requests: [HTTPRequest] = []
    func send(_ req: HTTPRequest) async throws -> RawResponse {
        requests.append(req)
        return try reply.get()
    }
}

private func body(_ s: String) -> Data { Data(s.utf8) }

final class TillClientTests: XCTestCase {
    let pairing = TillPairing(baseURL: "http://10.0.0.2:9877", token: "deadbeef")

    func testChargeSendsBearerLinkAndPinThenWipesPin() async throws {
        let t = MockTransport()
        t.reply = .success(RawResponse(status: 200, body: body(#"{"ok":true,"sale_id":7,"txid":"abcdef0123456789","price_msat":5000,"received_msat":5000,"overpaid_msat":0,"fees_msat":0,"inputs":[{"slot":0,"msat":8000}],"change":[{"slot":1,"msat":3000}],"collection_pending":false,"warnings":[],"duration_ms":900}"#)))
        let c = TillClient(pairing: { self.pairing }, transport: t)
        let pad = PinPad(); for d in [2, 4, 6, 8] { pad.press(d) }
        let pin = pad.take()!
        let r = await c.charge(priceMsat: 5000, clientRef: "ref-1", pin: pin, cardLink: "0123456789abcdef")
        XCTAssertTrue(pin.isWiped)
        guard case .success(let rc) = r else { return XCTFail("\(r)") }
        XCTAssertEqual(rc.saleId, 7); XCTAssertEqual(rc.change, [Note(slot: 1, msat: 3000)])
        let req = try XCTUnwrap(t.requests.first)
        XCTAssertEqual(req.method, "POST"); XCTAssertEqual(req.url.absoluteString, "http://10.0.0.2:9877/v1/charge")
        XCTAssertEqual(req.headers["Authorization"], "Bearer deadbeef")
        XCTAssertEqual(req.headers["X-Card-Link"], "0123456789abcdef")
        XCTAssertGreaterThanOrEqual(req.timeout, 120)
        let o = try JSONSerialization.jsonObject(with: try XCTUnwrap(req.body)) as! [String: Any]
        XCTAssertEqual(o["price_msat"] as? Int, 5000); XCTAssertEqual(o["client_ref"] as? String, "ref-1"); XCTAssertEqual(o["pin"] as? String, "2468")
    }

    func testChargeBodyWithoutPinAndEscaping() throws {
        let d = TillClient.chargeBody(priceMsat: 1, clientRef: "a\"b\\c", pin: nil)
        let o = try JSONSerialization.jsonObject(with: d) as! [String: Any]
        XCTAssertEqual(o["client_ref"] as? String, "a\"b\\c"); XCTAssertNil(o["pin"])
    }

    func testBalanceUsesLink() async {
        let t = MockTransport()
        t.reply = .success(RawResponse(status: 200, body: body(#"{"ok":true,"live_msat":21000,"notes":[{"slot":0,"msat":21000,"status":"live"}],"free_slots":[1,2]}"#)))
        let c = TillClient(pairing: { self.pairing }, transport: t)
        let r = await c.cardBalance(cardLink: "ffff")
        XCTAssertEqual(r, .read(CardBalance(liveMsat: 21000, notes: [CardNote(slot: 0, msat: 21000, status: "live")], freeSlots: [1, 2])))
        XCTAssertEqual(t.requests.first?.headers["X-Card-Link"], "ffff")
        XCTAssertEqual(t.requests.first?.url.path, "/v1/card/balance")
    }

    func testNotPairedSendsNothing() async {
        let t = MockTransport()
        let c = TillClient(pairing: { nil }, transport: t)
        let pin = Pin(bytes: Array("1234".utf8))
        let r = await c.charge(priceMsat: 1, clientRef: "r", pin: pin, cardLink: nil)
        XCTAssertEqual(r, .failed(.notPaired)); XCTAssertTrue(pin.isWiped); XCTAssertTrue(t.requests.isEmpty)
    }

    func testTransportFailuresMap() async {
        let t = MockTransport()
        let c = TillClient(pairing: { self.pairing }, transport: t)
        t.reply = .failure(.timeout)
        let r1 = await c.charge(priceMsat: 1, clientRef: "r", pin: nil, cardLink: nil)
        XCTAssertEqual(r1, .failed(.timeout))
        t.reply = .failure(.unreachable)
        let r2 = await c.status()
        XCTAssertEqual(r2, .err(.unreachable))
        t.reply = .failure(.interrupted)
        let r3 = await c.charge(priceMsat: 1, clientRef: "r", pin: nil, cardLink: nil)
        XCTAssertEqual(r3, .failed(.interrupted))
    }

    func testStatusAndUnauthorized() async {
        let t = MockTransport()
        let c = TillClient(pairing: { self.pairing }, transport: t)
        t.reply = .success(RawResponse(status: 200, body: body(#"{"federation_id":"ab","federation_name":"Island","wallet_balance_msat":123000,"card_relay_ok":false,"busy":false}"#)))
        let s = await c.status()
        XCTAssertEqual(s, .ok(TillStatus(federationId: "ab", federationName: "Island", walletBalanceMsat: 123000, cardRelayOk: false, busy: false)))
        XCTAssertEqual(t.requests.last?.headers["Authorization"], "Bearer deadbeef")
        t.reply = .success(RawResponse(status: 401, body: body(#"{"ok":false,"error":"unauthorized"}"#)))
        let s2 = await c.status()
        XCTAssertEqual(s2, .err(.unauthorized))
        let s3 = await c.charge(priceMsat: 1, clientRef: "r", pin: nil, cardLink: "x")
        XCTAssertEqual(s3, .declined(code: .unauthorized, detail: "", httpStatus: 401))
    }

    func testStatusReadsTheOfflineQueue() {
        let r = TillParse.status(body(#"{"federation_id":"ab","federation_name":null,"wallet_balance_msat":1,"card_relay_ok":true,"busy":false,"offline":{"queued":3,"queued_msat":4500000,"double_spent":1,"cap_msat":100000000,"cap_count":20}}"#))
        guard case .ok(let s) = r else { return XCTFail() }
        XCTAssertEqual(s.offlineQueued, 3); XCTAssertEqual(s.offlineQueuedMsat, 4_500_000); XCTAssertEqual(s.offlineDoubleSpent, 1)
        XCTAssertEqual(waitingToSyncText(s.offlineQueued), "3 payments waiting to sync")
        XCTAssertEqual(waitingToSyncText(1), "1 payment waiting to sync")
        XCTAssertNil(waitingToSyncText(0))
    }

    func testStatusWithoutOrWithOddOfflineFieldsIsNothingQueued() {
        let base = #""federation_id":"ab","wallet_balance_msat":1,"card_relay_ok":true,"busy":false"#
        for extra in ["", #","offline":null"#, #","offline":true"#, #","offline":{}"#, #","offline":{"queued":"many"}"#,
                      #","offline":{"double_spent":null}"#, #","offline":{"double_spent":"2"}"#] {
            guard case .ok(let s) = TillParse.status(body("{" + base + extra + "}")) else { return XCTFail(extra) }
            XCTAssertEqual(s.offlineQueued, 0, extra); XCTAssertNil(s.offlineDoubleSpent, extra)
        }
    }

    func testChargeAnsweredOfflineIsMarkedAndSaysPaidOffline() {
        guard case .success(let rc) = TillParse.charge(status: 200, body: body(#"{"ok":true,"offline":true,"txid":"t","price_msat":5000,"received_msat":5000}"#)) else { return XCTFail() }
        XCTAssertTrue(rc.offline)
        XCTAssertEqual(receiptHeadline(rc), "Paid offline · 5 sats")
        guard case .success(let on) = TillParse.charge(status: 200, body: body(#"{"ok":true,"txid":"t","price_msat":5000}"#)) else { return XCTFail() }
        XCTAssertFalse(on.offline)
        XCTAssertEqual(receiptHeadline(on), "Paid 5 sats")
        guard case .success(let no) = TillParse.charge(status: 200, body: body(#"{"ok":true,"offline":false,"price_msat":5000}"#)) else { return XCTFail() }
        XCTAssertFalse(no.offline)
    }

    func testSalesParse() {
        let r = TillParse.sales(body(#"{"count":1,"total_price_msat":5000,"total_received_msat":5000,"sales":[{"id":1,"ts_unix_ms":1700000000000,"price_msat":5000,"received_msat":5000,"overpaid_msat":0,"fees_msat":0,"txid":"t","inputs":[],"change":[],"client_ref":null}]}"#))
        guard case .ok(let l) = r else { return XCTFail() }
        XCTAssertEqual(l.sales.first?.clientRef, nil); XCTAssertEqual(l.count, 1)
    }
}

final class ErrorMappingTests: XCTestCase {
    func testEveryWireCodeParses() {
        for c in ChargeErrorCode.allCases where c != .unknown {
            let r = TillParse.charge(status: 409, body: Data(#"{"ok":false,"error":"\#(c.rawValue)","detail":"x"}"#.utf8))
            XCTAssertEqual(r, .declined(code: c, detail: "x", httpStatus: 409))
            XCTAssertEqual(ErrorKind.from(c) == .unknown, false, c.rawValue)
        }
        XCTAssertEqual(ChargeErrorCode.fromWire("nonsense"), .unknown)
        XCTAssertEqual(ChargeErrorCode.fromWire(nil), .unknown)
    }

    func testNoRoomForChangeOfflineCarriesTheSuggestion() {
        let body = #"{"ok":false,"error":"offline_no_change_room","detail":"d","suggested_msat":12000,"suggest_below_msat":12000,"suggest_above_msat":null}"#
        XCTAssertEqual(TillParse.charge(status: 409, body: Data(body.utf8)),
                       .declined(code: .offlineNoChangeRoom, detail: "d", httpStatus: 409, suggestedMsat: 12_000))
        let none = #"{"ok":false,"error":"offline_no_change_room","detail":"d","suggested_msat":null}"#
        XCTAssertEqual(TillParse.charge(status: 409, body: Data(none.utf8)), .declined(code: .offlineNoChangeRoom, detail: "d", httpStatus: 409))
    }

    func testUnreadableBodies() {
        XCTAssertEqual(TillParse.charge(status: 202, body: Data("x".utf8)), .declined(code: .undecided, detail: "", httpStatus: 202))
        XCTAssertEqual(TillParse.charge(status: 200, body: Data("x".utf8)), .failed(.interrupted))
        XCTAssertEqual(TillParse.charge(status: 500, body: Data("x".utf8)), .failed(.badResponse("bad json")))
    }

    func testTriesLeft() {
        XCTAssertEqual(parseTriesLeft("wrong PIN; 2 tries left before the card blocks"), 2)
        XCTAssertEqual(parseTriesLeft("1 try left"), 1)
        XCTAssertNil(parseTriesLeft("wrong PIN"))
        XCTAssertEqual(wrongPinCopy(1), "Wrong PIN — 1 try left")
        XCTAssertEqual(wrongPinCopy(3), "Wrong PIN — 3 tries left")
        XCTAssertEqual(wrongPinCopy(nil), "Wrong PIN")
    }

    func testKinds() {
        XCTAssertTrue(ErrorKind.undecided.needsSalesCheck); XCTAssertTrue(ErrorKind.timeoutUnknown.needsSalesCheck)
        for k: ErrorKind in [.pinRequired, .cardChanged, .unknownCard, .opsUnreachable, .opsRefused, .cardLinkGone, .unauthorized] {
            XCTAssertFalse(k.definitive, "\(k) proves nothing about an earlier attempt")
        }
        XCTAssertTrue(ErrorKind.wrongPin.definitive); XCTAssertTrue(ErrorKind.insufficient.definitive)
        XCTAssertFalse(ErrorKind.pinBlocked.retryable); XCTAssertTrue(ErrorKind.busy.retryable)
        XCTAssertEqual(ErrorKind.from(TillError.unreachable), .tillUnreachable)
        XCTAssertEqual(ErrorKind.from(TillError.interrupted), .timeoutUnknown)
    }

    func testBalanceProblems() {
        XCTAssertEqual(balanceProblem(.refused(code: .cardError, detail: "different card presented")), .notOurCard)
        XCTAssertEqual(balanceProblem(.refused(code: .cardError, detail: "SW 6A82")), .notOurCard)
        XCTAssertEqual(balanceProblem(.refused(code: .cardError, detail: "tag lost")), .cardMoved)
        XCTAssertEqual(balanceProblem(.refused(code: .cardLinkGone, detail: "")), .cardMoved)
        XCTAssertEqual(balanceProblem(.refused(code: .busy, detail: "")), .busy)
        XCTAssertEqual(balanceProblem(.refused(code: .opsRefused, detail: "")), .cardService)
        XCTAssertEqual(balanceProblem(.refused(code: .unauthorized, detail: "")), .notPaired)
        XCTAssertEqual(balanceProblem(.failed(.timeout)), .timeout)
        XCTAssertEqual(balanceProblem(.failed(.unreachable)), .tillUnreachable)
    }

    func testCopyIsPlainAndNeverEmptyTitle() {
        let kinds: [ErrorKind] = [.badRequest, .busy, .noCard, .relayUnreachable, .cardError, .insufficient, .noExactPlan, .declined,
                                  .undecided, .recovering, .conflict, .pinRequired, .wrongPin, .pinBlocked, .cardChanged, .unknownCard,
                                  .opsUnreachable, .opsRefused, .cardLinkGone, .unauthorized, .internal, .unknown, .tillUnreachable,
                                  .timeoutUnknown, .notPaired, .offlineNoChangeRoom]
        for k in kinds {
            let (t, d) = errorCopy(k, "")
            XCTAssertFalse(t.isEmpty)
            XCTAssertFalse((t + d).lowercased().contains("adb"), "\(k) mentions adb")
        }
        XCTAssertEqual(errorCopy(.pinBlocked, "").0, "PIN blocked")
        XCTAssertEqual(errorCopy(.insufficient, "").0, "Card balance too low")
        // Offline with no slot for change is not a full card (OFFLINE-PLAN.md § 10).
        XCTAssertEqual(errorCopy(.noExactPlan, "").0, "Card needs a top up")
        XCTAssertEqual(errorCopy(.offlineNoChangeRoom, "d", suggestedMsat: 12_000).0, "No room for change offline")
        XCTAssertEqual(errorCopy(.offlineNoChangeRoom, "d", suggestedMsat: 12_000).1, "This card can't take change while offline. Try 12 sats")
        XCTAssertEqual(errorCopy(.offlineNoChangeRoom, "d").1, "This card can't take change while offline. Try another amount")
        for p: BalanceProblem in [.cardMoved, .notOurCard, .tillUnreachable, .relayUnreachable, .busy, .cardService, .timeout, .notPaired, .other] {
            XCTAssertFalse(balanceErrorCopy(p).0.isEmpty)
        }
    }
}

/// The phone's offline routes (docs/TILL-API.md): the kit and the sync.
final class OfflineRoutesTests: XCTestCase {
    let pairing = TillPairing(baseURL: "http://10.0.0.2:9877", token: "deadbeef")

    func testTheKitIsAskedForThisPhone() async throws {
        let t = MockTransport()
        t.reply = .success(RawResponse(status: 200, body: Data(#"{"ok":true,"kit":{}}"#.utf8)))
        let c = TillClient(pairing: { self.pairing }, transport: t)
        let r = await c.offlineKit(phoneId: "0123456789abcdef0123456789abcdef")
        XCTAssertEqual(r, .ok(Data(#"{"ok":true,"kit":{}}"#.utf8)), "the answer as it is: the core takes it whole")
        let req = try XCTUnwrap(t.requests.first)
        XCTAssertEqual(req.method, "GET")
        XCTAssertEqual(req.url.absoluteString, "http://10.0.0.2:9877/v1/offline/kit?phone=0123456789abcdef0123456789abcdef")
        XCTAssertEqual(req.headers["Authorization"], "Bearer deadbeef")
        XCTAssertNil(req.body)
        XCTAssertEqual(req.timeout, 30)
    }

    func testTheSyncPostsTheDrainedBodyAsItIs() async throws {
        let t = MockTransport()
        t.reply = .success(RawResponse(status: 200, body: Data(#"{"ok":true,"results":[]}"#.utf8)))
        let c = TillClient(pairing: { self.pairing }, transport: t)
        let body = #"{"phone_id":"ab","charges":[{"txid":"t1"}]}"#
        let r = await c.postOfflineCharges(body)
        XCTAssertEqual(r, .ok(Data(#"{"ok":true,"results":[]}"#.utf8)))
        let req = try XCTUnwrap(t.requests.first)
        XCTAssertEqual(req.method, "POST")
        XCTAssertEqual(req.url.path, "/v1/offline/charges")
        XCTAssertEqual(req.headers["Content-Type"], "application/json")
        XCTAssertEqual(req.headers["Authorization"], "Bearer deadbeef")
        XCTAssertEqual(req.body, Data(body.utf8))
        XCTAssertNil(req.headers["X-Card-Link"])
    }

    func testOnlyA200IsAnAnswer() async {
        let t = MockTransport()
        let c = TillClient(pairing: { self.pairing }, transport: t)
        let cases: [(Result<RawResponse, TransportError>, TillResult<Data>)] = [
            (.success(RawResponse(status: 401, body: Data())), .err(.unauthorized)),
            (.success(RawResponse(status: 403, body: Data())), .err(.unauthorized)),
            (.success(RawResponse(status: 503, body: Data(#"{"ok":false,"error":"offline_not_ready"}"#.utf8))), .err(.badResponse("HTTP 503"))),
            (.success(RawResponse(status: 413, body: Data())), .err(.badResponse("HTTP 413"))),
            (.success(RawResponse(status: 202, body: Data())), .err(.badResponse("HTTP 202"))),
            (.failure(.unreachable), .err(.unreachable)),
            (.failure(.timeout), .err(.timeout)),
        ]
        for (reply, want) in cases {
            t.reply = reply
            let kit = await c.offlineKit(phoneId: "ab")
            XCTAssertEqual(kit, want, "\(reply)")
            let sync = await c.postOfflineCharges("{}")
            XCTAssertEqual(sync, want, "\(reply)")
        }
        let none = TillClient(pairing: { nil }, transport: t)
        let r = await none.offlineKit(phoneId: "ab")
        XCTAssertEqual(r, .err(.notPaired))
    }
}
