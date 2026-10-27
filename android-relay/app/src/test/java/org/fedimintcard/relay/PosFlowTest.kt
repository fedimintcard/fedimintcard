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
    @Test fun timeoutOffersSalesCheck() {
        val f = flow(); f.dispatch(PosEvent.CardPresent); f.dispatch(PosEvent.ChargePressed)
        f.dispatch(PosEvent.ChargeDone(f.signingId(), ChargeResult.Failed(TillError.Timeout)))
        assertTrue((f.state as PosState.Error).kind.needsSalesCheck)
        f.dispatch(PosEvent.SalesOpened); assertEquals(PosState.Sales, f.state)
        f.dispatch(PosEvent.BackPressed); assertEquals(PosState.Entering, f.state)
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
        f.dispatch(PosEvent.SalesOpened); assertEquals(PosState.Sales, f.state)
    }
}
