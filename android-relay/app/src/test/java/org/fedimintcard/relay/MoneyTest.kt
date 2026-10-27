package org.fedimintcard.relay

import org.junit.Assert.*
import org.junit.Test

class MoneyTest {
    @Test fun satsToMsat() = assertEquals(5000L, satsToMsat(5))
    @Test fun formatsWholeSats() { assertEquals("0", formatSats(0)); assertEquals("1,234,567", formatSats(1_234_567_000)) }
    @Test fun formatsFractions() { assertEquals("1.5", formatSats(1500)); assertEquals("0.001", formatSats(1)); assertEquals("12.034", formatSats(12_034)) }
    @Test fun shortTxidTrims() {
        assertEquals("01234567…cdef", shortTxid("0123456789abcdef"))
        assertEquals("abc", shortTxid("abc"))
    }
}
