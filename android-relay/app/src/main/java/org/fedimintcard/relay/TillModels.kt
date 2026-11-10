package org.fedimintcard.relay

data class TillStatus(
    val federationId: String,
    val federationName: String?,
    val walletBalanceMsat: Long,
    val cardRelayOk: Boolean,
    val busy: Boolean,
    /** Offline charges queued on the till and not yet sent (`offline.queued`); 0 on a till without the field. */
    val offlineQueued: Long = 0,
    val offlineQueuedMsat: Long = 0,
    /** Offline charges the registry refused as double spends (`offline.double_spent`). */
    val offlineDoubleSpent: Long = 0,
)

data class Note(val slot: Int, val msat: Long)

/** One card slot's contents, from `POST /v1/card/balance`. */
data class CardNote(val slot: Int, val msat: Long, val status: String)

/** What the card holds, read off the card itself rather than the till's wallet. */
data class CardBalance(
    val liveMsat: Long,
    val notes: List<CardNote>,
    val freeSlots: List<Int>,
    /** The card's id on the registry (`short_id`), when the till is on ops: a top-up started from
     *  the balance screen sends it so the till fits the amount to this card's free slots. */
    val shortId: String? = null,
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
    /** The till took this charge with no registry or federation reachable (`"offline": true`). */
    val offline: Boolean = false,
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
    /** Offline, the price needs change and the card has no slot for it; `suggested_msat` may say what it can pay. */
    OFFLINE_NO_CHANGE_ROOM("offline_no_change_room"),
    // The phone's own offline charge answers these (docs/POS-OFFLINE.md § Refusals). Some come from
    // the till's own offline path too: from the till, the first six read as they did before (PosFlow).
    /** No offline kit for this pairing yet (or no core): nothing was asked of the card. */
    OFFLINE_NOT_READY("offline_not_ready"),
    /** The kit is past its expiry, by the phone's clock or the card's. */
    OFFLINE_KIT_EXPIRED("offline_kit_expired"),
    /** What waits to sync has reached the kit's caps. */
    OFFLINE_CAP_REACHED("offline_cap_reached"),
    /** Notes that do not check against the kit's keys, and the rest do not pay. */
    OFFLINE_NOTE_INVALID("offline_note_invalid"),
    /** The kit has run out of a till note the price needs; `suggested_msat` is a price it still pays. */
    OFFLINE_POOL_EXHAUSTED("offline_pool_exhausted"),
    /** An applet 2.x card, which is taken online only. */
    OFFLINE_UNSUPPORTED_CARD("offline_unsupported_card"),
    /** The card's certificate or attestation does not check (a clone). */
    CARD_NOT_GENUINE("card_not_genuine"),
    /** The card's own daily limit. */
    CARD_LIMIT("card_limit"),
    INTERNAL("internal"),
    UNKNOWN("");

    companion object {
        /** `card_link_gone` (the link closed before the job ran; nothing moved) reads as `no_card`. */
        fun fromWire(s: String?): ChargeErrorCode =
            if (s == "card_link_gone") NO_CARD else values().firstOrNull { it.wire.isNotEmpty() && it.wire == s } ?: UNKNOWN
    }
}

/** Tries left from a `wrong_pin` detail ("wrong PIN; 2 tries left before the card blocks"), or null. */
fun parseTriesLeft(detail: String): Int? =
    Regex("""(\d+)\s+tr(?:y|ies)\s+left""", RegexOption.IGNORE_CASE).find(detail)?.groupValues?.get(1)?.toIntOrNull()
