package org.fedimintcard.relay

import java.util.UUID

enum class ErrorKind {
    BAD_REQUEST, BUSY, NO_CARD, RELAY_UNREACHABLE, CARD_ERROR, INSUFFICIENT, NO_EXACT_PLAN,
    DECLINED, UNDECIDED, RECOVERING, INTERNAL, UNKNOWN, TILL_UNREACHABLE, TIMEOUT_UNKNOWN;

    /** Outcome may already have happened: starts an unresolved sale. */
    val needsSalesCheck: Boolean get() = this == UNDECIDED || this == TIMEOUT_UNKNOWN

    /** Nothing moved: clears an unresolved sale. */
    val definitive: Boolean get() = this in setOf(INSUFFICIENT, NO_EXACT_PLAN, DECLINED, BAD_REQUEST, NO_CARD, CARD_ERROR)

    companion object {
        fun from(c: ChargeErrorCode): ErrorKind = valueOf(c.name)
    }
}

sealed class PosState {
    object Entering : PosState()
    data class WaitingForCard(val priceMsat: Long, val mustRemoveFirst: Boolean) : PosState()
    data class Signing(val priceMsat: Long, val chargeId: Int, val clientRef: String, val unresolvedRef: String? = null) : PosState()
    data class Receipt(val receipt: ChargeReceipt) : PosState()
    data class Error(val priceMsat: Long, val kind: ErrorKind, val detail: String, val clientRef: String, val unresolvedRef: String? = null) : PosState()
    /** [returnTo] is the unresolved Error to go back to, or null to go back to Entering. */
    data class Sales(val returnTo: Error? = null) : PosState()
}

sealed class PosEvent {
    object ChargePressed : PosEvent()
    object CancelPressed : PosEvent()
    object CardPresent : PosEvent()
    object CardLost : PosEvent()
    data class ChargeDone(val chargeId: Int, val result: ChargeResult) : PosEvent()
    object RetryPressed : PosEvent()
    object NewSalePressed : PosEvent()
    object BackPressed : PosEvent()
    object AbandonPressed : PosEvent()
    object SalesOpened : PosEvent()
    /** The sales poll found a row carrying the unresolved sale's client_ref. */
    data class SaleFound(val sale: Sale) : PosEvent()
}

sealed class PosEffect {
    data class StartCharge(val chargeId: Int, val priceMsat: Long, val clientRef: String) : PosEffect()
}

/** Pure state machine for one till session. Not thread-safe: call from the UI thread. */
class PosFlow(private val newRef: () -> String = { UUID.randomUUID().toString() }) {
    val pad = AmountPad()
    var state: PosState = PosState.Entering
        private set
    private var cardPresent = false
    private var lingering = false
    private var nextId = 0
    private var pendingRef = ""

    /** The client_ref of a sale whose outcome is unknown, while we are waiting to learn it. */
    val unresolvedRef: String?
        get() = when (val s = state) {
            is PosState.Error -> s.unresolvedRef
            is PosState.Signing -> s.unresolvedRef
            is PosState.Sales -> s.returnTo?.unresolvedRef
            else -> null
        }

    /** What the Android back button should do in the current state (null = swallow it). */
    fun systemBack(): PosEvent? = when (val s = state) {
        is PosState.WaitingForCard -> PosEvent.CancelPressed
        is PosState.Receipt -> PosEvent.NewSalePressed
        is PosState.Error -> if (s.unresolvedRef != null) null else PosEvent.BackPressed
        is PosState.Sales -> PosEvent.BackPressed
        else -> null
    }

    private fun sign(price: Long, ref: String, unresolved: String? = null): List<PosEffect> {
        val id = ++nextId
        state = PosState.Signing(price, id, ref, unresolved)
        return listOf(PosEffect.StartCharge(id, price, ref))
    }

    private fun wait(price: Long, ref: String): List<PosEffect> {
        pendingRef = ref
        return if (cardPresent) sign(price, ref) else { state = PosState.WaitingForCard(price, false); emptyList() }
    }

