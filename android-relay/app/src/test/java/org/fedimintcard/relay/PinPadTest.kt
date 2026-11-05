package org.fedimintcard.relay

import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class PinPadTest {
    private fun PinPad.type(s: String) = s.forEach { press(it - '0') }

    @Test fun completeOnlyBetweenFourAndEightDigits() {
        val p = PinPad()
        p.type("123"); assertFalse(p.isComplete); assertNull(p.take())
        p.press(4); assertTrue(p.isComplete)
        p.type("5678"); assertEquals(8, p.length); assertTrue(p.isComplete)
        p.press(9); assertEquals("a ninth digit is ignored", 8, p.length)
    }
    @Test fun digitsOnly() {
        val p = PinPad()
        p.press(-1); p.press(10); p.press(42)
        assertEquals(0, p.length)
    }
    @Test fun backspaceAndClear() {
        val p = PinPad()
        p.backspace(); assertEquals(0, p.length)
        p.type("1234"); p.backspace(); assertEquals(3, p.length); assertFalse(p.isComplete)
        p.clear(); assertEquals(0, p.length)
    }
    @Test fun neverShowsTheDigits() {
        val p = PinPad(); p.type("4321")
        assertEquals("●●●●", p.masked)
        assertFalse(p.toString().contains("4321"))
        val pin = p.take()!!
        assertEquals("Pin(****)", pin.toString())
    }
    @Test fun takeMovesTheDigitsOutAndClearsThePad() {
        val p = PinPad(); p.type("1234")
        val pin = p.take()!!
        assertEquals(0, p.length)
        assertEquals("1234", pin.withChars { String(it) })
        pin.wipe(); assertTrue(pin.isWiped)
        assertEquals("", pin.withChars { String(it) })
    }
}

class PinFlowTest {
    private var n = 0
    private fun flow(sats: Int = 5): PosFlow = PosFlow { "ref${++n}" }.also { f -> sats.toString().forEach { f.pad.press(it - '0') } }
    private fun PosFlow.typePin(p: String = "1234") = p.forEach { pinPad.press(it - '0') }
    private fun wrongPin(detail: String) = ChargeResult.Declined(ChargeErrorCode.WRONG_PIN, detail, 403)
    private val receipt = ChargeReceipt(1, "t", 5000, 5000, 0, 0, emptyList(), emptyList(), false, emptyList(), 1)

    /** Card on the phone, amount entered, PIN typed and confirmed: the charge effect. */
    private fun signed(f: PosFlow = flow(), pin: String = "1234"): Pair<PosFlow, PosEffect.StartCharge> {
        f.dispatch(PosEvent.CardPresent)
        f.dispatch(PosEvent.ChargePressed)
        f.typePin(pin)
        return f to f.dispatch(PosEvent.PinConfirmed).single() as PosEffect.StartCharge
    }

