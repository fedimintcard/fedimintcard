import XCTest
@testable import FedimintCardPOS

final class PosFlowTests: XCTestCase {
    private var n = 0
    private func flow(_ sats: Int = 5) -> PosFlow {
        let f = PosFlow { self.n += 1; return "ref\(self.n)" }
        for c in String(sats) { f.pad.press(Int(String(c))!) }
        return f
    }
    private let receipt = ChargeReceipt(saleId: 1, txid: "t", priceMsat: 5000, receivedMsat: 5000, overpaidMsat: 0, feesMsat: 0,
                                        inputs: [], change: [], collectionPending: false, warnings: [], durationMs: 1)
    private func declined(_ c: ChargeErrorCode, _ d: String = "d") -> ChargeResult { .declined(code: c, detail: d, httpStatus: 409) }
    private func typePin(_ f: PosFlow, _ p: String = "1234") { for c in p { f.pinPad.press(Int(String(c))!) } }

    /// Charge, PIN, Pay, card tapped: returns the StartCharge effect.
    @discardableResult
    private func chargeToSigning(_ f: PosFlow, pin: String = "1234") -> (Int, String, Pin?) {
        if case .entering = f.state { f.dispatch(.chargePressed) }
        typePin(f, pin)
        let fx = f.dispatch(.pinConfirmed)
        guard case .startCardSession(.charge)? = fx.first else { XCTFail("no NFC session: \(fx)"); return (0, "", nil) }
        let fx2 = f.dispatch(.cardPresent)
        guard case .startCharge(let id, _, let ref, let pin)? = fx2.first else { XCTFail("no charge"); return (0, "", nil) }
        return (id, ref, pin)
    }

    private func signingId(_ f: PosFlow) -> Int { if case .signing(_, let id, _, _) = f.state { return id }; return -1 }

    func testHappyPath() {
        let f = flow()
        f.dispatch(.chargePressed)
        guard case .enteringPin(5000, "ref1", false, nil, nil) = f.state else { return XCTFail("\(f.state)") }
        typePin(f)
        let fx = f.dispatch(.pinConfirmed)
        XCTAssertEqual(f.state, .waitingForCard(priceMsat: 5000, clientRef: "ref1", returnTo: nil))
        guard case .startCardSession(.charge)? = fx.first else { return XCTFail() }
        XCTAssertTrue(f.holdsPin)
        let fx2 = f.dispatch(.cardPresent)
        guard case .startCharge(let id, 5000, "ref1", let pin?)? = fx2.first else { return XCTFail() }
        XCTAssertEqual(pin.withBytes { String(decoding: $0, as: UTF8.self) }, "1234")
        XCTAssertFalse(f.holdsPin, "the request owns the PIN once signing starts")
        f.dispatch(.chargeDone(chargeId: id, result: .success(receipt)))
        XCTAssertEqual(f.state, .receipt(receipt))
        f.dispatch(.newSalePressed)
        XCTAssertEqual(f.state, .entering); XCTAssertEqual(f.pad.sats, 0)
    }

    func testZeroAmountDoesNothing() {
        let f = PosFlow()
        XCTAssertTrue(f.dispatch(.chargePressed).isEmpty)
        XCTAssertEqual(f.state, .entering)
    }

    func testPinConfirmNeedsFourDigits() {
        let f = flow()
        f.dispatch(.chargePressed)
        typePin(f, "123")
        XCTAssertTrue(f.dispatch(.pinConfirmed).isEmpty)
        guard case .enteringPin = f.state else { return XCTFail() }
    }

    func testCancelPinWipesAndKeepsAmount() {
        let f = flow(42)
        f.dispatch(.chargePressed); typePin(f, "99")
        f.dispatch(.cancelPressed)
        XCTAssertEqual(f.state, .entering); XCTAssertEqual(f.pad.sats, 42)
        XCTAssertFalse(f.holdsPin); XCTAssertTrue(f.pinPad.bufferIsZeroed)
    }

    func testNfcCancelledWhileWaitingDropsPin() {
        let f = flow()
        f.dispatch(.chargePressed); typePin(f); f.dispatch(.pinConfirmed)
        XCTAssertTrue(f.holdsPin)
        f.dispatch(.cardSessionEnded)
        XCTAssertEqual(f.state, .entering); XCTAssertFalse(f.holdsPin); XCTAssertEqual(f.pad.sats, 5)
    }

    func testAppCancelWhileWaitingEndsSession() {
        let f = flow()
        f.dispatch(.chargePressed); typePin(f); f.dispatch(.pinConfirmed)
        guard case .endCardSession? = f.dispatch(.cancelPressed).first else { return XCTFail() }
        XCTAssertEqual(f.state, .entering)
    }

    func testBackgroundWipesPin() {
        let f = flow()
        f.dispatch(.chargePressed); typePin(f, "12")
        f.dispatch(.backgrounded)
        XCTAssertEqual(f.state, .entering); XCTAssertFalse(f.holdsPin)
        f.dispatch(.chargePressed); typePin(f); f.dispatch(.pinConfirmed)
        guard case .endCardSession? = f.dispatch(.backgrounded).first else { return XCTFail() }
        XCTAssertEqual(f.state, .entering); XCTAssertFalse(f.holdsPin)
    }

