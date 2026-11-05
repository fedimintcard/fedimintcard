package org.fedimintcard.relay

/** A top-up the till made (`POST /v1/topup`, docs/TOPUP.md): the invoice is for [amountMsat]. */
data class TopUp(
    val topupId: String,
    val amountMsat: Long,
    val invoice: String,
    val expiresAtMs: Long,
    val state: String,
)

/** `GET /v1/topup/{id}` and the reply to `POST /v1/topup/{id}/load`. */
data class TopUpStatus(
    val topupId: String,
    val amountMsat: Long,
    val state: String,
    val loadTxid: String?,
    val detail: String?,
)

/** A till reply to a top-up route. */
sealed class TopUpReply<out T> {
    data class Ok<T>(val value: T) : TopUpReply<T>()
    /** `{"ok":false,"error":…}`: [error] is the wire code (`card_full`, `expired`, …). */
    data class Refused(val error: String, val detail: String, val httpStatus: Int) : TopUpReply<Nothing>()
    data class Failed(val error: TillError) : TopUpReply<Nothing>()
}

/** Why a top-up step failed, in the terms the screen needs. */
enum class TopUpProblem {
    /** Unpaid at expiry: nothing moved. */
    EXPIRED,
    /** The card has no room for the notes; the top-up stays paid. */
    CARD_FULL,
    /** The card has no free slot, found before any invoice was made: nothing was paid. */
    NO_ROOM,
    CARD_MOVED, NOT_OUR_CARD, TILL_UNREACHABLE, TIMEOUT, CARD_SERVICE, BAD_AMOUNT, BUSY,
    /** Paid, and the card step was cancelled: the top-up waits in the till until a tap loads it. */
    NOT_LOADED,
    /** The till gave up on this top-up (`state: failed`); any eCash received stays in the till. */
    FAILED,
    OTHER,
}

/** Sorts a refused or failed top-up reply. */
fun topUpProblem(r: TopUpReply<*>): TopUpProblem = when (r) {
    is TopUpReply.Ok -> TopUpProblem.OTHER
    is TopUpReply.Failed -> when (r.error) {
        TillError.Unreachable -> TopUpProblem.TILL_UNREACHABLE
        TillError.Timeout, TillError.Interrupted -> TopUpProblem.TIMEOUT
        is TillError.BadResponse -> TopUpProblem.OTHER
    }
    is TopUpReply.Refused -> when (r.error) {
        "expired" -> TopUpProblem.EXPIRED
        "card_full" -> TopUpProblem.CARD_FULL
        "no_card", "card_link_gone", "card_changed", "card_lost" -> TopUpProblem.CARD_MOVED
        "card_error" ->
            if (Regex("different card|6a82|6e00|6d00", RegexOption.IGNORE_CASE).containsMatchIn(r.detail)) TopUpProblem.NOT_OUR_CARD
            else TopUpProblem.CARD_MOVED
        "unknown_card" -> TopUpProblem.NOT_OUR_CARD
        "ops_not_connected", "ops_unreachable", "ops_refused" -> TopUpProblem.CARD_SERVICE
        "bad_amount" -> TopUpProblem.BAD_AMOUNT
        "busy", "recovering" -> TopUpProblem.BUSY
        "relay_unreachable" -> TopUpProblem.TILL_UNREACHABLE
        else -> TopUpProblem.OTHER
    }
}

/** Title and detail for a failed top-up step. [paid]: the holder has paid and the top-up waits for the card. */
fun topUpErrorCopy(p: TopUpProblem, paid: Boolean, detail: String): Pair<String, String> {
    val waiting = if (paid) " The top-up is paid and waiting." else ""
    return when (p) {
        TopUpProblem.EXPIRED -> "The invoice expired" to "Nothing was paid. Make a new invoice to try again."
        TopUpProblem.NOT_LOADED -> "Not loaded yet" to "The top-up is paid and waiting. Tap the card to load it; Done leaves the payment with the till."
        TopUpProblem.CARD_FULL -> "This card is full" to "Spend from it, then tap again.$waiting"
        TopUpProblem.NO_ROOM -> "This card is full" to "Spend from it first, then top up."
        TopUpProblem.CARD_MOVED -> "The card moved away" to "Hold the card still until it's loaded.$waiting"
        TopUpProblem.NOT_OUR_CARD -> "Not a card for this till" to "Try the holder's card.$waiting"
        TopUpProblem.TILL_UNREACHABLE -> "Can't reach the till" to "Check that the till is running, then try again.$waiting"
        TopUpProblem.TIMEOUT -> "The till took too long" to "Try again: the same top-up is never loaded twice.$waiting"
        TopUpProblem.CARD_SERVICE -> "Card service unreachable" to "Check the network, then try again.$waiting"
        TopUpProblem.BAD_AMOUNT -> "That amount can't go on a card" to detail.ifEmpty { "Try a different amount." }
        TopUpProblem.BUSY -> "The till is busy" to "Try again in a moment.$waiting"
        TopUpProblem.FAILED -> "The top-up failed" to (if (detail.isEmpty()) "" else "$detail ") + "Any payment received stays with the till."
        TopUpProblem.OTHER -> "Something went wrong" to (detail + waiting).trim()
    }
}

/** `m:ss` left until [expiresAtMs]; 0:00 once past. */
fun timeLeft(expiresAtMs: Long, nowMs: Long): String {
    val s = ((expiresAtMs - nowMs).coerceAtLeast(0) + 999) / 1000
    return "${s / 60}:${(s % 60).toString().padStart(2, '0')}"
}

/** What a wallet scans: `LIGHTNING:` and the invoice, upper case (the QR's alphanumeric mode). */
fun invoiceQrText(invoice: String): String = "LIGHTNING:" + invoice.uppercase()