    @Test fun chargeAsksForThePinBeforeTheCard() {
        val f = flow(); f.dispatch(PosEvent.CardPresent)
        assertTrue(f.dispatch(PosEvent.ChargePressed).isEmpty())
        val s = f.state as PosState.EnteringPin
        assertEquals(5000L, s.priceMsat); assertTrue(s.fresh); assertFalse(s.wrongPin)
        assertEquals(PosEvent.CancelPressed, f.systemBack())
    }
    @Test fun confirmNeedsFourDigits() {
        val f = flow(); f.dispatch(PosEvent.CardPresent); f.dispatch(PosEvent.ChargePressed)
        f.typePin("123")
        assertTrue(f.dispatch(PosEvent.PinConfirmed).isEmpty())
        assertTrue(f.state is PosState.EnteringPin)
    }
    @Test fun theEffectCarriesThePadsPinAndTheFlowKeepsNone() {
        val (f, fx) = signed(pin = "98765")
        assertEquals("98765", fx.pin!!.withChars { String(it) })
        assertFalse(f.holdsPin)
        assertFalse(fx.toString().contains("98765"))
        assertFalse(f.state.toString().contains("98765"))
    }
    @Test fun pinHeldWhileWaitingIsWipedOnCancel() {
        val f = flow(); f.dispatch(PosEvent.ChargePressed); f.typePin()
        assertTrue(f.dispatch(PosEvent.PinConfirmed).isEmpty())
        assertTrue(f.state is PosState.WaitingForCard); assertTrue(f.holdsPin)
        f.dispatch(PosEvent.CancelPressed)
        assertEquals(PosState.Entering, f.state); assertFalse(f.holdsPin)
        assertEquals("the amount stays", 5L, f.pad.sats)
    }
    @Test fun cardArrivingAfterThePinSendsIt() {
        val f = flow(); f.dispatch(PosEvent.ChargePressed); f.typePin("2468"); f.dispatch(PosEvent.PinConfirmed)
        val fx = f.dispatch(PosEvent.CardPresent).single() as PosEffect.StartCharge
        assertEquals("2468", fx.pin!!.withChars { String(it) })
    }
    @Test fun cancelOnThePadWipesAndReturnsToTheAmount() {
        val f = flow(); f.dispatch(PosEvent.ChargePressed); f.typePin("12")
        f.dispatch(PosEvent.CancelPressed)
        assertEquals(PosState.Entering, f.state); assertEquals(0, f.pinPad.length); assertEquals(5L, f.pad.sats)
    }
    @Test fun backgroundWipesTypedAndHeldPins() {
        val f = flow(); f.dispatch(PosEvent.ChargePressed); f.typePin("12")
        f.dispatch(PosEvent.Backgrounded)
        assertEquals(PosState.Entering, f.state); assertFalse(f.holdsPin)
        f.dispatch(PosEvent.ChargePressed); f.typePin(); f.dispatch(PosEvent.PinConfirmed)
        assertTrue(f.state is PosState.WaitingForCard)
        f.dispatch(PosEvent.Backgrounded)
        assertEquals(PosState.Entering, f.state); assertFalse(f.holdsPin)
    }
    @Test fun wrongPinReturnsToThePadForTheSameSaleAndRef() {
        val (f, fx) = signed()
        f.dispatch(PosEvent.ChargeDone(fx.chargeId, wrongPin("wrong PIN; 2 tries left before the card blocks")))
        val s = f.state as PosState.EnteringPin
        assertEquals(5000L, s.priceMsat); assertTrue(s.wrongPin); assertEquals(2, s.triesLeft)
        assertEquals("same ref: a refused PIN moved nothing", fx.clientRef, s.clientRef)
        assertNull(f.unresolvedRef); assertFalse(f.holdsPin)
        f.typePin("4321")
        // The card is still on the phone; the holder's confirm is the go-ahead, no lift needed.
        val again = f.dispatch(PosEvent.PinConfirmed).single() as PosEffect.StartCharge
        assertEquals(fx.clientRef, again.clientRef); assertNotEquals(fx.chargeId, again.chargeId)
        assertEquals("4321", again.pin!!.withChars { String(it) })
    }
    @Test fun wrongPinWithoutTriesSaysWrongPin() {
        val (f, fx) = signed()
        f.dispatch(PosEvent.ChargeDone(fx.chargeId, wrongPin("")))
        val s = f.state as PosState.EnteringPin
        assertTrue(s.wrongPin); assertNull(s.triesLeft)
    }
    @Test fun wrongPinWithNoTriesLeftIsBlocked() {
        val (f, fx) = signed()
        f.dispatch(PosEvent.ChargeDone(fx.chargeId, wrongPin("wrong PIN; 0 tries left before the card blocks")))
        assertEquals(ErrorKind.PIN_BLOCKED, (f.state as PosState.Error).kind)
    }
    @Test fun cancelAfterWrongPinGoesToTheAmount() {
        val (f, fx) = signed()
        f.dispatch(PosEvent.ChargeDone(fx.chargeId, wrongPin("1 try left")))
        f.dispatch(PosEvent.CancelPressed)
        assertEquals(PosState.Entering, f.state); assertEquals(5L, f.pad.sats)
    }
    @Test fun pinBlockedCannotBeRetriedAndBackGoesToTheAmount() {
        val (f, fx) = signed()
        f.dispatch(PosEvent.ChargeDone(fx.chargeId, ChargeResult.Declined(ChargeErrorCode.PIN_BLOCKED, "blocked", 423)))
        val e = f.state as PosState.Error
        assertEquals(ErrorKind.PIN_BLOCKED, e.kind); assertNull(e.unresolvedRef); assertFalse(e.kind.retryable)
        assertTrue(f.dispatch(PosEvent.RetryPressed).isEmpty()); assertSame(e, f.state)
        f.dispatch(PosEvent.BackPressed); assertEquals(PosState.Entering, f.state)
    }
    @Test fun wrongPinClearsAnUnresolvedSale() {
        val (f, fx) = signed()
        f.dispatch(PosEvent.ChargeDone(fx.chargeId, ChargeResult.Failed(TillError.Timeout)))
        assertEquals(fx.clientRef, f.unresolvedRef)
        f.dispatch(PosEvent.RetryPressed); f.typePin(); val again = f.dispatch(PosEvent.PinConfirmed).single() as PosEffect.StartCharge
        f.dispatch(PosEvent.ChargeDone(again.chargeId, wrongPin("2 tries left")))
        assertTrue(f.state is PosState.EnteringPin); assertNull(f.unresolvedRef)
    }
    @Test fun pinRequiredKeepsAnUnresolvedSale() {
        val (f, fx) = signed()
        f.dispatch(PosEvent.ChargeDone(fx.chargeId, ChargeResult.Failed(TillError.Timeout)))
        f.dispatch(PosEvent.RetryPressed); f.typePin(); val again = f.dispatch(PosEvent.PinConfirmed).single() as PosEffect.StartCharge
        f.dispatch(PosEvent.ChargeDone(again.chargeId, ChargeResult.Declined(ChargeErrorCode.PIN_REQUIRED, "", 400)))
        assertEquals(ErrorKind.PIN_REQUIRED, (f.state as PosState.Error).kind)
        assertEquals("answered before the till settles: proves nothing", fx.clientRef, f.unresolvedRef)
    }
    @Test fun unresolvedTryAgainAsksForThePinAndCancelGoesBackToTheError() {
        val (f, fx) = signed()
        f.dispatch(PosEvent.ChargeDone(fx.chargeId, ChargeResult.Failed(TillError.Timeout)))
        val err = f.state as PosState.Error
        assertTrue(f.dispatch(PosEvent.RetryPressed).isEmpty())
        assertEquals(err, (f.state as PosState.EnteringPin).returnTo)
        assertEquals("sales polling continues", fx.clientRef, f.unresolvedRef)
        f.dispatch(PosEvent.CancelPressed); assertEquals(err, f.state)
        f.dispatch(PosEvent.RetryPressed); f.typePin(); f.dispatch(PosEvent.Backgrounded)
        assertEquals(err, f.state); assertFalse(f.holdsPin)
    }
    @Test fun saleFoundWhileOnThePadWipesIt() {
        val (f, fx) = signed()
        f.dispatch(PosEvent.ChargeDone(fx.chargeId, ChargeResult.Failed(TillError.Timeout)))
        f.dispatch(PosEvent.RetryPressed); f.typePin("12")
        f.dispatch(PosEvent.SaleFound(Sale(4, 1, 5000, 5000, 0, 0, "tx", fx.clientRef)))
        assertTrue(f.state is PosState.Receipt); assertFalse(f.holdsPin)
    }
    @Test fun successLeavesNoPin() {
        val (f, fx) = signed()
        f.dispatch(PosEvent.ChargeDone(fx.chargeId, ChargeResult.Success(receipt)))
        assertTrue(f.state is PosState.Receipt); assertFalse(f.holdsPin)
    }
}

