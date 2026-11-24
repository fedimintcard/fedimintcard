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
            declined(ChargeErrorCode.UNKNOWN) to ErrorKind.TIMEOUT_UNKNOWN, // the till may have taken the sale
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
            ChargeResult.Failed(TillError.Timeout), declined(ChargeErrorCode.UNDECIDED), ChargeResult.Failed(TillError.BadResponse("x")),
            // Refused before the till looks at the earlier attempt: they prove nothing about it.
            declined(ChargeErrorCode.fromWire("ops_not_connected")), declined(ChargeErrorCode.fromWire("unauthorized")),
            declined(ChargeErrorCode.fromWire("forbidden")))) {
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

    /**
     * An answer from the till that does not say whether it took the sale leaves the sale
     * unresolved, as a lost reply does: "Outcome unknown", no Back, Try again asks the PIN for the
     * same sale (the same client_ref), and Abandon clears the sale and the amount.
     */
    private fun assertUnresolvedSale(answer: ChargeResult) {
        val (f, fx) = unresolved(answer)
        val e = f.state as PosState.Error
        assertEquals("$answer", ErrorKind.TIMEOUT_UNKNOWN, e.kind)
        assertEquals("$answer", fx.clientRef, e.unresolvedRef); assertFalse("$answer", e.held)
        assertEquals("$answer", "Outcome unknown", errorCopy(e.kind, e.detail, e.suggestedMsat, unresolved = true).first)
        f.dispatch(PosEvent.BackPressed); assertEquals("$answer: no Back while the outcome is unknown", e, f.state)
        assertNull("$answer", f.systemBack())
        f.dispatch(PosEvent.RetryPressed)
        assertEquals("$answer", PosState.EnteringPin(5000, fx.clientRef, returnTo = e), f.state)
        f.typePin()
        val again = f.dispatch(PosEvent.PinConfirmed).single() as PosEffect.StartCharge
        assertEquals("$answer: Try again sends the same client_ref", fx.clientRef, again.clientRef)
        f.dispatch(PosEvent.ChargeDone(again.chargeId, answer))
        assertEquals("$answer", fx.clientRef, f.unresolvedRef)
        f.dispatch(PosEvent.AbandonPressed)
        assertEquals("$answer", PosState.Entering, f.state); assertNull("$answer", f.unresolvedRef)
        assertEquals("$answer: Abandon clears the amount", 0L, f.pad.sats)
    }

    /**
     * A 500 `internal`: the till may have taken the sale (an error after the federation accepted
     * it, or the till service stopping before it answered).
     */
    @Test fun anInternalAnswerFromTheTillIsAnUnresolvedSale() {
        assertUnresolvedSale(ChargeResult.Declined(ChargeErrorCode.INTERNAL, "the till service stopped before answering", 500))
    }

    /** A code the app does not know (a newer till, or JSON from something in front of the till), whatever its status. */
    @Test fun aCodeTheAppDoesNotKnowIsAnUnresolvedSale() {
        assertUnresolvedSale(ChargeResult.Declined(ChargeErrorCode.UNKNOWN, "", 500))
        assertUnresolvedSale(ChargeResult.Declined(ChargeErrorCode.fromWire("a_code_from_a_newer_till"), "something new", 409))
    }

    /**
     * Refusals of a charge that ran nothing (docs/TILL-API.md): `ops_not_connected` (a till that
     * belongs on a registry and has no token yet), `unauthorized` (no bearer the till takes) and
     * `forbidden` (a token with the pay scope). Each reads as before, in the till's own words with
     * Back, and leaves no sale unresolved.
     */
    @Test fun opsNotConnectedUnauthorizedAndForbiddenArePlainRefusals() {
        for ((wire, status) in listOf("ops_not_connected" to 503, "unauthorized" to 401, "forbidden" to 403)) {
            val (f, _) = unresolved(ChargeResult.Declined(ChargeErrorCode.fromWire(wire), "the till's words", status))
            val e = f.state as PosState.Error
            assertEquals(wire, ErrorKind.UNKNOWN, e.kind)
            assertNull(wire, e.unresolvedRef); assertNull(wire, f.unresolvedRef)
            assertEquals(wire, "Something went wrong" to "the till's words", errorCopy(e.kind, e.detail, e.suggestedMsat, unresolved = false))
            f.dispatch(PosEvent.BackPressed); assertEquals(wire, PosState.Entering, f.state)
        }
    }

    /**
     * Every other code the app knows is a refusal of this request, as before: nothing is left
     * unresolved and Back goes back to the amount. A code added to [ChargeErrorCode] lands here, so
     * whether it leaves a sale unresolved is decided when it is added.
     */
    @Test fun everyOtherCodeTheAppKnowsStillOffersBack() {
        val unclear = setOf(ChargeErrorCode.UNDECIDED, ChargeErrorCode.INTERNAL, ChargeErrorCode.UNKNOWN)
        for (c in ChargeErrorCode.values()) {
            if (c in unclear || c == ChargeErrorCode.WRONG_PIN) continue // a wrong PIN asks for it again
            val (f, _) = unresolved(declined(c))
            val e = f.state as PosState.Error
            assertNull(c.name, e.unresolvedRef)
            f.dispatch(PosEvent.BackPressed); assertEquals(c.name, PosState.Entering, f.state)
        }
    }

    @Test fun pollGenerationSupersedes() {
        val g = PollGeneration()
        val a = g.next(); val b = g.next()
        assertFalse(g.isCurrent(a)); assertTrue(g.isCurrent(b))
        g.cancel(); assertFalse(g.isCurrent(b))
    }

    // ---- the phone's own charge (docs/POS-OFFLINE.md § Android) ----

    private fun local(c: ChargeErrorCode, held: Boolean = false, detail: String = "d", suggested: Long? = null, wrongTries: Int? = null, waitS: Int? = null) =
        ChargeResult.Declined(c, detail, 0, suggested, held = held, local = true, wrongTries = wrongTries, waitS = waitS)

    /**
     * The rule for an unclear answer is the till's only. A sale the phone's own core may have
     * signed is held on the phone, so its `internal`, or a code the app does not know, with nothing
     * held means nothing was signed: a plain error with Back, as before.
     */
    @Test fun aLocalInternalRefusalIsNotAnUnclearTillAnswer() {
        for (c in listOf(ChargeErrorCode.INTERNAL, ChargeErrorCode.UNKNOWN)) {
            val (f, _) = unresolved(local(c))
            val e = f.state as PosState.Error
            assertEquals(c.name, ErrorKind.from(c), e.kind); assertNull(c.name, e.unresolvedRef); assertFalse(c.name, e.held)
            f.dispatch(PosEvent.BackPressed); assertEquals(c.name, PosState.Entering, f.state)
        }
    }

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
        f.dispatch(PosEvent.ChargeDone(f.signingId(), local(ChargeErrorCode.WRONG_PIN, held = true, wrongTries = 2, waitS = 2)))
        val ask = f.state as PosState.EnteringPin
        assertTrue(ask.wrongPin); assertEquals(2, ask.wrongTries); assertEquals(fx.clientRef, ask.clientRef)
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

    /** The card is serving its wait and was lifted: the wait is said, the sale stays held, Try again is there. */
    @Test fun aHeldPinWaitSaysTheWaitAndStaysHeldAndRetryable() {
        val (f, fx) = unresolved(local(ChargeErrorCode.UNDECIDED, held = true))
        f.retry()
        f.dispatch(PosEvent.ChargeDone(f.signingId(), local(ChargeErrorCode.PIN_WAIT, held = true, waitS = 9)))
        val e = f.state as PosState.Error
        assertEquals(ErrorKind.PIN_WAIT, e.kind); assertEquals(9, e.waitS); assertTrue(e.held); assertEquals(fx.clientRef, e.unresolvedRef)
        assertTrue(e.kind.retryable)
        assertNull(f.systemBack()); assertTrue(f.unresolvedHeld)
        f.dispatch(PosEvent.AbandonPressed); assertEquals(PosState.Entering, f.state); assertFalse(f.unresolvedHeld)
    }

    /** A held sale may already be signed in part: a refused signature on its retap keeps it held and never says nothing was spent. */
    @Test fun aHeldRefusedSignatureStaysHeldAndNeverSaysNothingWasSpent() {
        val (f, fx) = unresolved(local(ChargeErrorCode.UNDECIDED, held = true))
        f.retry()
        f.dispatch(PosEvent.ChargeDone(f.signingId(), local(ChargeErrorCode.SIGN_CHECK_FAILED, held = true)))
        val e = f.state as PosState.Error
        assertEquals(ErrorKind.HELD_ON_PHONE, e.kind); assertTrue(e.held); assertEquals(fx.clientRef, e.unresolvedRef)
        assertTrue(e.kind.retryable)
        assertFalse(f.saysNothingWasSpent())
    }

    @Test fun aWrongPinOnALocalChargeThatHoldsNothingIsAFreshAttempt() {
        val (f, _) = unresolved(local(ChargeErrorCode.WRONG_PIN, wrongTries = 2, waitS = 2))
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

    // ---- what may say "Nothing was spent" ----

    private val nothingSpent = Regex("nothing was spent", RegexOption.IGNORE_CASE)
    private fun PosFlow.saysNothingWasSpent(): Boolean {
        val e = state as? PosState.Error ?: return false
        val (t, d) = errorCopy(e.kind, e.detail, e.suggestedMsat, e.unresolvedRef != null, e.waitS)
        return nothingSpent.containsMatchIn("$t\n$d")
    }

    /** A lost reply leaves the sale unresolved. An old card tapped on Try again says nothing about the first card's attempt. */
    @Test fun anOldCardOnTheRetryOfALostReplyLeavesTheSaleUnresolved() {
        val (f, fx) = unresolved(ChargeResult.Failed(TillError.Timeout))
        f.retry()
        f.dispatch(PosEvent.ChargeDone(f.signingId(), declined(ChargeErrorCode.OLD_CARD)))
        val e = f.state as PosState.Error
        assertEquals(ErrorKind.OLD_CARD, e.kind); assertEquals(fx.clientRef, e.unresolvedRef)
        assertEquals(fx.clientRef, f.unresolvedRef)
        f.dispatch(PosEvent.BackPressed); assertTrue(f.state is PosState.Error)
        assertNull(f.systemBack())
        // Abandon still takes the merchant back to the amount, with the sale cleared.
        f.dispatch(PosEvent.AbandonPressed); assertEquals(PosState.Entering, f.state)
    }

    @Test fun onlyARefusedSignatureOutsideAHeldSaleSaysNothingWasSpent() {
        val answers = mutableListOf<Pair<ChargeResult, Boolean>>()
        for (c in ChargeErrorCode.values()) {
            answers += declined(c) to (c == ChargeErrorCode.SIGN_CHECK_FAILED)
            answers += local(c) to (c == ChargeErrorCode.SIGN_CHECK_FAILED)
        }
        for (te in listOf(TillError.Unreachable, TillError.Timeout, TillError.Interrupted, TillError.BadResponse("x"))) answers += ChargeResult.Failed(te) to false
        for ((r, says) in answers) {
            val (f, _) = unresolved(r)
            assertEquals("$r", says, f.saysNothingWasSpent())
        }
        // A sale held on the phone, whatever its retap answers.
        for (c in ChargeErrorCode.values()) {
            val (f, _) = unresolved(local(ChargeErrorCode.UNDECIDED, held = true))
            f.retry()
            f.dispatch(PosEvent.ChargeDone(f.signingId(), local(c, held = true)))
            assertFalse("held $c", f.saysNothingWasSpent())
        }
        // Every kind's copy, with and without an unresolved sale.
        for (k in ErrorKind.values()) for (u in listOf(false, true)) {
            val (t, d) = errorCopy(k, "d", 1000, u, 3, 3)
            assertEquals("$k", k == ErrorKind.SIGN_CHECK_FAILED, nothingSpent.containsMatchIn("$t\n$d"))
        }
    }
}
