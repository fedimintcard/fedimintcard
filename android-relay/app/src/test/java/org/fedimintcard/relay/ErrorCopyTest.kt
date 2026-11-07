package org.fedimintcard.relay

import org.junit.Assert.assertEquals
import org.junit.Test

class ErrorCopyTest {
    /**
     * A status word is a card-protocol fault, not a missing card: the till reported it after the
     * card answered. Telling a cashier to reposition the card sends them after the wrong cause, so
     * the word stays visible for whoever reads the log.
     */
    @Test
    fun cardErrorKeepsTheStatusWordVisible() {
        val (title, detail) = errorCopy(ErrorKind.CARD_ERROR, "card returned status word 6E00")
        assertEquals("Card error", title)
        assertEquals("card returned status word 6E00", detail)
    }

    @Test
    fun onlyNoCardBlamesTheCardPosition() {
        assertEquals("Card not detected", errorCopy(ErrorKind.NO_CARD, "").first)
        assertEquals("Card error", errorCopy(ErrorKind.CARD_ERROR, "6E00").first)
    }

    @Test
    fun otherKindsAreUnchanged() {
        assertEquals("Card declined", errorCopy(ErrorKind.DECLINED, "nope").first)
        assertEquals("Till is busy", errorCopy(ErrorKind.BUSY, "").first)
    }

    /**
     * Offline with no slot for change is not a full card: "Card needs a top up" sent holders to top
     * up a card that only needed another price (OFFLINE-PLAN.md § 10).
     */
    @Test
    fun noRoomForChangeOfflineIsNotATopUp() {
        val (title, detail) = errorCopy(ErrorKind.OFFLINE_NO_CHANGE_ROOM, "needs change", 12_000)
        assertEquals("No room for change offline", title)
        assertEquals("This card can't take change while offline. Try 12 sats", detail)
        assertEquals(
            "This card can't take change while offline. Try another amount",
            errorCopy(ErrorKind.OFFLINE_NO_CHANGE_ROOM, "needs change").second,
        )
        assertEquals("Card needs a top up", errorCopy(ErrorKind.NO_EXACT_PLAN, "").first)
    }

    @Test
    fun theTillsSuggestionReachesTheErrorScreen() {
        val body = """{"ok":false,"error":"offline_no_change_room","detail":"d","suggested_msat":12000,"suggest_below_msat":12000,"suggest_above_msat":null}"""
        val r = parseCharge(409, body) as ChargeResult.Declined
        assertEquals(ChargeErrorCode.OFFLINE_NO_CHANGE_ROOM, r.code)
        assertEquals(12_000L, r.suggestedMsat)
        val none = parseCharge(409, """{"ok":false,"error":"offline_no_change_room","detail":"d","suggested_msat":null}""") as ChargeResult.Declined
        assertEquals(null, none.suggestedMsat)
        assertEquals(ErrorKind.OFFLINE_NO_CHANGE_ROOM, ErrorKind.from(r.code))
    }
}
