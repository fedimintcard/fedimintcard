package org.fedimintcard.relay

import org.junit.Assert.*
import org.junit.Test

class PosFlowTest {
    private var n = 0
    private fun flow(sats: Int = 5): PosFlow = PosFlow { "ref${++n}" }.also { f -> sats.toString().forEach { f.pad.press(it - '0') } }
    private val receipt = ChargeReceipt(1, "t", 5000, 5000, 0, 0, emptyList(), emptyList(), false, emptyList(), 1)
    private val ok = ChargeResult.Success(receipt)
    private fun declined(c: ChargeErrorCode) = ChargeResult.Declined(c, "d", 409)
    private fun PosFlow.signingId() = (state as PosState.Signing).chargeId
    private fun PosFlow.typePin(p: String = "1234") = p.forEach { pinPad.press(it - '0') }
    /** Amount screen's Charge, then the holder's PIN and Pay. */
    private fun PosFlow.charge(): List<PosEffect> { dispatch(PosEvent.ChargePressed); typePin(); return dispatch(PosEvent.PinConfirmed) }
    /** Error screen's Retry / Try again, then the holder's PIN and Pay. */
    private fun PosFlow.retry(): List<PosEffect> { dispatch(PosEvent.RetryPressed); typePin(); return dispatch(PosEvent.PinConfirmed) }

