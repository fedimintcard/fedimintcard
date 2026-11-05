package org.fedimintcard.relay

import org.junit.Assert.*
import org.junit.Test

/** Top-up by Lightning from the balance screen (docs/TOPUP.md). */
class TopUpFlowTest {
    private var refs = 0
    private fun flow(): PosFlow = PosFlow { "ref${++refs}" }
    private val t = TopUp("t1", 100_352, "lnbc1x", 1_000_000, "waiting")
    private fun st(state: String, txid: String? = null, detail: String? = null) =
        TopUpReply.Ok(TopUpStatus("t1", 100_352, state, txid, detail))

    /** Balance read and shown, card still on the phone unless [lift]. */
    private fun shown(lift: Boolean = true): PosFlow {
        val f = flow()
        f.dispatch(PosEvent.CardPresent)
        val fx = f.dispatch(PosEvent.CardBalanceOpened).single() as PosEffect.StartBalanceRead
        f.dispatch(PosEvent.BalanceDone(fx.readId, BalanceResult.Read(CardBalance(5_000, emptyList(), emptyList()))))
        if (lift) f.dispatch(PosEvent.CardLost)
        return f
    }

    private fun invoiced(f: PosFlow = shown()): PosFlow {
        f.dispatch(PosEvent.TopUpPressed)
        "100".forEach { f.topUpPad.press(it - '0') }
        val c = f.dispatch(PosEvent.CreateInvoicePressed).single() as PosEffect.CreateTopUp
        f.dispatch(PosEvent.TopUpCreated(c.createId, TopUpReply.Ok(t)))
        return f
    }

    @Test fun topUpFromTheBalanceScreen() {
        val f = shown()
        f.pad.press(7) // a sale being typed is kept apart
        f.dispatch(PosEvent.TopUpPressed)
        assertEquals(PosState.TopUpEntering, f.state)
        assertEquals(0L, f.topUpPad.sats)
        assertTrue(f.dispatch(PosEvent.CreateInvoicePressed).isEmpty()) // nothing typed
        "100".forEach { f.topUpPad.press(it - '0') }
        val c = f.dispatch(PosEvent.CreateInvoicePressed).single() as PosEffect.CreateTopUp
        assertEquals(100_000L, c.amountMsat)
        assertEquals(PosState.TopUpCreating(c.createId, 100_000, c.clientRef), f.state)
        f.dispatch(PosEvent.TopUpCreated(c.createId, TopUpReply.Ok(t)))
        assertEquals(PosState.TopUpInvoice(t, c.clientRef), f.state)
        assertEquals(7L, f.pad.sats)
    }

    @Test fun chargeCardGoesToTheKeypad() {
        val f = shown(lift = false)
        f.dispatch(PosEvent.ChargeCardPressed)
        assertEquals(PosState.Entering, f.state)
    }

    @Test fun paidWaitsForTheCardThenLoads() {
        val f = invoiced()
        assertTrue(f.dispatch(PosEvent.TopUpPolled("t1", st("waiting"))).isEmpty())
        assertTrue(f.state is PosState.TopUpInvoice)
        f.dispatch(PosEvent.TopUpPolled("t1", st("paid")))
        assertTrue(f.state is PosState.TopUpWaitingForCard)
        val l = f.dispatch(PosEvent.CardPresent).single() as PosEffect.LoadTopUp
        assertEquals("t1", l.topupId)
        f.dispatch(PosEvent.TopUpLoadDone(l.loadId, st("loaded", txid = "abc")))
        assertEquals(PosState.TopUpLoaded(100_352, "abc"), f.state)
        f.dispatch(PosEvent.NewSalePressed)
        assertEquals(PosState.Entering, f.state)
    }

    @Test fun aCardStillOnThePhoneIsLoadedAtOnce() {
        val f = invoiced(shown(lift = false))
        assertTrue(f.dispatch(PosEvent.TopUpPolled("t1", st("paid"))).single() is PosEffect.LoadTopUp)
        assertTrue(f.state is PosState.TopUpLoading)
    }

    @Test fun pollsForAnotherInvoiceOrFailedPollsAreIgnored() {
        val f = invoiced()
        f.dispatch(PosEvent.TopUpPolled("other", st("paid")))
        f.dispatch(PosEvent.TopUpPolled("t1", TopUpReply.Failed(TillError.Unreachable)))
        assertTrue(f.state is PosState.TopUpInvoice)
    }

    @Test fun expiredOffersANewInvoiceWithANewRef() {
        val f = invoiced()
        val ref = (f.state as PosState.TopUpInvoice).clientRef
        f.dispatch(PosEvent.TopUpPolled("t1", st("expired")))
        val e = f.state as PosState.TopUpFailed
        assertEquals(TopUpProblem.EXPIRED, e.problem)
        val c = f.dispatch(PosEvent.RetryPressed).single() as PosEffect.CreateTopUp
        assertNotEquals(ref, c.clientRef)
        assertEquals(100_352L, c.amountMsat)
    }

    @Test fun aFailedCreateRetriesWithTheSameRef() {
        val f = shown()
        f.dispatch(PosEvent.TopUpPressed); f.topUpPad.press(5)
        val c = f.dispatch(PosEvent.CreateInvoicePressed).single() as PosEffect.CreateTopUp
        f.dispatch(PosEvent.TopUpCreated(c.createId, TopUpReply.Failed(TillError.Timeout)))
        assertEquals(TopUpProblem.TIMEOUT, (f.state as PosState.TopUpFailed).problem)
        val again = f.dispatch(PosEvent.RetryPressed).single() as PosEffect.CreateTopUp
        assertEquals(c.clientRef, again.clientRef)
        assertEquals(5_000L, again.amountMsat)
    }

