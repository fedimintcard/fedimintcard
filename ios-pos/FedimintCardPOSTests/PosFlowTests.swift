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

/// The copy for the phone as an offline till (ENG-843, one set for both apps).
final class OfflineCopyTests: XCTestCase {
    func testEveryNewKindHasItsTitle() {
        let titles: [(ErrorKind, String)] = [
            (.offlineNoKit, "Till service unreachable"), (.offlineKitExpired, "Till service unreachable"),
            (.offlineCapReached, "Till service unreachable"), (.offlineNoteInvalid, "Till service unreachable"),
            (.offlinePoolExhausted, "Amount not available offline"), (.offlineUnsupportedCard, "This card pays online only"),
            (.cardNotGenuine, "Card declined"), (.cardLimit, "Card limit reached"), (.heldOnPhone, "Payment not finished"),
        ]
        for (k, title) in titles {
            let (t, d) = errorCopy(k, "")
            XCTAssertEqual(t, title, "\(k)")
            XCTAssertFalse(d.isEmpty, "\(k)")
        }
        XCTAssertEqual(errorCopy(.offlineNoteInvalid, "").1, "This card can't be checked without the till. It pays online only.")
        XCTAssertEqual(errorCopy(.offlineUnsupportedCard, "").1, "It can't be taken without the till.")
        XCTAssertEqual(errorCopy(.cardNotGenuine, "").1, "This isn't a genuine Fedimint Card.")
        XCTAssertEqual(errorCopy(.cardLimit, "").1, "This card has reached its daily limit.")
    }

    func testOneCopyForATillOutOfReach() {
        XCTAssertEqual(errorCopy(.tillUnreachable, "").0, "Till service unreachable")
        XCTAssertTrue(errorCopy(.tillUnreachable, "") == balanceErrorCopy(.tillUnreachable), "the sale and the balance say the same")
        XCTAssertTrue(errorCopy(.tillUnreachable, "") == tillUnreachableCopy)
        XCTAssertEqual(errorCopy(.tillUnreachable, "", unresolved: true).1,
                       "The last reply was lost, so this sale can't be taken on the phone. Try again once the till answers.")
        XCTAssertEqual(errorCopy(.tillUnreachable, "", unresolved: true).0, "Till service unreachable")
    }

    func testPoolExhaustedQuotesThePriceTheKitPays() {
        XCTAssertEqual(errorCopy(.offlinePoolExhausted, "d", suggestedMsat: 12_000).1, "Try 12 sats, or reach the till for a new kit.")
        XCTAssertEqual(errorCopy(.offlinePoolExhausted, "d").1, "Try another amount, or reach the till for a new kit.")
        XCTAssertEqual(errorCopy(.offlinePoolExhausted, "d", suggestedMsat: 1_234_500).1, "Try 1,234.5 sats, or reach the till for a new kit.")
    }

    func testTheAbandonAlertSaysAHeldSaleStillFinishes() {
        // Held: the sale's client_ref is in the phone's localOnlyRefs, whatever its screen's kind
        // (OfflineFlowTests drive both from the flow).
        XCTAssertEqual(abandonCopy(held: true), "The card already signed part of this payment. Its next tap on this phone still finishes it.")
        XCTAssertEqual(abandonCopy(held: false), "If the payment did go through, the customer will have paid without a receipt. Check Sales first.")
    }

    func testTheNewKindsHold() {
        XCTAssertTrue(ErrorKind.heldOnPhone.needsSalesCheck, "a held sale stays unresolved")
        let new: [ErrorKind] = [.offlineNoKit, .offlineKitExpired, .offlineCapReached, .offlineNoteInvalid, .offlinePoolExhausted,
                                .offlineUnsupportedCard, .cardNotGenuine, .cardLimit, .heldOnPhone]
        for k in new { XCTAssertFalse(k.definitive, "\(k) proves nothing about an earlier attempt") }
        for k: ErrorKind in [.offlineUnsupportedCard, .cardNotGenuine, .cardLimit] { XCTAssertFalse(k.retryable, "\(k)") }
        for k: ErrorKind in [.offlineNoKit, .offlineKitExpired, .offlineCapReached, .offlineNoteInvalid, .offlinePoolExhausted, .heldOnPhone] {
            XCTAssertTrue(k.retryable, "\(k)")
            XCTAssertTrue(k.phoneOnly, "\(k)")
        }
        XCTAssertEqual(ErrorKind.from(ChargeErrorCode.offlineNotReady), .offlineNoKit)
    }

    func testTheWaitingCountAddsThePhonesToTheTills() {
        let till = TillStatus(federationId: "f", federationName: nil, walletBalanceMsat: 0, cardRelayOk: true, busy: false, offlineQueued: 2)
        XCTAssertEqual(waitingToSyncText(till: till, phone: OfflineStats(waiting: 3)), "5 payments waiting to sync")
        XCTAssertEqual(waitingToSyncText(till: nil, phone: OfflineStats(waiting: 1)), "1 payment waiting to sync", "no till in reach")
        XCTAssertEqual(waitingToSyncText(till: till, phone: OfflineStats()), "2 payments waiting to sync")
        XCTAssertNil(waitingToSyncText(till: nil, phone: OfflineStats(signing: 1)), "a held sale is not waiting to sync")
    }