    @Test fun happyPath() {
        val f = flow()
        assertTrue(f.charge().isEmpty())
        assertEquals(PosState.WaitingForCard(5000, false), f.state)
        val fx = f.dispatch(PosEvent.CardPresent)
        assertEquals(1, fx.size)
        val sc = fx[0] as PosEffect.StartCharge
        assertEquals(5000L, sc.priceMsat)
        f.dispatch(PosEvent.ChargeDone(sc.chargeId, ok))
        assertTrue(f.state is PosState.Receipt)
        f.dispatch(PosEvent.NewSalePressed)
        assertEquals(PosState.Entering, f.state); assertEquals(0L, f.pad.sats)
    }
    @Test fun cardAlreadyPresentGoesStraightToSigning() {
        val f = flow(); f.dispatch(PosEvent.CardPresent)
        assertTrue(f.charge().single() is PosEffect.StartCharge)
        assertTrue(f.state is PosState.Signing)
    }
    @Test fun noEffectUntilCardPresent() {
        val f = flow()
        assertTrue(f.charge().isEmpty())
        assertTrue(f.dispatch(PosEvent.CardLost).isEmpty())
        assertTrue(f.state is PosState.WaitingForCard)
    }
    @Test fun zeroAmountNotChargeable() {
        val f = PosFlow { "r" }
        assertTrue(f.charge().isEmpty()); assertEquals(PosState.Entering, f.state)
    }
    @Test fun signingIgnoresCardEventsAndCancel() {
        val f = flow(); f.charge()
        f.dispatch(PosEvent.CardPresent); val before = f.state
        assertTrue(f.dispatch(PosEvent.CardPresent).isEmpty())
        assertTrue(f.dispatch(PosEvent.CardLost).isEmpty())
        assertTrue(f.dispatch(PosEvent.CancelPressed).isEmpty())
        assertEquals(before, f.state)
    }
    @Test fun staleChargeDoneIgnored() {
        val f = flow(); f.charge(); f.dispatch(PosEvent.CardPresent)
        f.dispatch(PosEvent.ChargeDone(99, ok))
        assertTrue(f.state is PosState.Signing)
    }
    @Test fun lingeringCardNeedsCardLostFirst() {
        val f = flow(); f.dispatch(PosEvent.CardPresent); f.charge()
        f.dispatch(PosEvent.ChargeDone(f.signingId(), ok)); f.dispatch(PosEvent.NewSalePressed)
        f.pad.press(3)
        assertTrue(f.charge().isEmpty())
        assertEquals(PosState.WaitingForCard(3000, true), f.state)
        assertTrue(f.dispatch(PosEvent.CardPresent).isEmpty())
        assertTrue(f.state is PosState.WaitingForCard)
        f.dispatch(PosEvent.CardLost)
        assertEquals(PosState.WaitingForCard(3000, false), f.state)
        assertTrue(f.dispatch(PosEvent.CardPresent).single() is PosEffect.StartCharge)
    }
    @Test fun errorMappings() {
        val cases = mapOf<ChargeResult, ErrorKind>(
            declined(ChargeErrorCode.NO_CARD) to ErrorKind.NO_CARD,
            declined(ChargeErrorCode.INSUFFICIENT) to ErrorKind.INSUFFICIENT,
            declined(ChargeErrorCode.NO_EXACT_PLAN) to ErrorKind.NO_EXACT_PLAN,
            declined(ChargeErrorCode.OFFLINE_NO_CHANGE_ROOM) to ErrorKind.OFFLINE_NO_CHANGE_ROOM,
            declined(ChargeErrorCode.UNDECIDED) to ErrorKind.UNDECIDED,
            declined(ChargeErrorCode.UNKNOWN) to ErrorKind.UNKNOWN,
            declined(ChargeErrorCode.RECOVERING) to ErrorKind.RECOVERING,
            ChargeResult.Failed(TillError.Unreachable) to ErrorKind.TILL_UNREACHABLE,
            ChargeResult.Failed(TillError.Timeout) to ErrorKind.TIMEOUT_UNKNOWN,
            ChargeResult.Failed(TillError.BadResponse("x")) to ErrorKind.INTERNAL,
        )
        for ((r, k) in cases) {
            val f = flow(); f.dispatch(PosEvent.CardPresent); f.charge()
            f.dispatch(PosEvent.ChargeDone(f.signingId(), r))
            assertEquals(k, (f.state as PosState.Error).kind)
        }
        for (c in ChargeErrorCode.values()) ErrorKind.from(c)
    }
    @Test fun noRoomForChangeOfflineKeepsTheSuggestionAndMovedNothing() {
        val f = flow(); f.dispatch(PosEvent.CardPresent); f.charge()
        f.dispatch(PosEvent.ChargeDone(f.signingId(), ChargeResult.Declined(ChargeErrorCode.OFFLINE_NO_CHANGE_ROOM, "d", 409, 12_000)))
        val e = f.state as PosState.Error
        assertEquals(ErrorKind.OFFLINE_NO_CHANGE_ROOM, e.kind)
        assertEquals(12_000L, e.suggestedMsat)
        assertNull(e.unresolvedRef)
    }
    @Test fun retryReusesClientRefAndGoesStraightToSigningWithCard() {
        val f = flow(); f.dispatch(PosEvent.CardPresent)
        val first = f.charge().single() as PosEffect.StartCharge
        f.dispatch(PosEvent.ChargeDone(first.chargeId, declined(ChargeErrorCode.BUSY)))
        val second = f.retry().single() as PosEffect.StartCharge
        assertEquals(first.clientRef, second.clientRef); assertNotEquals(first.chargeId, second.chargeId)
    }
    @Test fun retryWithoutCardWaitsThenReusesRef() {
        val f = flow(); f.charge()
        val first = f.dispatch(PosEvent.CardPresent).single() as PosEffect.StartCharge
        f.dispatch(PosEvent.CardLost)
        f.dispatch(PosEvent.ChargeDone(first.chargeId, declined(ChargeErrorCode.NO_CARD)))
        assertTrue(f.retry().isEmpty())
        assertTrue(f.state is PosState.WaitingForCard)
        val second = f.dispatch(PosEvent.CardPresent).single() as PosEffect.StartCharge
        assertEquals(first.clientRef, second.clientRef)
    }
    private fun unresolved(r: ChargeResult): Pair<PosFlow, PosEffect.StartCharge> {
        val f = flow(); f.dispatch(PosEvent.CardPresent)
        val fx = f.charge().single() as PosEffect.StartCharge
        f.dispatch(PosEvent.ChargeDone(fx.chargeId, r))
        return f to fx
    }
    @Test fun timeoutKeepsRefNoBackAndSalesReturnsToError() {
        val (f, fx) = unresolved(ChargeResult.Failed(TillError.Timeout))
        val err = f.state as PosState.Error
        assertNotNull(err.unresolvedRef); assertEquals(fx.clientRef, f.unresolvedRef)
        f.dispatch(PosEvent.BackPressed); assertEquals(err, f.state)
        assertNull(f.systemBack())
        f.dispatch(PosEvent.SalesOpened); assertEquals(PosState.Sales(err), f.state)
        assertEquals(fx.clientRef, f.unresolvedRef)
        f.dispatch(PosEvent.BackPressed); assertEquals(err, f.state)
    }
    @Test fun interruptedIsOutcomeUnknown() {
        val (f, _) = unresolved(ChargeResult.Failed(TillError.Interrupted))
        assertEquals(ErrorKind.TIMEOUT_UNKNOWN, (f.state as PosState.Error).kind)
    }
    @Test fun tryAgainFromUndecidedReusesRef() {
        val (f, fx) = unresolved(declined(ChargeErrorCode.UNDECIDED))
        val again = f.retry().single() as PosEffect.StartCharge
        assertEquals(fx.clientRef, again.clientRef)
    }
    @Test fun salesPollResolvesToReceipt() {
        val (f, fx) = unresolved(declined(ChargeErrorCode.UNDECIDED))
        f.dispatch(PosEvent.SaleFound(Sale(9, 1, 5000, 5000, 0, 0, "tx", "other")))
        assertTrue(f.state is PosState.Error)
        f.dispatch(PosEvent.SaleFound(Sale(9, 1, 5000, 5000, 0, 0, "tx", fx.clientRef)))
        assertEquals(9L, (f.state as PosState.Receipt).receipt.saleId)
        assertNull(f.unresolvedRef)
    }
    @Test fun salesPollResolvesFromSalesScreen() {
        val (f, fx) = unresolved(ChargeResult.Failed(TillError.Timeout))
        f.dispatch(PosEvent.SalesOpened)
        f.dispatch(PosEvent.SaleFound(Sale(2, 1, 5000, 5000, 0, 0, "tx", fx.clientRef)))
        assertTrue(f.state is PosState.Receipt)
    }
    @Test fun lingeringAfterDeclinedBlocksImmediateResign() {
        val (f, _) = unresolved(declined(ChargeErrorCode.DECLINED))
        f.dispatch(PosEvent.BackPressed)
        assertTrue(f.charge().isEmpty())
        assertEquals(PosState.WaitingForCard(5000, true), f.state)
    }
    @Test fun systemBackMapping() {
        val f = flow()
        assertNull(f.systemBack())
        f.charge(); assertEquals(PosEvent.CancelPressed, f.systemBack())
        f.dispatch(PosEvent.CardPresent); assertTrue(f.state is PosState.Signing)
        assertNull(f.systemBack())
        f.dispatch(PosEvent.BackPressed); assertTrue(f.state is PosState.Signing)
        f.dispatch(PosEvent.ChargeDone(f.signingId(), ok)); assertEquals(PosEvent.NewSalePressed, f.systemBack())
    }
    @Test fun cancelWorksWhileWaitingNotSigning() {
        val f = flow(); f.charge(); f.dispatch(PosEvent.CancelPressed)
        assertEquals(PosState.Entering, f.state)
        f.dispatch(PosEvent.CardPresent); f.charge(); f.dispatch(PosEvent.CancelPressed)
        assertTrue(f.state is PosState.Signing)
    }
    @Test fun errorBackAndSalesFromEntering() {
        val f = flow(); f.dispatch(PosEvent.CardPresent); f.charge()
        f.dispatch(PosEvent.ChargeDone(f.signingId(), declined(ChargeErrorCode.DECLINED)))
        f.dispatch(PosEvent.BackPressed); assertEquals(PosState.Entering, f.state)
        f.dispatch(PosEvent.SalesOpened); assertEquals(PosState.Sales(null), f.state)
    }
    @Test fun tryAgainFromUnresolvedSignsImmediatelyWithoutCard() {
        val (f, fx) = unresolved(ChargeResult.Failed(TillError.Timeout))
        f.dispatch(PosEvent.CardLost)
        val again = f.retry().single() as PosEffect.StartCharge
        assertEquals(fx.clientRef, again.clientRef); assertTrue(f.state is PosState.Signing)
    }
    @Test fun unresolvedIsStickyAcrossIndefiniteRetryOutcomes() {
        for (r in listOf<ChargeResult>(declined(ChargeErrorCode.BUSY), declined(ChargeErrorCode.RECOVERING),
            declined(ChargeErrorCode.RELAY_UNREACHABLE), declined(ChargeErrorCode.INTERNAL), declined(ChargeErrorCode.UNKNOWN),
            ChargeResult.Failed(TillError.Unreachable), ChargeResult.Failed(TillError.Interrupted),
            ChargeResult.Failed(TillError.Timeout), declined(ChargeErrorCode.UNDECIDED), ChargeResult.Failed(TillError.BadResponse("x")))) {
            val (f, fx) = unresolved(declined(ChargeErrorCode.UNDECIDED))
            f.retry()
            f.dispatch(PosEvent.ChargeDone(f.signingId(), r))
            assertEquals(fx.clientRef, (f.state as PosState.Error).unresolvedRef)
            f.dispatch(PosEvent.BackPressed); assertTrue(f.state is PosState.Error)
            assertNull(f.systemBack())
        }
    }
    @Test fun definitiveOutcomeAfterRetryClearsUnresolved() {
        for (c in listOf(ChargeErrorCode.INSUFFICIENT, ChargeErrorCode.NO_EXACT_PLAN, ChargeErrorCode.DECLINED,
            ChargeErrorCode.BAD_REQUEST, ChargeErrorCode.NO_CARD, ChargeErrorCode.CARD_ERROR)) {
            val (f, _) = unresolved(declined(ChargeErrorCode.UNDECIDED))
            f.retry()
            f.dispatch(PosEvent.ChargeDone(f.signingId(), declined(c)))
            assertNull((f.state as PosState.Error).unresolvedRef)
            f.dispatch(PosEvent.BackPressed); assertEquals(PosState.Entering, f.state)
        }
    }
    @Test fun retrySuccessClearsUnresolved() {
        val (f, _) = unresolved(ChargeResult.Failed(TillError.Timeout))
        f.retry(); f.dispatch(PosEvent.ChargeDone(f.signingId(), ok))
        assertTrue(f.state is PosState.Receipt); assertNull(f.unresolvedRef)
    }
    @Test fun abandonClearsRefAndPad() {
        val (f, _) = unresolved(ChargeResult.Failed(TillError.Timeout))
        f.dispatch(PosEvent.AbandonPressed)
        assertEquals(PosState.Entering, f.state); assertNull(f.unresolvedRef); assertEquals(0L, f.pad.sats)
    }
    @Test fun abandonIgnoredOnOrdinaryError() {
        val (f, _) = unresolved(declined(ChargeErrorCode.DECLINED))
        f.dispatch(PosEvent.AbandonPressed); assertTrue(f.state is PosState.Error)
    }
    @Test fun pollGenerationSupersedes() {
        val g = PollGeneration()
        val a = g.next(); val b = g.next()
        assertFalse(g.isCurrent(a)); assertTrue(g.isCurrent(b))
        g.cancel(); assertFalse(g.isCurrent(b))
    }

