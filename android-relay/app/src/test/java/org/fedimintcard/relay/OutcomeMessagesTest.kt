package org.fedimintcard.relay

import org.junit.Assert.*
import org.junit.Test

/** What the screen says for each outcome the card or the till can report (DESIGN 4.0 sections 3, 5, 7). */
class OutcomeMessagesTest {
    private fun copy(kind: ErrorKind, waitS: Int? = null, wrongTries: Int? = null) = errorCopy(kind, "d", waitS = waitS, wrongTries = wrongTries)

    @Test fun aPinWaitSaysHowLongToHoldAndTapAgainIfLifted() {
        assertEquals("Hold the card, about 12 s" to "Tap again to finish the wait", copy(ErrorKind.PIN_WAIT, waitS = 12))
        assertEquals("Hold the card" to "Tap again to finish the wait", copy(ErrorKind.PIN_WAIT, waitS = 0))
        assertEquals("Hold the card" to "Tap again to finish the wait", copy(ErrorKind.PIN_WAIT))
    }
    @Test fun theSecondsWhileTheCardServesTheWait() {
        assertEquals("Hold the card, about 5 s", holdingCopy(5))
        assertEquals("Hold the card, about 1 s", holdingCopy(1))
        assertEquals("Paying… keep the card on the phone", holdingCopy(0))
    }
    @Test fun aWrongPinSaysTheWrongTriesInARow() {
        assertEquals("Wrong PIN. 1 wrong try in a row", copy(ErrorKind.WRONG_PIN, wrongTries = 1).first)
        assertEquals("Wrong PIN. 15 wrong tries in a row", copy(ErrorKind.WRONG_PIN, wrongTries = 15).first)
        assertEquals("Wrong PIN", copy(ErrorKind.WRONG_PIN).first)
        assertEquals("Wrong PIN", copy(ErrorKind.WRONG_PIN, wrongTries = 0).first)
    }
    @Test fun aRefusedSignatureSaysNothingWasSpentAndTapAgain() {
        assertEquals("Nothing was spent" to "The card did not sign. Tap again.", copy(ErrorKind.SIGN_CHECK_FAILED))
    }
    @Test fun anOldCardNeedsAResetAndANewSetUp() {
        assertEquals("Old card" to "This card needs a reset and a new set-up.", copy(ErrorKind.OLD_CARD))
    }
    @Test fun eachOutcomeIsRetryableOrNot() {
        assertTrue(ErrorKind.PIN_WAIT.retryable); assertTrue(ErrorKind.SIGN_CHECK_FAILED.retryable)
        assertFalse(ErrorKind.OLD_CARD.retryable)
        for (k in listOf(ErrorKind.PIN_WAIT, ErrorKind.SIGN_CHECK_FAILED, ErrorKind.WRONG_PIN)) assertTrue(k.name, k.definitive)
        // Answered before the till settles an earlier attempt: it proves nothing about one.
        assertFalse(ErrorKind.OLD_CARD.definitive)
    }
    @Test fun noScreenSaysTriesLeftOrBlocked() {
        for (k in ErrorKind.values()) {
            val (t, d) = errorCopy(k, "d", waitS = 3, wrongTries = 3)
            for (text in listOf(t, d)) assertFalse("$k: $text", Regex("tries left|try left|blocked", RegexOption.IGNORE_CASE).containsMatchIn(text))
        }
    }
}