    /** Unresolved is sticky: only success, a found sale or a nothing-moved code clears it. */
    private fun error(s: PosState.Signing, kind: ErrorKind, detail: String): PosState.Error {
        val unresolved = when {
            kind.needsSalesCheck -> s.clientRef
            kind.definitive -> null
            else -> s.unresolvedRef
        }
        return PosState.Error(s.priceMsat, kind, detail, s.clientRef, unresolved)
    }

    fun dispatch(e: PosEvent): List<PosEffect> {
        val s = state
        when (e) {
            PosEvent.CardPresent -> {
                cardPresent = true
                if (s is PosState.WaitingForCard && !s.mustRemoveFirst) return sign(s.priceMsat, pendingRef)
                return emptyList()
            }
            PosEvent.CardLost -> {
                cardPresent = false
                lingering = false
                if (s is PosState.WaitingForCard && s.mustRemoveFirst) state = s.copy(mustRemoveFirst = false)
                return emptyList()
            }
            else -> {}
        }
        when {
            e is PosEvent.ChargePressed && s is PosState.Entering -> {
                if (!pad.isChargeable) return emptyList()
                val ref = newRef()
                pendingRef = ref
                return if (cardPresent && !lingering) sign(pad.msat, ref)
                else { state = PosState.WaitingForCard(pad.msat, cardPresent && lingering); emptyList() }
            }
            e is PosEvent.CancelPressed && s is PosState.WaitingForCard -> state = PosState.Entering
            e is PosEvent.ChargeDone && s is PosState.Signing && e.chargeId == s.chargeId -> {
                lingering = true // the card is still on the phone after any outcome
                when (val r = e.result) {
                    is ChargeResult.Success -> state = PosState.Receipt(r.receipt)
                    is ChargeResult.Declined -> state = error(s, ErrorKind.from(r.code), r.detail)
                    is ChargeResult.Failed -> state = error(
                        s,
                        when (r.error) {
                            TillError.Unreachable -> ErrorKind.TILL_UNREACHABLE
                            TillError.Timeout, TillError.Interrupted -> ErrorKind.TIMEOUT_UNKNOWN
                            is TillError.BadResponse -> ErrorKind.INTERNAL
                        },
                        (r.error as? TillError.BadResponse)?.detail ?: "",
                    )
                }
            }
            // Unresolved: re-POST the same ref at once, the till replays without touching the card.
            e is PosEvent.RetryPressed && s is PosState.Error && s.unresolvedRef != null ->
                return sign(s.priceMsat, s.clientRef, s.unresolvedRef)
            e is PosEvent.RetryPressed && s is PosState.Error -> return wait(s.priceMsat, s.clientRef)
            e is PosEvent.BackPressed && s is PosState.Error -> if (s.unresolvedRef == null) state = PosState.Entering
            e is PosEvent.AbandonPressed && s is PosState.Error && s.unresolvedRef != null -> { pad.clear(); state = PosState.Entering }
            e is PosEvent.BackPressed && s is PosState.Sales -> state = s.returnTo ?: PosState.Entering
            e is PosEvent.NewSalePressed && s is PosState.Receipt -> { pad.clear(); state = PosState.Entering }
            e is PosEvent.SalesOpened && s is PosState.Entering -> state = PosState.Sales(null)
            e is PosEvent.SalesOpened && s is PosState.Error -> state = PosState.Sales(s)
            e is PosEvent.SaleFound -> {
                val ref = unresolvedRef
                if (ref != null && e.sale.clientRef == ref) {
                    val x = e.sale
                    state = PosState.Receipt(
                        ChargeReceipt(x.id, x.txid, x.priceMsat, x.receivedMsat, x.overpaidMsat, x.feesMsat,
                            emptyList(), emptyList(), false, emptyList(), 0)
                    )
                }
            }
        }
        return emptyList()
    }
}

/** Generation counter so only the newest sales-poll loop keeps running. */
class PollGeneration {
    private var gen = 0
    fun next(): Int = ++gen
    fun cancel() { gen++ }
    fun isCurrent(g: Int): Boolean = g == gen
}