    // ---- the phone's own charge (docs/POS-OFFLINE.md § Android) ----

    private fun local(c: ChargeErrorCode, held: Boolean = false, detail: String = "d", suggested: Long? = null) =
        ChargeResult.Declined(c, detail, 0, suggested, held = held, local = true)

    @Test fun aSaleHeldOnThePhoneIsUnresolvedWithoutBackAndTryAgainSendsTheSameRef() {
        val (f, fx) = unresolved(local(ChargeErrorCode.UNDECIDED, held = true))
        val e = f.state as PosState.Error
        assertEquals(ErrorKind.HELD_ON_PHONE, e.kind); assertTrue(e.held)
        assertEquals(fx.clientRef, e.unresolvedRef); assertTrue(f.unresolvedHeld)
        assertNull(f.systemBack())
        f.dispatch(PosEvent.BackPressed); assertEquals(e, f.state)
        // Try again: the PIN, then the same sale at once, card or not (the core finds the card).
        f.dispatch(PosEvent.CardLost)
        val again = f.retry().single() as PosEffect.StartCharge
        assertEquals(fx.clientRef, again.clientRef); assertNotEquals(fx.chargeId, again.chargeId)
        assertEquals(fx.clientRef, f.unresolvedRef)
    }

    @Test fun whateverTheRetapAnswersAHeldSaleStaysHeld() {
        for (c in listOf(ChargeErrorCode.UNDECIDED, ChargeErrorCode.NO_CARD, ChargeErrorCode.OFFLINE_KIT_EXPIRED, ChargeErrorCode.CARD_ERROR,
            ChargeErrorCode.INSUFFICIENT, ChargeErrorCode.OFFLINE_NOT_READY)) {
            val (f, fx) = unresolved(local(ChargeErrorCode.UNDECIDED, held = true))
            f.retry()
            f.dispatch(PosEvent.ChargeDone(f.signingId(), local(c, held = true)))
            val e = f.state as PosState.Error
            assertEquals(c.name, ErrorKind.HELD_ON_PHONE, e.kind); assertTrue(e.held); assertEquals(fx.clientRef, e.unresolvedRef)
            assertNull(f.systemBack())
        }
    }