class PinRequestTest {
    private class Fake(val resp: RawResponse? = null, val ex: Exception? = null) : Transport {
        var body: String? = null
        override fun send(method: String, url: String, body: String?, connectMs: Int, readMs: Int): RawResponse {
            this.body = body
            ex?.let { throw it }
            return resp!!
        }
    }
    private fun pinOf(s: String): Pin = PinPad().apply { s.forEach { press(it - '0') } }.take()!!

    @Test fun pinFromThePadIsSentAndWipedAfter() {
        val f = Fake(RawResponse(403, """{"ok":false,"error":"wrong_pin","detail":"wrong PIN; 2 tries left before the card blocks"}"""))
        val pin = pinOf("13579")
        val r = TillClient({ "http://h:1" }, f).charge(5000, "ref-1", pin) as ChargeResult.Declined
        assertEquals(ChargeErrorCode.WRONG_PIN, r.code)
        val o = JSONObject(f.body!!)
        assertEquals("13579", o.getString("pin")); assertEquals("ref-1", o.getString("client_ref")); assertEquals(5000L, o.getLong("price_msat"))
        assertTrue(pin.isWiped)
    }
    @Test fun pinIsWipedWhenTheRequestFails() {
        val pin = pinOf("1234")
        TillClient({ "http://h:1" }, Fake(ex = java.io.IOException("x"))).charge(1, "r", pin)
        assertTrue(pin.isWiped)
    }
    @Test fun noPinNoKey() {
        val f = Fake(RawResponse(400, """{"ok":false,"error":"pin_required","detail":"x"}"""))
        val r = TillClient({ "http://h:1" }, f).charge(1, "r") as ChargeResult.Declined
        assertFalse(JSONObject(f.body!!).has("pin"))
        assertEquals(ChargeErrorCode.PIN_REQUIRED, r.code)
    }
    @Test fun pinErrorCodesParse() {
        assertEquals(ChargeErrorCode.PIN_BLOCKED, (parseCharge(423, """{"ok":false,"error":"pin_blocked","detail":"d"}""") as ChargeResult.Declined).code)
        assertEquals(ErrorKind.WRONG_PIN, ErrorKind.from(ChargeErrorCode.WRONG_PIN))
    }
    @Test fun triesLeftParse() {
        assertEquals(2, parseTriesLeft("wrong PIN; 2 tries left before the card blocks"))
        assertEquals(1, parseTriesLeft("1 try left"))
        assertEquals(0, parseTriesLeft("wrong PIN; 0 tries left before the card blocks"))
        assertNull(parseTriesLeft("wrong PIN"))
        assertNull(parseTriesLeft(""))
    }
    @Test fun wrongPinAndBlockedCopy() {
        assertEquals("Wrong PIN — 2 tries left", wrongPinCopy(2))
        assertEquals("Wrong PIN — 1 try left", wrongPinCopy(1))
        assertEquals("Wrong PIN", wrongPinCopy(null))
        assertEquals("Wrong PIN — 2 tries left", errorCopy(ErrorKind.WRONG_PIN, "wrong PIN; 2 tries left before the card blocks").first)
        assertEquals("Wrong PIN", errorCopy(ErrorKind.WRONG_PIN, "garbled").first)
        assertEquals("This card's PIN is blocked. The card can't pay.", errorCopy(ErrorKind.PIN_BLOCKED, "").second)
        assertTrue(errorCopy(ErrorKind.PIN_REQUIRED, "").second.contains("app bug"))
    }
}
