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
        f.dispatch(PosEvent.BalanceDone(fx.readId, BalanceResult.Read(CardBalance(5_000, emptyList(), emptyList(), "6f94b19b"))))
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

    @Test fun theCardFromTheBalanceReadGoesWithEveryCreate() {
        val f = shown()
        f.dispatch(PosEvent.TopUpPressed)
        "100".forEach { f.topUpPad.press(it - '0') }
        val c = f.dispatch(PosEvent.CreateInvoicePressed).single() as PosEffect.CreateTopUp
        assertEquals("6f94b19b", c.shortId)
        // A failed create retried, and a new invoice after expiry, keep the card.
        f.dispatch(PosEvent.TopUpCreated(c.createId, TopUpReply.Failed(TillError.Unreachable)))
        val again = f.dispatch(PosEvent.RetryPressed).single() as PosEffect.CreateTopUp
        assertEquals("6f94b19b", again.shortId)
        f.dispatch(PosEvent.TopUpCreated(again.createId, TopUpReply.Ok(t)))
        f.dispatch(PosEvent.TopUpPolled("t1", st("expired")))
        val renewed = f.dispatch(PosEvent.RetryPressed).single() as PosEffect.CreateTopUp
        assertEquals("6f94b19b", renewed.shortId)
    }

    @Test fun aFullCardIsRefusedBeforeAnyInvoice() {
        val f = shown()
        f.dispatch(PosEvent.TopUpPressed)
        "100".forEach { f.topUpPad.press(it - '0') }
        val c = f.dispatch(PosEvent.CreateInvoicePressed).single() as PosEffect.CreateTopUp
        f.dispatch(PosEvent.TopUpCreated(c.createId, TopUpReply.Refused("card_full", "no free slot", 409)))
        val s = f.state as PosState.TopUpFailed
        assertEquals(TopUpProblem.NO_ROOM, s.problem)
        assertFalse(s.paid); assertNull(s.topUp)
        assertEquals("This card is full" to "Spend from it first, then top up.", topUpErrorCopy(s.problem, s.paid, s.detail))
        assertTrue(f.dispatch(PosEvent.RetryPressed).isEmpty())
        f.dispatch(PosEvent.DonePressed)
        assertEquals(PosState.Entering, f.state)
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

    // ---- waiting top-ups (docs/TOPUP.md § Waiting top-ups)

    private val w1 = WaitingTopUp("w1", 100_352, null, 1)
    private val w2 = WaitingTopUp("w2", 65_536, "6f94b19b", 2)

    /** The balance read for card 6f94b19b, and the fetch it asked for. */
    private fun shownAsking(lift: Boolean = true): Pair<PosFlow, PosEffect.FetchWaiting> {
        val f = flow()
        f.dispatch(PosEvent.CardPresent)
        val fx = f.dispatch(PosEvent.CardBalanceOpened).single() as PosEffect.StartBalanceRead
        val ask = f.dispatch(PosEvent.BalanceDone(fx.readId, BalanceResult.Read(CardBalance(5_000, emptyList(), emptyList(), "6f94b19b")))).single() as PosEffect.FetchWaiting
        if (lift) f.dispatch(PosEvent.CardLost)
        return f to ask
    }

    @Test fun aBalanceReadAsksForWaitingTopUpsWithoutDelayingTheBalance() {
        val (f, ask) = shownAsking()
        assertEquals("6f94b19b", ask.shortId)
        val s = f.state as PosState.BalanceShown
        assertEquals(5_000L, s.liveMsat); assertNull(s.waiting)
        f.dispatch(PosEvent.WaitingFetched(ask.readId, listOf(w1, w2)))
        assertEquals(w1, (f.state as PosState.BalanceShown).waiting) // the oldest
    }

    @Test fun aReadWithoutACardIdAsksForNothing() {
        val f = flow()
        f.dispatch(PosEvent.CardPresent)
        val fx = f.dispatch(PosEvent.CardBalanceOpened).single() as PosEffect.StartBalanceRead
        assertTrue(f.dispatch(PosEvent.BalanceDone(fx.readId, BalanceResult.Read(CardBalance(5_000, emptyList(), emptyList(), null)))).isEmpty())
    }

    @Test fun noneOrAFailedFetchShowsNoButtonAndStaleRepliesAreIgnored() {
        val (f, ask) = shownAsking()
        f.dispatch(PosEvent.WaitingFetched(ask.readId, null))
        assertNull((f.state as PosState.BalanceShown).waiting)
        f.dispatch(PosEvent.WaitingFetched(ask.readId, emptyList()))
        assertNull((f.state as PosState.BalanceShown).waiting)
        f.dispatch(PosEvent.WaitingFetched(ask.readId + 99, listOf(w1)))
        assertNull((f.state as PosState.BalanceShown).waiting)
        f.dispatch(PosEvent.DonePressed)
        f.dispatch(PosEvent.WaitingFetched(ask.readId, listOf(w1))) // the balance screen is gone
        assertEquals(PosState.Entering, f.state)
        assertTrue(f.dispatch(PosEvent.LoadWaitingPressed).isEmpty())
    }

    @Test fun loadingAWaitingTopUpTakesThePaidPath() {
        val (f, ask) = shownAsking()
        f.dispatch(PosEvent.WaitingFetched(ask.readId, listOf(w1)))
        assertTrue(f.dispatch(PosEvent.LoadWaitingPressed).isEmpty())
        val waiting = f.state as PosState.TopUpWaitingForCard
        assertEquals("w1", waiting.topUp.topupId); assertEquals(100_352L, waiting.topUp.amountMsat)
        val l = f.dispatch(PosEvent.CardPresent).single() as PosEffect.LoadTopUp
        assertEquals("w1", l.topupId)
        f.dispatch(PosEvent.TopUpLoadDone(l.loadId, TopUpReply.Ok(TopUpStatus("w1", 100_352, "loaded", "tx", null))))
        assertEquals(PosState.TopUpLoaded(100_352, "tx"), f.state)
    }

    @Test fun aWaitingTopUpLoadsAtOnceWhileTheCardIsOnThePhone() {
        val (f, ask) = shownAsking(lift = false)
        f.dispatch(PosEvent.WaitingFetched(ask.readId, listOf(w2)))
        val l = f.dispatch(PosEvent.LoadWaitingPressed).single() as PosEffect.LoadTopUp
        assertEquals("w2", l.topupId)
        // A full card: the top-up stays paid and the tap can be tried again.
        f.dispatch(PosEvent.TopUpLoadDone(l.loadId, TopUpReply.Refused("card_full", "", 409)))
        val failed = f.state as PosState.TopUpFailed
        assertEquals(TopUpProblem.CARD_FULL, failed.problem); assertTrue(failed.paid)
        assertTrue(f.dispatch(PosEvent.RetryPressed).single() is PosEffect.LoadTopUp)
    }
}