    @Test fun aHeldSaleSettlesOnlyOnThePhone() {
        val (f, _) = unresolved(local(ChargeErrorCode.UNDECIDED, held = true))
        f.retry()
        val paid = receipt.copy(saleId = 0, offline = true, collectionPending = true)
        f.dispatch(PosEvent.ChargeDone(f.signingId(), ChargeResult.Success(paid)))
        assertEquals(PosState.Receipt(paid), f.state)
        assertNull(f.unresolvedRef); assertFalse(f.unresolvedHeld)
    }

    @Test fun aHeldWrongPinAsksAgainAndReturnsToTheHeldSale() {
        val (f, fx) = unresolved(local(ChargeErrorCode.UNDECIDED, held = true))
        f.retry()
        f.dispatch(PosEvent.ChargeDone(f.signingId(), local(ChargeErrorCode.WRONG_PIN, held = true, detail = "wrong PIN; 2 tries left before the card blocks")))
        val ask = f.state as PosState.EnteringPin
        assertTrue(ask.wrongPin); assertEquals(2, ask.triesLeft); assertEquals(fx.clientRef, ask.clientRef)
        assertEquals(ErrorKind.HELD_ON_PHONE, ask.returnTo!!.kind); assertTrue(ask.returnTo!!.held)
        assertTrue(f.unresolvedHeld); assertEquals(fx.clientRef, f.unresolvedRef)
        // Cancel goes back to the held sale, never to the amount.
        f.dispatch(PosEvent.CancelPressed)
        assertEquals(ErrorKind.HELD_ON_PHONE, (f.state as PosState.Error).kind)
        // The PIN again sends the same sale at once.
        f.dispatch(PosEvent.RetryPressed); f.typePin()
        val again = f.dispatch(PosEvent.PinConfirmed).single() as PosEffect.StartCharge
        assertEquals(fx.clientRef, again.clientRef)
    }

