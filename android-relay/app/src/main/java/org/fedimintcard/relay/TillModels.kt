package org.fedimintcard.relay

data class TillStatus(
    val federationId: String,
    val federationName: String?,
    val walletBalanceMsat: Long,
    val cardRelayOk: Boolean,
    val busy: Boolean,
)

data class Note(val slot: Int, val msat: Long)

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
    INTERNAL("internal"),
    UNKNOWN("");

    companion object {
        fun fromWire(s: String?): ChargeErrorCode =
            values().firstOrNull { it.wire.isNotEmpty() && it.wire == s } ?: UNKNOWN
    }
}
