package org.fedimintcard.relay

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class CardBalanceTest {
    @Test
    fun readsTheTillsReply() {
        val body = """{"ok":true,"live_msat":481280,"notes":[
            {"slot":0,"msat":65536,"status":"live"},
            {"slot":3,"msat":262144,"status":"live"},
            {"slot":5,"msat":0,"status":"spent"}],"free_slots":[6,7]}"""
        val r = parseCardBalance(200, body)
        assertTrue(r is BalanceResult.Read)
        val b = (r as BalanceResult.Read).balance
        assertEquals(481280L, b.liveMsat)
        assertEquals(listOf(6, 7), b.freeSlots)
        assertEquals(CardNote(3, 262144, "live"), b.notes[1])
        assertEquals(2, b.notes.count { it.status == "live" })
    }

    /** A missing card is this route's normal answer, not a malformed body. */
    @Test
    fun aRefusalCarriesTheTillsCodeAndReason() {
        val r = parseCardBalance(409, """{"ok":false,"error":"no_card","detail":"no card on the relay"}""")
        assertEquals(BalanceResult.Refused(ChargeErrorCode.NO_CARD, "no card on the relay"), r)
    }

    @Test
    fun aRefusalWithNoDetailFallsBackToTheCode() {
        val r = parseCardBalance(409, """{"ok":false,"error":"busy"}""")
        assertEquals(BalanceResult.Refused(ChargeErrorCode.BUSY, "busy"), r)
    }

    @Test
    fun garbageIsABadResponse() {
        assertTrue((parseCardBalance(200, "not json") as BalanceResult.Failed).error is TillError.BadResponse)
    }

    private fun refused(c: ChargeErrorCode, d: String = "") = balanceProblem(BalanceResult.Refused(c, d))

    @Test
    fun problemsSortIntoWhatTheHolderCanDo() {
        assertEquals(BalanceProblem.CARD_MOVED, refused(ChargeErrorCode.NO_CARD))
        assertEquals(BalanceProblem.CARD_MOVED, refused(ChargeErrorCode.CARD_ERROR, "card relay at http://127.0.0.1:9876: POST /apdu: HTTP 503: tag lost"))
        assertEquals(BalanceProblem.NOT_OUR_CARD, refused(ChargeErrorCode.CARD_ERROR, "different card presented (slot 0 is empty)"))
        assertEquals(BalanceProblem.NOT_OUR_CARD, refused(ChargeErrorCode.CARD_ERROR, "GET_PUB: status word 6A82"))
        assertEquals(BalanceProblem.NOT_OUR_CARD, refused(ChargeErrorCode.UNKNOWN_CARD))
        assertEquals(BalanceProblem.RELAY_UNREACHABLE, refused(ChargeErrorCode.RELAY_UNREACHABLE))
        assertEquals(BalanceProblem.BUSY, refused(ChargeErrorCode.BUSY))
        assertEquals(BalanceProblem.BUSY, refused(ChargeErrorCode.RECOVERING))
        assertEquals(BalanceProblem.CARD_SERVICE, refused(ChargeErrorCode.OPS_UNREACHABLE))
        assertEquals(BalanceProblem.OTHER, refused(ChargeErrorCode.INTERNAL))
        assertEquals(BalanceProblem.TILL_UNREACHABLE, balanceProblem(BalanceResult.Failed(TillError.Unreachable)))
        assertEquals(BalanceProblem.TIMEOUT, balanceProblem(BalanceResult.Failed(TillError.Timeout)))
    }

    /** The holder reads this copy: no status words, slots, ports or adb commands. */
    @Test
    fun copyIsPlain() {
        for (p in BalanceProblem.values()) {
            val (title, detail) = balanceErrorCopy(p)
            assertTrue(title.isNotBlank())
            for (w in listOf("adb", "tcp", "slot", "msat", "APDU", "relay")) {
                assertFalse("$p mentions $w", (title + detail).contains(w, ignoreCase = true))
            }
        }
    }
}