    @Test fun aHeldWrongPinWithNoTriesLeftIsBlockedAndStillHeld() {
        val (f, fx) = unresolved(local(ChargeErrorCode.UNDECIDED, held = true))
        f.retry()
        f.dispatch(PosEvent.ChargeDone(f.signingId(), local(ChargeErrorCode.WRONG_PIN, held = true, detail = "wrong PIN; 0 tries left before the card blocks")))
        val e = f.state as PosState.Error
        assertEquals(ErrorKind.PIN_BLOCKED, e.kind); assertEquals(fx.clientRef, e.unresolvedRef); assertTrue(e.held)
        assertNull(f.systemBack()); assertTrue(f.unresolvedHeld)
        f.dispatch(PosEvent.BackPressed); assertEquals(e, f.state)
        f.dispatch(PosEvent.AbandonPressed); assertEquals(PosState.Entering, f.state); assertFalse(f.unresolvedHeld)
    }

    @Test fun aWrongPinOnALocalChargeThatHoldsNothingIsAFreshAttempt() {
        val (f, _) = unresolved(local(ChargeErrorCode.WRONG_PIN, detail = "wrong PIN; 2 tries left before the card blocks"))
        val ask = f.state as PosState.EnteringPin
        assertTrue(ask.wrongPin); assertNull(ask.returnTo); assertFalse(f.unresolvedHeld); assertNull(f.unresolvedRef)
    }

    @Test fun abandoningAHeldSaleOnlyHidesIt() {
        val (f, _) = unresolved(local(ChargeErrorCode.UNDECIDED, held = true))
        f.dispatch(PosEvent.AbandonPressed)
        assertEquals(PosState.Entering, f.state); assertNull(f.unresolvedRef); assertEquals(0L, f.pad.sats)
    }

