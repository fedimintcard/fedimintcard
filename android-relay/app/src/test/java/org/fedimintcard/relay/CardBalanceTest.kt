package org.fedimintcard.relay

import org.junit.Assert.assertEquals
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
        assertTrue(r is TillResult.Ok)
        val b = (r as TillResult.Ok).value
        assertEquals(481280L, b.liveMsat)
        assertEquals(listOf(6, 7), b.freeSlots)
        assertEquals(CardNote(3, 262144, "live"), b.notes[1])
        assertEquals(2, b.notes.count { it.status == "live" })
    }

    /** A missing card is this route's normal answer, not a malformed body. */
    @Test
    fun aRefusalCarriesTheTillsReason() {
        val r = parseCardBalance(409, """{"ok":false,"error":"no_card","detail":"no card on the relay"}""")
        assertTrue(r is TillResult.Err)
        assertEquals("no card on the relay", ((r as TillResult.Err).error as TillError.BadResponse).detail)
    }

    @Test
    fun aRefusalWithNoDetailFallsBackToTheCode() {
        val r = parseCardBalance(409, """{"ok":false,"error":"busy"}""")
        assertEquals("busy", ((r as TillResult.Err).error as TillError.BadResponse).detail)
    }

    @Test
    fun garbageIsABadResponse() {
        assertTrue(parseCardBalance(200, "not json") is TillResult.Err)
    }

    @Test
    fun copyNamesTheFixForAMissingCard() {
        val e = TillError.BadResponse("card relay at http://127.0.0.1:9876: POST /apdu: HTTP 503: {\"error\":\"no card\"}")
        assertTrue(cardBalanceErrorCopy(e).startsWith("No card on the phone"))
        assertEquals(
            "Till service unreachable — check adb reverse tcp:9877 tcp:9877",
            cardBalanceErrorCopy(TillError.Unreachable),
        )
    }
}
