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

    @Test fun happyPath() {
        val f = flow()
        assertTrue(f.dispatch(PosEvent.ChargePressed).isEmpty())
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
        assertTrue(f.dispatch(PosEvent.ChargePressed).single() is PosEffect.StartCharge)
        assertTrue(f.state is PosState.Signing)
    }
    @Test fun noEffectUntilCardPresent() {
        val f = flow()
        assertTrue(f.dispatch(PosEvent.ChargePressed).isEmpty())
        assertTrue(f.dispatch(PosEvent.CardLost).isEmpty())
        assertTrue(f.state is PosState.WaitingForCard)
    }
    @Test fun zeroAmountNotChargeable() {
        val f = PosFlow { "r" }
        assertTrue(f.dispatch(PosEvent.ChargePressed).isEmpty()); assertEquals(PosState.Entering, f.state)
    }
    @Test fun signingIgnoresCardEventsAndCancel() {
        val f = flow(); f.dispatch(PosEvent.ChargePressed)
        f.dispatch(PosEvent.CardPresent); val before = f.state
        assertTrue(f.dispatch(PosEvent.CardPresent).isEmpty())
        assertTrue(f.dispatch(PosEvent.CardLost).isEmpty())
        assertTrue(f.dispatch(PosEvent.CancelPressed).isEmpty())
        assertEquals(before, f.state)
    }
    @Test fun staleChargeDoneIgnored() {
        val f = flow(); f.dispatch(PosEvent.ChargePressed); f.dispatch(PosEvent.CardPresent)
        f.dispatch(PosEvent.ChargeDone(99, ok))
        assertTrue(f.state is PosState.Signing)
    }
    @Test fun lingeringCardNeedsCardLostFirst() {
        val f = flow(); f.dispatch(PosEvent.CardPresent); f.dispatch(PosEvent.ChargePressed)
        f.dispatch(PosEvent.ChargeDone(f.signingId(), ok)); f.dispatch(PosEvent.NewSalePressed)
        f.pad.press(3)
        assertTrue(f.dispatch(PosEvent.ChargePressed).isEmpty())
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
            declined(ChargeErrorCode.UNDECIDED) to ErrorKind.UNDECIDED,
            declined(ChargeErrorCode.UNKNOWN) to ErrorKind.UNKNOWN,
            declined(ChargeErrorCode.RECOVERING) to ErrorKind.RECOVERING,
            ChargeResult.Failed(TillError.Unreachable) to ErrorKind.TILL_UNREACHABLE,
            ChargeResult.Failed(TillError.Timeout) to ErrorKind.TIMEOUT_UNKNOWN,
            ChargeResult.Failed(TillError.BadResponse("x")) to ErrorKind.INTERNAL,
        )
        for ((r, k) in cases) {
            val f = flow(); f.dispatch(PosEvent.CardPresent); f.dispatch(PosEvent.ChargePressed)
            f.dispatch(PosEvent.ChargeDone(f.signingId(), r))
            assertEquals(k, (f.state as PosState.Error).kind)
        }
        for (c in ChargeErrorCode.values()) ErrorKind.from(c)
    }
    @Test fun retryReusesClientRefAndGoesStraightToSigningWithCard() {
        val f = flow(); f.dispatch(PosEvent.CardPresent)
        val first = f.dispatch(PosEvent.ChargePressed).single() as PosEffect.StartCharge
        f.dispatch(PosEvent.ChargeDone(first.chargeId, declined(ChargeErrorCode.BUSY)))
        val second = f.dispatch(PosEvent.RetryPressed).single() as PosEffect.StartCharge
        assertEquals(first.clientRef, second.clientRef); assertNotEquals(first.chargeId, second.chargeId)
    }
    @Test fun retryWithoutCardWaitsThenReusesRef() {
        val f = flow(); f.dispatch(PosEvent.ChargePressed)
        val first = f.dispatch(PosEvent.CardPresent).single() as PosEffect.StartCharge
        f.dispatch(PosEvent.CardLost)
        f.dispatch(PosEvent.ChargeDone(first.chargeId, declined(ChargeErrorCode.NO_CARD)))
        assertTrue(f.dispatch(PosEvent.RetryPressed).isEmpty())
        assertTrue(f.state is PosState.WaitingForCard)
        val second = f.dispatch(PosEvent.CardPresent).single() as PosEffect.StartCharge
        assertEquals(first.clientRef, second.clientRef)
    }
    private fun unresolved(r: ChargeResult): Pair<PosFlow, PosEffect.StartCharge> {
        val f = flow(); f.dispatch(PosEvent.CardPresent)
        val fx = f.dispatch(PosEvent.ChargePressed).single() as PosEffect.StartCharge
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
        val again = f.dispatch(PosEvent.RetryPressed).single() as PosEffect.StartCharge
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
        assertTrue(f.dispatch(PosEvent.ChargePressed).isEmpty())
        assertEquals(PosState.WaitingForCard(5000, true), f.state)
    }
    @Test fun systemBackMapping() {
        val f = flow()
        assertNull(f.systemBack())
        f.dispatch(PosEvent.ChargePressed); assertEquals(PosEvent.CancelPressed, f.systemBack())
        f.dispatch(PosEvent.CardPresent); assertTrue(f.state is PosState.Signing)
        assertNull(f.systemBack())
        f.dispatch(PosEvent.BackPressed); assertTrue(f.state is PosState.Signing)
        f.dispatch(PosEvent.ChargeDone(f.signingId(), ok)); assertEquals(PosEvent.NewSalePressed, f.systemBack())
    }
    @Test fun cancelWorksWhileWaitingNotSigning() {
        val f = flow(); f.dispatch(PosEvent.ChargePressed); f.dispatch(PosEvent.CancelPressed)
        assertEquals(PosState.Entering, f.state)
        f.dispatch(PosEvent.CardPresent); f.dispatch(PosEvent.ChargePressed); f.dispatch(PosEvent.CancelPressed)
        assertTrue(f.state is PosState.Signing)
    }
    @Test fun errorBackAndSalesFromEntering() {
        val f = flow(); f.dispatch(PosEvent.CardPresent); f.dispatch(PosEvent.ChargePressed)
        f.dispatch(PosEvent.ChargeDone(f.signingId(), declined(ChargeErrorCode.DECLINED)))
        f.dispatch(PosEvent.BackPressed); assertEquals(PosState.Entering, f.state)
        f.dispatch(PosEvent.SalesOpened); assertEquals(PosState.Sales(null), f.state)
    }
    @Test fun tryAgainFromUnresolvedSignsImmediatelyWithoutCard() {
        val (f, fx) = unresolved(ChargeResult.Failed(TillError.Timeout))
        f.dispatch(PosEvent.CardLost)
        val again = f.dispatch(PosEvent.RetryPressed).single() as PosEffect.StartCharge
        assertEquals(fx.clientRef, again.clientRef); assertTrue(f.state is PosState.Signing)
    }
    @Test fun unresolvedIsStickyAcrossIndefiniteRetryOutcomes() {
        for (r in listOf<ChargeResult>(declined(ChargeErrorCode.BUSY), declined(ChargeErrorCode.RECOVERING),
            declined(ChargeErrorCode.RELAY_UNREACHABLE), declined(ChargeErrorCode.INTERNAL), declined(ChargeErrorCode.UNKNOWN),
            ChargeResult.Failed(TillError.Unreachable), ChargeResult.Failed(TillError.Interrupted),
            ChargeResult.Failed(TillError.Timeout), declined(ChargeErrorCode.UNDECIDED), ChargeResult.Failed(TillError.BadResponse("x")))) {
            val (f, fx) = unresolved(declined(ChargeErrorCode.UNDECIDED))
            f.dispatch(PosEvent.RetryPressed)
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
            f.dispatch(PosEvent.RetryPressed)
            f.dispatch(PosEvent.ChargeDone(f.signingId(), declined(c)))
            assertNull((f.state as PosState.Error).unresolvedRef)
            f.dispatch(PosEvent.BackPressed); assertEquals(PosState.Entering, f.state)
        }
    }
    @Test fun retrySuccessClearsUnresolved() {
        val (f, _) = unresolved(ChargeResult.Failed(TillError.Timeout))
        f.dispatch(PosEvent.RetryPressed); f.dispatch(PosEvent.ChargeDone(f.signingId(), ok))
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
}
