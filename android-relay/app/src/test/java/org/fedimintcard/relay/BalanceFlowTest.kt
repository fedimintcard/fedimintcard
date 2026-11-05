package org.fedimintcard.relay

import org.junit.Assert.*
import org.junit.Test

/** Check card balance: wait for a card, read it, show it, return to the amount. */
class BalanceFlowTest {
    private fun flow(sats: Int = 5): PosFlow = PosFlow { "ref" }.also { f -> sats.toString().forEach { f.pad.press(it - '0') } }
    private val read = BalanceResult.Read(CardBalance(65_500, emptyList(), emptyList()))
    private fun PosFlow.readId() = (state as PosState.BalanceReading).readId

    @Test fun waitsForTheCardBeforeAskingTheTill() {
        val f = flow()
        assertTrue(f.dispatch(PosEvent.CardBalanceOpened).isEmpty())
        assertEquals(PosState.BalanceWaiting, f.state)
        val fx = f.dispatch(PosEvent.CardPresent).single() as PosEffect.StartBalanceRead
        assertEquals(PosState.BalanceReading(fx.readId), f.state)
        f.dispatch(PosEvent.BalanceDone(fx.readId, read))
        assertEquals(PosState.BalanceShown(fx.readId, 65_500), f.state)
    }

    @Test fun aCardAlreadyOnThePhoneIsReadAtOnce() {
        val f = flow(); f.dispatch(PosEvent.CardPresent)
        assertTrue(f.dispatch(PosEvent.CardBalanceOpened).single() is PosEffect.StartBalanceRead)
        assertTrue(f.state is PosState.BalanceReading)
    }

    @Test fun aCardLeftByTheLastSaleIsReadAtOnce() {
        val f = flow(); f.dispatch(PosEvent.CardPresent)
        f.dispatch(PosEvent.ChargePressed); "1234".forEach { f.pinPad.press(it - '0') }
        val sc = f.dispatch(PosEvent.PinConfirmed).single() as PosEffect.StartCharge
        f.dispatch(PosEvent.ChargeDone(sc.chargeId, ChargeResult.Declined(ChargeErrorCode.INSUFFICIENT, "", 402)))
        f.dispatch(PosEvent.BackPressed)
        assertTrue(f.dispatch(PosEvent.CardBalanceOpened).single() is PosEffect.StartBalanceRead)
    }

    @Test fun onlyFromTheAmountScreen() {
        val f = flow(); f.dispatch(PosEvent.ChargePressed)
        assertTrue(f.dispatch(PosEvent.CardBalanceOpened).isEmpty())
        assertTrue(f.state is PosState.EnteringPin)
    }

    @Test fun cancelWhileWaitingKeepsTheAmount() {
        val f = flow(42); f.dispatch(PosEvent.CardBalanceOpened)
        f.dispatch(PosEvent.CancelPressed)
        assertEquals(PosState.Entering, f.state); assertEquals(42L, f.pad.sats)
        assertTrue("a card after cancel starts nothing", f.dispatch(PosEvent.CardPresent).isEmpty())
    }

    @Test fun cancelWhileReadingDropsTheLateReply() {
        val f = flow(); f.dispatch(PosEvent.CardBalanceOpened); f.dispatch(PosEvent.CardPresent)
        val id = f.readId()
        f.dispatch(PosEvent.CancelPressed)
        f.dispatch(PosEvent.BalanceDone(id, read))
        assertEquals(PosState.Entering, f.state)
    }

    @Test fun aStaleReplyDoesNotLandOnANewerRead() {
        val f = flow(); f.dispatch(PosEvent.CardPresent)
        f.dispatch(PosEvent.CardBalanceOpened); val first = f.readId()
        f.dispatch(PosEvent.CancelPressed)
        f.dispatch(PosEvent.CardBalanceOpened); val second = f.readId()
        assertNotEquals(first, second)
        f.dispatch(PosEvent.BalanceDone(first, read))
        assertEquals(PosState.BalanceReading(second), f.state)
    }

    @Test fun cardLostWhileReadingWaitsForTheTill() {
        val f = flow(); f.dispatch(PosEvent.CardBalanceOpened); f.dispatch(PosEvent.CardPresent)
        val id = f.readId()
        assertTrue(f.dispatch(PosEvent.CardLost).isEmpty())
        assertEquals(PosState.BalanceReading(id), f.state)
        f.dispatch(PosEvent.BalanceDone(id, BalanceResult.Refused(ChargeErrorCode.NO_CARD, "no card")))
        assertEquals(PosState.BalanceFailed(BalanceProblem.CARD_MOVED, "no card"), f.state)
    }