    @Test fun codesFromThePhoneMapToTheirOwnKinds() {
        val cases = mapOf(
            ChargeErrorCode.OFFLINE_NOT_READY to ErrorKind.OFFLINE_NOT_READY,
            ChargeErrorCode.OFFLINE_KIT_EXPIRED to ErrorKind.OFFLINE_KIT_EXPIRED,
            ChargeErrorCode.OFFLINE_CAP_REACHED to ErrorKind.OFFLINE_CAP_REACHED,
            ChargeErrorCode.OFFLINE_NOTE_INVALID to ErrorKind.OFFLINE_NOTE_INVALID,
            ChargeErrorCode.OFFLINE_POOL_EXHAUSTED to ErrorKind.OFFLINE_POOL_EXHAUSTED,
            ChargeErrorCode.OFFLINE_UNSUPPORTED_CARD to ErrorKind.OFFLINE_UNSUPPORTED_CARD,
            ChargeErrorCode.CARD_NOT_GENUINE to ErrorKind.CARD_NOT_GENUINE,
            ChargeErrorCode.CARD_LIMIT to ErrorKind.CARD_LIMIT,
            ChargeErrorCode.OFFLINE_NO_CHANGE_ROOM to ErrorKind.OFFLINE_NO_CHANGE_ROOM,
            ChargeErrorCode.INSUFFICIENT to ErrorKind.INSUFFICIENT,
            ChargeErrorCode.NO_CARD to ErrorKind.NO_CARD,
            ChargeErrorCode.CARD_ERROR to ErrorKind.CARD_ERROR,
        )
        for ((c, k) in cases) {
            val (f, _) = unresolved(local(c))
            val e = f.state as PosState.Error
            assertEquals(c.name, k, e.kind); assertNull(c.name, e.unresolvedRef); assertFalse(e.held)
            f.dispatch(PosEvent.BackPressed); assertEquals(PosState.Entering, f.state)
        }
        val (f, _) = unresolved(local(ChargeErrorCode.OFFLINE_POOL_EXHAUSTED, suggested = 49_000))
        assertEquals(49_000L, (f.state as PosState.Error).suggestedMsat)
    }

    @Test fun phoneOnlyCodesFromTheTillReadAsBefore() {
        for (c in listOf(ChargeErrorCode.OFFLINE_NOT_READY, ChargeErrorCode.OFFLINE_KIT_EXPIRED, ChargeErrorCode.OFFLINE_CAP_REACHED,
            ChargeErrorCode.OFFLINE_NOTE_INVALID, ChargeErrorCode.OFFLINE_POOL_EXHAUSTED, ChargeErrorCode.OFFLINE_UNSUPPORTED_CARD)) {
            val (f, _) = unresolved(declined(c))
            assertEquals(c.name, ErrorKind.UNKNOWN, (f.state as PosState.Error).kind)
        }
        // The card's own refusals say the same from either.
        assertEquals(ErrorKind.CARD_NOT_GENUINE, (unresolved(declined(ChargeErrorCode.CARD_NOT_GENUINE)).first.state as PosState.Error).kind)
        assertEquals(ErrorKind.CARD_LIMIT, (unresolved(declined(ChargeErrorCode.CARD_LIMIT)).first.state as PosState.Error).kind)
        // A till's undecided is not held on the phone: its sales are polled as before.
        val (f, _) = unresolved(declined(ChargeErrorCode.UNDECIDED))
        assertEquals(ErrorKind.UNDECIDED, (f.state as PosState.Error).kind); assertFalse(f.unresolvedHeld)
    }

    @Test fun phoneCodesNeverSettleAnUnresolvedSale() {
        for (k in listOf(ErrorKind.OFFLINE_NOT_READY, ErrorKind.OFFLINE_KIT_EXPIRED, ErrorKind.OFFLINE_CAP_REACHED, ErrorKind.OFFLINE_NOTE_INVALID,
            ErrorKind.OFFLINE_POOL_EXHAUSTED, ErrorKind.HELD_ON_PHONE)) assertFalse(k.name, k.definitive)
        assertTrue(ErrorKind.HELD_ON_PHONE.needsSalesCheck); assertTrue(ErrorKind.HELD_ON_PHONE.retryable)
    }

    @Test fun anOfflineReceipt() {
        val paid = receipt.copy(saleId = 0, txid = "tx-local", offline = true, collectionPending = true)
        val (f, _) = unresolved(ChargeResult.Success(paid))
        val r = (f.state as PosState.Receipt).receipt
        assertTrue(r.offline); assertEquals("tx-local", r.txid); assertEquals(5000L, r.priceMsat)
    }

    /** Decision 3: a lost reply, then a retry whose link cannot open, stays on the unreachable screen, unresolved. */
    @Test fun aLostReplyThatCannotReachTheTillStaysUnresolved() {
        val (f, fx) = unresolved(ChargeResult.Failed(TillError.Timeout))
        f.retry()
        f.dispatch(PosEvent.ChargeDone(f.signingId(), ChargeResult.Failed(TillError.Unreachable)))
        val e = f.state as PosState.Error
        assertEquals(ErrorKind.TILL_UNREACHABLE, e.kind); assertEquals(fx.clientRef, e.unresolvedRef); assertFalse(e.held)
        assertFalse(f.unresolvedHeld)
    }
}