    @Test fun badAmountShowsTheTillsWords() {
        val f = shown()
        f.dispatch(PosEvent.TopUpPressed); f.topUpPad.press(9)
        val c = f.dispatch(PosEvent.CreateInvoicePressed).single() as PosEffect.CreateTopUp
        f.dispatch(PosEvent.TopUpCreated(c.createId, TopUpReply.Refused("bad_amount", "at most 100,000 sats", 400)))
        val e = f.state as PosState.TopUpFailed
        assertEquals(TopUpProblem.BAD_AMOUNT, e.problem)
        assertEquals("at most 100,000 sats", topUpErrorCopy(e.problem, e.paid, e.detail).second)
    }

    @Test fun cardFullStaysPaidAndTapsAgain() {
        val f = invoiced()
        f.dispatch(PosEvent.TopUpPolled("t1", st("paid")))
        val l = f.dispatch(PosEvent.CardPresent).single() as PosEffect.LoadTopUp
        f.dispatch(PosEvent.TopUpLoadDone(l.loadId, TopUpReply.Refused("card_full", "no free slots", 409)))
        val e = f.state as PosState.TopUpFailed
        assertEquals(TopUpProblem.CARD_FULL, e.problem)
        assertTrue(e.paid)
        assertEquals("This card is full", topUpErrorCopy(e.problem, e.paid, e.detail).first)
        // the card is still on the phone: tap again loads at once
        assertTrue(f.dispatch(PosEvent.RetryPressed).single() is PosEffect.LoadTopUp)
    }

    @Test fun aLostCardWaitsForTheCardAgain() {
        val f = invoiced()
        f.dispatch(PosEvent.TopUpPolled("t1", st("paid")))
        val l = f.dispatch(PosEvent.CardPresent).single() as PosEffect.LoadTopUp
        f.dispatch(PosEvent.CardLost)
        f.dispatch(PosEvent.TopUpLoadDone(l.loadId, TopUpReply.Refused("card_link_gone", "", 409)))
        assertEquals(TopUpProblem.CARD_MOVED, (f.state as PosState.TopUpFailed).problem)
        assertTrue(f.dispatch(PosEvent.RetryPressed).isEmpty())
        assertTrue(f.state is PosState.TopUpWaitingForCard)
        assertTrue(f.dispatch(PosEvent.CardPresent).single() is PosEffect.LoadTopUp)
    }

    @Test fun theTillGivingUpHasNoRetry() {
        val f = invoiced()
        f.dispatch(PosEvent.TopUpPolled("t1", st("failed", detail = "gateway refused")))
        val e = f.state as PosState.TopUpFailed
        assertEquals(TopUpProblem.FAILED, e.problem)
        assertTrue(f.dispatch(PosEvent.RetryPressed).isEmpty())
        f.dispatch(PosEvent.DonePressed)
        assertEquals(PosState.Entering, f.state)
    }

    @Test fun staleLoadRepliesAreIgnored() {
        val f = invoiced()
        f.dispatch(PosEvent.TopUpPolled("t1", st("paid")))
        val l = f.dispatch(PosEvent.CardPresent).single() as PosEffect.LoadTopUp
        f.dispatch(PosEvent.TopUpLoadDone(l.loadId + 99, st("loaded", txid = "x")))
        assertTrue(f.state is PosState.TopUpLoading)
    }

    @Test fun cancelAndBack() {
        val f = invoiced()
        assertEquals(PosEvent.CancelPressed, f.systemBack())
        f.dispatch(PosEvent.CancelPressed)
        assertEquals(PosState.Entering, f.state)
        val g = invoiced()
        g.dispatch(PosEvent.TopUpPolled("t1", st("paid")))
        assertEquals(PosEvent.CancelPressed, g.systemBack())
        g.dispatch(PosEvent.CancelPressed) // paid: kept, not dropped
        val nl = g.state as PosState.TopUpFailed
        assertEquals(TopUpProblem.NOT_LOADED, nl.problem); assertTrue(nl.paid)
        g.dispatch(PosEvent.RetryPressed)
        assertTrue(g.state is PosState.TopUpWaitingForCard)
        val l = g.dispatch(PosEvent.CardPresent).single() as PosEffect.LoadTopUp
        assertNull(g.systemBack()) // not while the card is being loaded
        assertTrue(g.dispatch(PosEvent.CancelPressed).isEmpty())
        assertTrue(g.state is PosState.TopUpLoading)
        g.dispatch(PosEvent.TopUpLoadDone(l.loadId, st("loaded", txid = "x")))
    }

    @Test fun onlyFromTheBalanceScreen() {
        val f = flow()
        f.dispatch(PosEvent.TopUpPressed)
        f.dispatch(PosEvent.ChargeCardPressed)
        assertEquals(PosState.Entering, f.state)
    }

    @Test fun timeLeftAndQrText() {
        assertEquals("10:00", timeLeft(600_000, 0))
        assertEquals("0:01", timeLeft(1_000, 1))
        assertEquals("0:00", timeLeft(1_000, 5_000))
        assertEquals("LIGHTNING:LNBC1X", invoiceQrText("lnbc1x"))
    }
}
