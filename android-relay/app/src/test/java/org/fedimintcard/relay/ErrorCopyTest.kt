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

    // ---- the phone's own charge: one copy set with iOS (docs/POS-OFFLINE.md § Android) ----

    /** The unreachable screen stays only for the cases the phone cannot take, and says which. */
    @Test
    fun theUnreachableScreenSaysWhichCase() {
        val cases = mapOf(
            ErrorKind.OFFLINE_NOT_READY to "No offline kit yet. Reach the till once while online.",
            ErrorKind.OFFLINE_KIT_EXPIRED to "The offline kit has expired. Reach the till to renew it.",
            ErrorKind.OFFLINE_CAP_REACHED to "Offline limit reached. Reach the till so waiting payments can sync.",
            ErrorKind.OFFLINE_NOTE_INVALID to "This card can't be checked without the till. It pays online only.",
        )
        for ((k, detail) in cases) assertEquals("Till service unreachable" to detail, errorCopy(k, "core detail"))
        assertEquals(
            "Till service unreachable" to "The last reply was lost, so this sale can't be taken on the phone. Try again once the till answers.",
            errorCopy(ErrorKind.TILL_UNREACHABLE, "", unresolved = true),
        )
        // A sale nothing was sent for: the USB setup's hint, as before.
        assertEquals("Till service unreachable" to "Check adb reverse tcp:9877 tcp:9877", errorCopy(ErrorKind.TILL_UNREACHABLE, ""))
    }

    @Test
    fun anAmountTheKitCannotPaySaysWhatItCan() {
        assertEquals("Amount not available offline" to "Try 49 sats, or reach the till for a new kit.", errorCopy(ErrorKind.OFFLINE_POOL_EXHAUSTED, "d", 49_000))
        assertEquals("Try another amount, or reach the till for a new kit.", errorCopy(ErrorKind.OFFLINE_POOL_EXHAUSTED, "d").second)
    }

    @Test
    fun aHeldSaleAndTheCardsOwnRefusals() {
        assertEquals(
            "Payment not finished" to "The card was taken away while it signed. Hold the same card to the phone and press Try again. It won't be charged twice.",
            errorCopy(ErrorKind.HELD_ON_PHONE, "undecided", unresolved = true),
        )
        assertEquals("This card pays online only" to "It can't be taken without the till.", errorCopy(ErrorKind.OFFLINE_UNSUPPORTED_CARD, "applet 2.1"))
        assertEquals("Card declined" to "This isn't a genuine Fedimint Card.", errorCopy(ErrorKind.CARD_NOT_GENUINE, "attestation"))
        assertEquals("Card limit reached" to "This card has reached its daily limit.", errorCopy(ErrorKind.CARD_LIMIT, "6985"))
    }

    @Test
    fun abandoningAHeldSaleSaysItsCardStillFinishesIt() {
        assertEquals("Abandon sale?" to "The card already signed part of this payment. Its next tap on this phone still finishes it.", abandonCopy(held = true))
        assertEquals(
            null to "Abandon sale? If the payment did go through, the customer will have paid without a receipt. Check Sales first.",
            abandonCopy(held = false),
        )
    }

    @Test
    fun theHomeLineSettingsRowsAndConfirms() {
        assertEquals("1 payment is waiting for its card: tap that card to finish it.", heldLine(1))
        assertEquals("2 payments are waiting for their cards: tap those cards to finish them.", heldLine(2))
        assertEquals("1 payment is waiting to sync with this till. It stays on this phone and can't be synced to another till.", pendingCopy(1))
        assertEquals("3 payments are waiting to sync with this till. They stay on this phone and can't be synced to another till.", pendingCopy(3))
        assertEquals("Unpair anyway?", UNPAIR_TITLE)
        assertEquals("21 sats · pool_entry_reused: the till refused it after the card paid.", refusedRowCopy(RefusedEntry("t", "pool_entry_reused", 21_000, 1)))
        assertEquals("Delete this record?" to "The card was charged; this deletes the only record of it.", DISMISS_TITLE to DISMISS_COPY)
    }

    @Test
    fun settingsSaysWhereOfflinePaymentsStand() {
        val day = 86_400_000L
        val now = 100 * day
        val kit = KitStore.Kit("{}", now - 3 * 60_000, "k1", now - 3 * 60_000, now + 6 * day + 1, 100_000_000, 200)
        fun view(off: String? = null, starting: Boolean = false, paired: Boolean = true, k: KitStore.Kit? = kit, stats: CoreStats? = CoreStats(2, 0, 1, 0, 5), kitError: String? = null, syncError: String? = null) =
            OfflineView(off, starting, paired, k, stats, emptyList(), kitError, syncError)
        assertEquals(
            listOf(
                "Offline kit: fetched 3 min ago, good for 6 days. What waits to sync is capped at 100,000 sats and 200 payments.",
                "On this phone: 2 waiting to sync, 1 waiting for their cards, 5 synced.",
            ),
            offlineLines(view(), now),
        )
        assertEquals("Offline kit: none yet. Reach the till once while online.", offlineLines(view(k = null), now)[0])
        assertEquals(
            "Offline kit: fetched 8 days ago, expired 1 day ago. Reach the till to renew it.",
            offlineLines(view(k = kit.copy(fetchedAtMs = now - 8 * day, expiresAtMs = now - day)), now)[0],
        )
        assertEquals(listOf("Offline payments are off: UnsatisfiedLinkError: x"), offlineLines(view(off = "UnsatisfiedLinkError: x", stats = null), now))
        assertEquals("Offline payments: starting…", offlineLines(view(starting = true, stats = null), now)[0])
        assertEquals("Offline payments: pair with a till to take them.", offlineLines(view(paired = false, k = null), now)[0])
        val failed = offlineLines(view(kitError = "HTTP 503 offline_not_ready", syncError = "the till did not answer"), now)
        assertEquals(listOf("Last kit fetch failed: HTTP 503 offline_not_ready", "Last sync stopped: the till did not answer"), failed.takeLast(2))
        assertEquals(listOf("0 s", "59 s", "1 min", "59 min", "1 h", "23 h", "1 day", "2 days"),
            listOf(0L, 59_000, 60_000, 3_599_000, 3_600_000, 86_399_000, day, 2 * day).map(::span))
    }
}
