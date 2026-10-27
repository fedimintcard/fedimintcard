package org.fedimintcard.relay

import org.junit.Assert.*
import org.junit.Test

class AmountPadTest {
    private fun pad(s: String) = AmountPad().also { p -> s.forEach { p.press(it - '0') } }

    @Test fun startsEmptyAndNotChargeable() { val p = AmountPad(); assertEquals(0L, p.sats); assertFalse(p.isChargeable); assertEquals("0", p.display) }
    @Test fun buildsAmount() { val p = pad("1250"); assertEquals(1250L, p.sats); assertEquals(1_250_000L, p.msat); assertEquals("1,250", p.display); assertTrue(p.isChargeable) }
    @Test fun stripsLeadingZeros() { val p = pad("0007"); assertEquals(7L, p.sats); assertFalse(pad("000").isChargeable) }
    @Test fun capsAtNineDigits() { val p = pad("1234567890"); assertEquals(123_456_789L, p.sats) }
    @Test fun backspaceAndClear() { val p = pad("123"); p.backspace(); assertEquals(12L, p.sats); p.clear(); assertEquals(0L, p.sats); p.backspace(); assertEquals(0L, p.sats) }
}