    func testWrongPinGoesBackToPadSameAmountSameRef() {
        let f = flow()
        let (id, ref, _) = chargeToSigning(f)
        f.dispatch(.chargeDone(chargeId: id, result: .declined(code: .wrongPin, detail: "wrong PIN; 2 tries left before the card blocks", httpStatus: 403)))
        XCTAssertEqual(f.state, .enteringPin(priceMsat: 5000, clientRef: ref, wrongPin: true, triesLeft: 2, returnTo: nil))
        XCTAssertFalse(f.holdsPin)
        let (_, ref2, pin2) = chargeToSigning(f, pin: "4321")
        XCTAssertEqual(ref2, ref, "a wrong-PIN retry resends the same client_ref")
        XCTAssertEqual(pin2?.withBytes { String(decoding: $0, as: UTF8.self) }, "4321")
    }

    func testWrongPinWithNoTriesLeftIsBlocked() {
        let f = flow()
        let (id, _, _) = chargeToSigning(f)
        f.dispatch(.chargeDone(chargeId: id, result: .declined(code: .wrongPin, detail: "wrong PIN; 0 tries left", httpStatus: 403)))
        guard case .error(let e) = f.state else { return XCTFail() }
        XCTAssertEqual(e.kind, .pinBlocked); XCTAssertFalse(e.kind.retryable)
    }

    func testPinBlocked() {
        let f = flow()
        let (id, _, _) = chargeToSigning(f)
        f.dispatch(.chargeDone(chargeId: id, result: .declined(code: .pinBlocked, detail: "", httpStatus: 423)))
        guard case .error(let e) = f.state else { return XCTFail() }
        XCTAssertEqual(e.kind, .pinBlocked); XCTAssertNil(e.unresolvedRef)
        f.dispatch(.retryPressed)
        guard case .error = f.state else { return XCTFail("blocked must not offer the PIN pad again") }
        f.dispatch(.backPressed)
        XCTAssertEqual(f.state, .entering)
    }

    func testStaleChargeDoneIgnored() {
        let f = flow()
        let (id, _, _) = chargeToSigning(f)
        f.dispatch(.chargeDone(chargeId: id + 7, result: .success(receipt)))
        XCTAssertEqual(signingId(f), id)
    }

    func testNoRoomForChangeOfflineKeepsTheSuggestionAndMovedNothing() {
        let f = flow()
        let (id, ref, _) = chargeToSigning(f)
        f.dispatch(.chargeDone(chargeId: id, result: .declined(code: .offlineNoChangeRoom, detail: "d", httpStatus: 409, suggestedMsat: 12_000)))
        guard case .error(let e) = f.state else { return XCTFail("\(f.state)") }
        XCTAssertEqual(e.kind, .offlineNoChangeRoom)
        XCTAssertEqual(e.suggestedMsat, 12_000)
        XCTAssertEqual(e.clientRef, ref)
        XCTAssertNil(e.unresolvedRef)
    }

    func testRetryableErrorKeepsRef() {
        let f = flow()
        let (id, ref, _) = chargeToSigning(f)
        f.dispatch(.chargeDone(chargeId: id, result: declined(.busy)))
        guard case .error(let e) = f.state else { return XCTFail() }
        XCTAssertEqual(e.kind, .busy); XCTAssertNil(e.unresolvedRef)
        f.dispatch(.retryPressed)
        guard case .enteringPin(_, ref, false, nil, nil) = f.state else { return XCTFail("\(f.state)") }
    }

    func testUndecidedIsStickyUntilSaleFound() {
        let f = flow()
        let (id, ref, _) = chargeToSigning(f)
        f.dispatch(.chargeDone(chargeId: id, result: .declined(code: .undecided, detail: "", httpStatus: 202)))
        XCTAssertEqual(f.unresolvedRef, ref)
        f.dispatch(.backPressed)
        guard case .error = f.state else { return XCTFail("back is not offered while unresolved") }
        // Try again: PIN, new NFC session, and ops-code failure keeps it unresolved.
        f.dispatch(.retryPressed)
        XCTAssertEqual(f.unresolvedRef, ref)
        let (id2, ref2, _) = chargeToSigning(f)
        XCTAssertEqual(ref2, ref)
        f.dispatch(.chargeDone(chargeId: id2, result: declined(.opsUnreachable)))
        XCTAssertEqual(f.unresolvedRef, ref)
        // Cancel from the NFC sheet on a retry goes back to the unresolved error, not the amount.
        f.dispatch(.retryPressed); typePin(f); f.dispatch(.pinConfirmed)
        f.dispatch(.cardSessionEnded)
        XCTAssertEqual(f.unresolvedRef, ref)
        f.dispatch(.saleFound(Sale(id: 9, tsUnixMs: 0, priceMsat: 5000, receivedMsat: 5000, overpaidMsat: 0, feesMsat: 0, txid: "x", clientRef: "other")))
        guard case .error = f.state else { return XCTFail() }
        f.dispatch(.saleFound(Sale(id: 9, tsUnixMs: 0, priceMsat: 5000, receivedMsat: 5000, overpaidMsat: 0, feesMsat: 0, txid: "x", clientRef: ref)))
        guard case .receipt(let r) = f.state else { return XCTFail() }
        XCTAssertEqual(r.saleId, 9)
    }