    @Test fun tryAgainWaitsForTheCardAgain() {
        val f = flow(); f.dispatch(PosEvent.CardBalanceOpened); f.dispatch(PosEvent.CardPresent)
        f.dispatch(PosEvent.BalanceDone(f.readId(), BalanceResult.Failed(TillError.Unreachable)))
        assertEquals(BalanceProblem.TILL_UNREACHABLE, (f.state as PosState.BalanceFailed).problem)
        f.dispatch(PosEvent.CardLost)
        assertTrue(f.dispatch(PosEvent.RetryPressed).isEmpty())
        assertEquals(PosState.BalanceWaiting, f.state)
        assertTrue(f.dispatch(PosEvent.CardPresent).single() is PosEffect.StartBalanceRead)
    }

    @Test fun doneAndBackLeaveAnErrorForTheAmount() {
        for (e in listOf(PosEvent.DonePressed, PosEvent.BackPressed)) {
            val f = flow(7); f.dispatch(PosEvent.CardBalanceOpened); f.dispatch(PosEvent.CardPresent)
            f.dispatch(PosEvent.BalanceDone(f.readId(), BalanceResult.Refused(ChargeErrorCode.UNKNOWN_CARD, "")))
            f.dispatch(e)
            assertEquals(PosState.Entering, f.state); assertEquals(7L, f.pad.sats)
        }
    }

    @Test fun doneLeavesTheBalance() {
        val f = flow(); f.dispatch(PosEvent.CardBalanceOpened); f.dispatch(PosEvent.CardPresent)
        f.dispatch(PosEvent.BalanceDone(f.readId(), read))
        f.dispatch(PosEvent.DonePressed)
        assertEquals(PosState.Entering, f.state)
    }

    @Test fun theShownBalanceExpiresOnlyForItsOwnRead() {
        val f = flow(); f.dispatch(PosEvent.CardPresent)
        f.dispatch(PosEvent.CardBalanceOpened); val first = f.readId()
        f.dispatch(PosEvent.BalanceDone(first, read)); f.dispatch(PosEvent.DonePressed)
        f.dispatch(PosEvent.CardBalanceOpened); val second = f.readId()
        f.dispatch(PosEvent.BalanceDone(second, read))
        f.dispatch(PosEvent.BalanceExpired(first)) // the first screen's timer, still running
        assertEquals(PosState.BalanceShown(second, 65_500), f.state)
        f.dispatch(PosEvent.BalanceExpired(second))
        assertEquals(PosState.Entering, f.state)
        assertEquals(15_000L, PosFlow.BALANCE_SHOWN_MS)
    }

    @Test fun backgroundingReturnsToTheAmount() {
        for (setup in listOf<(PosFlow) -> Unit>(
            { it.dispatch(PosEvent.CardBalanceOpened) },
            { it.dispatch(PosEvent.CardBalanceOpened); it.dispatch(PosEvent.CardPresent) },
        )) {
            val f = flow(); setup(f)
            f.dispatch(PosEvent.Backgrounded)
            assertEquals(PosState.Entering, f.state)
        }
    }

    @Test fun systemBack() {
        val f = flow(); f.dispatch(PosEvent.CardBalanceOpened)
        assertEquals(PosEvent.CancelPressed, f.systemBack())
        f.dispatch(PosEvent.CardPresent)
        assertEquals(PosEvent.CancelPressed, f.systemBack())
        f.dispatch(PosEvent.BalanceDone(f.readId(), read))
        assertEquals(PosEvent.DonePressed, f.systemBack())
    }

    @Test fun aChargeAfterABalanceReadStillWorks() {
        val f = flow(); f.dispatch(PosEvent.CardBalanceOpened); f.dispatch(PosEvent.CardPresent)
        f.dispatch(PosEvent.BalanceDone(f.readId(), read)); f.dispatch(PosEvent.DonePressed)
        f.dispatch(PosEvent.CardLost)
        f.dispatch(PosEvent.ChargePressed); "1234".forEach { f.pinPad.press(it - '0') }
        assertTrue(f.dispatch(PosEvent.PinConfirmed).isEmpty())
        assertEquals(PosState.WaitingForCard(5000, false), f.state)
    }
}