    func testTheHeldLine() {
        XCTAssertNil(heldOnPhoneText(0))
        XCTAssertEqual(heldOnPhoneText(1), "1 payment is waiting for its card: tap that card to finish it.")
        XCTAssertEqual(heldOnPhoneText(3), "3 payments are waiting for their cards: tap those cards to finish them.")
    }

    func testSettingsLines() {
        let t0: Int64 = 1_800_000_000_000
        XCTAssertEqual(durationText(45_000), "45 s")
        XCTAssertEqual(durationText(12 * 60_000), "12 min")
        XCTAssertEqual(durationText(5 * 3_600_000), "5 h")
        XCTAssertEqual(durationText(6 * 86_400_000), "6 days")
        XCTAssertEqual(kitAgeText(fetchedAtMs: t0, expiresAtMs: t0 + 7 * 86_400_000, nowMs: t0 + 3 * 60_000),
                       "Kit fetched 3 min ago · expires in 6 days")
        XCTAssertEqual(kitAgeText(fetchedAtMs: t0, expiresAtMs: t0 + 1000, nowMs: t0 + 1000), "Kit fetched 1 s ago · expired")
        XCTAssertEqual(refusedChargeText(priceMsat: 21_000, code: "pool_entry_unknown"),
                       "21 sats · pool_entry_unknown: the till refused it after the card paid.")
        XCTAssertEqual(unpairWarningText(2), "2 payments are waiting to sync with this till. They stay on this phone and can't be synced to another till.")
        XCTAssertEqual(unpairWarningText(1), "1 payment is waiting to sync with this till. It stays on this phone and can't be synced to another till.")
        let kit = KitInfo(kitId: "k", phoneId: "p", federationId: "f", capMsat: 100_000_000, capCount: 200, issuedAtMs: t0,
                          expiresAtMs: t0 + 7 * 86_400_000, poolEntries: 40, fetchedAtMs: t0)
        let on = OfflineSnapshot(enabled: true, disabledReason: nil, kit: kit, stats: OfflineStats(waiting: 2, waitingMsat: 7000, signing: 1, refused: 0, synced: 4))
        XCTAssertEqual(offlinePanelLines(on, nowMs: t0 + 60_000), [
            "Kit fetched 1 min ago · expires in 6 days",
            "Caps: 200 payments or 100,000 sats waiting to sync",
            "2 waiting to sync · 1 held for their cards · 4 synced",
        ])
        XCTAssertEqual(offlinePanelLines(OfflineSnapshot(enabled: true), nowMs: t0),
                       ["No offline kit yet. Reach the till once while online.", "0 waiting to sync · 0 held for their cards · 0 synced"])
        XCTAssertEqual(offlinePanelLines(.disabled, nowMs: t0), ["Offline payments are off on this phone."])
    }

    func testSettingsSaysWhyTheLastSyncStopped() {
        var phone = OfflineSnapshot(enabled: true, stats: OfflineStats(waiting: 2))
        phone.syncStopped = "2 payments wait for the till they were taken for"
        XCTAssertEqual(offlinePanelLines(phone, nowMs: 0), [
            "No offline kit yet. Reach the till once while online.",
            "2 waiting to sync · 0 held for their cards · 0 synced",
            "Sync stopped: 2 payments wait for the till they were taken for",
        ])
        phone.syncStopped = nil
        XCTAssertFalse(offlinePanelLines(phone, nowMs: 0).contains { $0.hasPrefix("Sync stopped") })
    }

    func testSettingsCountsTheDoubleSpendsTheTillRefused() {
        func till(_ offline: String) -> TillStatus? {
            let body = #"{"federation_id":"f","federation_name":null,"wallet_balance_msat":0,"card_relay_ok":true,"busy":false,"offline":"# + offline + "}"
            guard case .ok(let s) = TillParse.status(Data(body.utf8)) else { return nil }
            return s
        }
        var phone = OfflineSnapshot(enabled: true, stats: OfflineStats(waiting: 1))
        phone.syncStopped = "the till did not answer"
        let kitAndCounts = ["No offline kit yet. Reach the till once while online.", "1 waiting to sync · 0 held for their cards · 0 synced"]
        let stopped = "Sync stopped: the till did not answer"
        XCTAssertEqual(offlinePanelLines(phone, nowMs: 0, till: till(#"{"queued":0,"queued_msat":0,"double_spent":2}"#)),
                       kitAndCounts + ["Double spends refused by the till: 2", stopped])
        XCTAssertEqual(offlinePanelLines(phone, nowMs: 0, till: till(#"{"queued":0,"queued_msat":0,"double_spent":0}"#)),
                       kitAndCounts + ["Double spends refused by the till: 0", stopped], "a count of 0 the till reported")
        for absent in ["null", #"{"queued":0,"queued_msat":0}"#] {
            XCTAssertNotNil(till(absent), absent)
            XCTAssertEqual(offlinePanelLines(phone, nowMs: 0, till: till(absent)), kitAndCounts + [stopped], "no double_spent in \(absent)")
        }
        XCTAssertEqual(offlinePanelLines(phone, nowMs: 0), kitAndCounts + [stopped], "no till status read yet")
        XCTAssertEqual(offlinePanelLines(.disabled, nowMs: 0, till: till(#"{"double_spent":2}"#)),
                       ["Offline payments are off on this phone.", "Double spends refused by the till: 2"])
    }
}
