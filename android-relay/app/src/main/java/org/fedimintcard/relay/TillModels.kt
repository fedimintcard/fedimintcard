package org.fedimintcard.relay

data class TillStatus(
    val federationId: String,
    val federationName: String?,
    val walletBalanceMsat: Long,
    val cardRelayOk: Boolean,
    val busy: Boolean,
)

data class Note(val slot: Int, val msat: Long)

/** One card slot's contents, from `POST /v1/card/balance`. */
data class CardNote(val slot: Int, val msat: Long, val status: String)

/** What the card holds, read off the card itself rather than the till's wallet. */
data class CardBalance(
    val liveMsat: Long,
    val notes: List<CardNote>,
    val freeSlots: List<Int>,
)

data class ChargeReceipt(
    val saleId: Long,
    val txid: String,
    val priceMsat: Long,
    val receivedMsat: Long,
    val overpaidMsat: Long,
    val feesMsat: Long,
    val inputs: List<Note>,
    val change: List<Note>,
    val collectionPending: Boolean,
    val warnings: List<String>,
    val durationMs: Long,
)

data class Sale(
    val id: Long,
    val tsUnixMs: Long,
    val priceMsat: Long,
    val receivedMsat: Long,
    val overpaidMsat: Long,
    val feesMsat: Long,
    val txid: String,
    val clientRef: String?,
)

data class SalesList(
    val count: Long,
    val totalPriceMsat: Long,
    val totalReceivedMsat: Long,
    val sales: List<Sale>,
)

enum class ChargeErrorCode(val wire: String) {
    BAD_REQUEST("bad_request"),
    BUSY("busy"),
    NO_CARD("no_card"),
    RELAY_UNREACHABLE("relay_unreachable"),
    CARD_ERROR("card_error"),
    INSUFFICIENT("insufficient"),
    NO_EXACT_PLAN("no_exact_plan"),
    DECLINED("declined"),
    UNDECIDED("undecided"),
    RECOVERING("recovering"),
    CONFLICT("conflict"),
    PIN_REQUIRED("pin_required"),
    WRONG_PIN("wrong_pin"),
    PIN_BLOCKED("pin_blocked"),
    CARD_CHANGED("card_changed"),
    UNKNOWN_CARD("unknown_card"),
    OPS_UNREACHABLE("ops_unreachable"),
    OPS_REFUSED("ops_refused"),
    INTERNAL("internal"),
    UNKNOWN("");

    companion object {
        fun fromWire(s: String?): ChargeErrorCode =
            values().firstOrNull { it.wire.isNotEmpty() && it.wire == s } ?: UNKNOWN
    }
}

/** Tries left from a `wrong_pin` detail ("wrong PIN; 2 tries left before the card blocks"), or null. */
fun parseTriesLeft(detail: String): Int? =
    Regex("""(\d+)\s+tr(?:y|ies)\s+left""", RegexOption.IGNORE_CASE).find(detail)?.groupValues?.get(1)?.toIntOrNull()
