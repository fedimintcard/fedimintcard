package org.fedimintcard.relay

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.time.ZoneId
import java.time.ZoneOffset
import java.time.ZonedDateTime

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

    // ENG-867: notes the till holds back as stranded are beside the payable balance, not in it.

    @Test
    fun readsWhatIsOnHoldBesideThePayableBalance() {
        val body = """{"ok":true,"live_msat":65536,"stranded_msat":196608,"stranded_until_ms":1798000000000,"notes":[
            {"slot":0,"msat":65536,"status":"live"},
            {"slot":1,"msat":131072,"status":"live","stranded":true},
            {"slot":2,"msat":65536,"status":"live","stranded":true}],"free_slots":[]}"""
        val b = (parseCardBalance(200, body) as BalanceResult.Read).balance
        assertEquals(65_536L, b.liveMsat)
        assertEquals(196_608L, b.strandedMsat)
        assertEquals(1_798_000_000_000L, b.strandedUntilMs)
        assertEquals(3, b.notes.size)
    }

    @Test
    fun aHoldWithNoDateToWaitForHasNone() {
        val b = (parseCardBalance(200, """{"ok":true,"live_msat":0,"stranded_msat":65536,"stranded_until_ms":null,"notes":[],"free_slots":[]}""") as BalanceResult.Read).balance
        assertEquals(65_536L, b.strandedMsat)
        assertNull(b.strandedUntilMs)
    }

    /** A till from before these fields sends neither: nothing on hold, so no line. */
    @Test
    fun anOlderTillReadsAsNothingOnHold() {
        val b = (parseCardBalance(200, """{"ok":true,"live_msat":481280,"notes":[],"free_slots":[6,7]}""") as BalanceResult.Read).balance
        assertEquals(481_280L, b.liveMsat)
        assertEquals(0L, b.strandedMsat)
        assertNull(b.strandedUntilMs)
        assertNull(onHoldLine(b.strandedMsat, b.strandedUntilMs, now, zone))
    }

    /** A value that is not a number reads as absent; the payable balance still shows. */
    @Test
    fun oddHoldValuesReadAsAbsent() {
        for (extra in listOf(""""stranded_msat":null""", """"stranded_msat":"65536"""", """"stranded_msat":{}""",
            """"stranded_msat":65536,"stranded_until_ms":"soon"""", """"stranded_msat":65536,"stranded_until_ms":[1]""")) {
            val r = parseCardBalance(200, """{"ok":true,"live_msat":5,$extra,"notes":[],"free_slots":[]}""")
            val b = (r as BalanceResult.Read).balance
            assertEquals(extra, 5L, b.liveMsat)
            assertEquals(extra, if (extra.startsWith(""""stranded_msat":65536""")) 65_536L else 0L, b.strandedMsat)
            assertNull(extra, b.strandedUntilMs)
        }
    }

    // The on-hold line, on a fixed clock in a fixed zone: Jamaica keeps UTC-5 all year.
    private val zone: ZoneId = ZoneId.of("America/Jamaica")
    private fun at(month: Int, day: Int, hour: Int, zone: ZoneId = this.zone): Long =
        ZonedDateTime.of(2026, month, day, hour, 0, 0, 0, zone).toInstant().toEpochMilli()
    private val now = at(12, 1, 12)

    @Test
    fun nothingOnHoldIsNoLine() {
        assertNull(onHoldLine(0, null, now, zone))
        assertNull(onHoldLine(0, at(12, 21, 15), now, zone))
        assertNull(onHoldLine(-65_536, at(12, 21, 15), now, zone))
    }

    @Test
    fun aHoldWithNoDateSaysOnlyHowMuch() {
        assertEquals("65.536 sats on hold", onHoldLine(65_536, null, now, zone))
    }

    /** Due now or already: the notes come back within a tap or two, so no date. */
    @Test
    fun aHoldThatIsNoLongerAheadHasNoDate() {
        assertEquals("200 sats on hold", onHoldLine(200_000, now, now, zone))
        assertEquals("200 sats on hold", onHoldLine(200_000, now - 1, now, zone))
        assertEquals("200 sats on hold", onHoldLine(200_000, at(11, 3, 9), now, zone))
    }

    /** The first whole day the notes can come back: a hold ending mid-afternoon names the next day. */
    @Test
    fun aHoldAheadNamesTheFirstWholeDayAfterIt() {
        assertEquals("12,345 sats on hold until Dec 22", onHoldLine(12_345_000, at(12, 21, 15), now, zone))
        assertEquals("200 sats on hold until Dec 2", onHoldLine(200_000, now + 1, now, zone))
        assertEquals("200 sats on hold until Jan 7", onHoldLine(200_000, ZonedDateTime.of(2027, 1, 6, 23, 59, 59, 0, zone).toInstant().toEpochMilli(), now, zone))
    }

    /** A hold ending exactly at local midnight names the day that midnight starts, and not a ms later. */
    @Test
    fun theMidnightEdge() {
        val midnight = at(12, 21, 0)
        assertEquals("200 sats on hold until Dec 21", onHoldLine(200_000, midnight, now, zone))
        assertEquals("200 sats on hold until Dec 22", onHoldLine(200_000, midnight + 1, now, zone))
        assertEquals("200 sats on hold until Dec 21", onHoldLine(200_000, midnight - 1, now, zone))
    }

    /** The day is the phone's: 03:00 UTC on Dec 22 is still Dec 21 in Jamaica. */
    @Test
    fun theDayIsInThePhonesTimeZone() {
        val t = at(12, 22, 3, ZoneOffset.UTC)
        assertEquals("200 sats on hold until Dec 22", onHoldLine(200_000, t, now, zone))
        assertEquals("200 sats on hold until Dec 23", onHoldLine(200_000, t, now, ZoneOffset.UTC))
    }

    @Test
    fun theScreenReaderHearsTheHoldAfterTheBalance() {
        assertEquals("Card balance: 1,000 sats on this card", balanceDescription(1_000_000, null))
        assertEquals(
            "Card balance: 1,000 sats on this card. 200 sats on hold until Dec 22",
            balanceDescription(1_000_000, onHoldLine(200_000, at(12, 21, 15), now, zone)),
        )
    }
}