    func testTimeoutIsUnresolvedAndDefinitiveClears() {
        let f = flow()
        let (id, ref, _) = chargeToSigning(f)
        f.dispatch(.chargeDone(chargeId: id, result: .failed(.timeout)))
        guard case .error(let e) = f.state else { return XCTFail() }
        XCTAssertEqual(e.kind, .timeoutUnknown); XCTAssertEqual(e.unresolvedRef, ref)
        f.dispatch(.retryPressed)
        let (id2, _, _) = chargeToSigning(f)
        f.dispatch(.chargeDone(chargeId: id2, result: declined(.insufficient)))
        XCTAssertNil(f.unresolvedRef)
    }

    func testAbandonUnresolved() {
        let f = flow()
        let (id, _, _) = chargeToSigning(f)
        f.dispatch(.chargeDone(chargeId: id, result: .failed(.interrupted)))
        f.dispatch(.abandonPressed)
        XCTAssertEqual(f.state, .entering); XCTAssertEqual(f.pad.sats, 0)
    }

    func testLinkFailureBeforeChargeKeepsUnresolvedState() {
        let f = flow()
        f.dispatch(.chargePressed); typePin(f); f.dispatch(.pinConfirmed)
        f.dispatch(.linkFailed(.unauthorized))
        guard case .error(let e) = f.state else { return XCTFail() }
        XCTAssertEqual(e.kind, .unauthorized); XCTAssertNil(e.unresolvedRef); XCTAssertFalse(f.holdsPin)
        let g = flow()
        g.dispatch(.chargePressed); typePin(g); g.dispatch(.pinConfirmed)
        g.dispatch(.linkFailed(.notPaired))
        guard case .error(let e2) = g.state else { return XCTFail() }
        XCTAssertEqual(e2.kind, .notPaired)
    }

    func testSalesFromEntering() {
        let f = flow()
        f.dispatch(.salesOpened); XCTAssertEqual(f.state, .sales(returnTo: nil))
        f.dispatch(.backPressed); XCTAssertEqual(f.state, .entering)
    }

    // MARK: balance

    func testBalanceReadAndAutoReturnKeepsAmount() {
        let f = flow(321)
        guard case .startCardSession(.balance)? = f.dispatch(.cardBalanceOpened).first else { return XCTFail() }
        XCTAssertEqual(f.state, .balanceWaiting)
        guard case .startBalanceRead(let rid)? = f.dispatch(.cardPresent).first else { return XCTFail() }
        f.dispatch(.balanceDone(readId: rid, result: .read(CardBalance(liveMsat: 21_000, notes: [], freeSlots: []))))
        XCTAssertEqual(f.state, .balanceShown(readId: rid, liveMsat: 21_000))
        f.dispatch(.balanceExpired(readId: rid + 1))
        XCTAssertEqual(f.state, .balanceShown(readId: rid, liveMsat: 21_000), "a stale timer is ignored")
        f.dispatch(.balanceExpired(readId: rid))
        XCTAssertEqual(f.state, .entering)
        XCTAssertEqual(f.pad.sats, 321, "the typed amount survives a balance check")
    }

    func testBalanceDone() {
        let f = flow()
        f.dispatch(.cardBalanceOpened)
        guard case .startBalanceRead(let rid)? = f.dispatch(.cardPresent).first else { return XCTFail() }
        f.dispatch(.balanceDone(readId: rid, result: .read(CardBalance(liveMsat: 1, notes: [], freeSlots: []))))
        f.dispatch(.donePressed)
        XCTAssertEqual(f.state, .entering)
    }

    func testBalanceFailureAndRetry() {
        let f = flow()
        f.dispatch(.cardBalanceOpened)
        guard case .startBalanceRead(let rid)? = f.dispatch(.cardPresent).first else { return XCTFail() }
        f.dispatch(.balanceDone(readId: rid, result: .refused(code: .unknownCard, detail: "no such card")))
        XCTAssertEqual(f.state, .balanceFailed(.notOurCard, detail: "no such card"))
        guard case .startCardSession(.balance)? = f.dispatch(.retryPressed).first else { return XCTFail() }
        XCTAssertEqual(f.state, .balanceWaiting)
        f.dispatch(.cardSessionEnded)
        XCTAssertEqual(f.state, .entering)
    }

    func testBalanceLinkFailure() {
        let f = flow()
        f.dispatch(.cardBalanceOpened)
        f.dispatch(.linkFailed(.unreachable))
        XCTAssertEqual(f.state, .balanceFailed(.tillUnreachable, detail: ""))
    }

    func testBalanceNotOfferedMidSale() {
        let f = flow()
        f.dispatch(.chargePressed)
        XCTAssertTrue(f.dispatch(.cardBalanceOpened).isEmpty)
        guard case .enteringPin = f.state else { return XCTFail() }
    }
}
