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
}
