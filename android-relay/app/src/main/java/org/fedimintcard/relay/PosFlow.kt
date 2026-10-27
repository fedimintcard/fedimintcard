package org.fedimintcard.relay

import java.util.UUID

enum class ErrorKind {
    BAD_REQUEST, BUSY, NO_CARD, RELAY_UNREACHABLE, CARD_ERROR, INSUFFICIENT, NO_EXACT_PLAN,
    DECLINED, UNDECIDED, RECOVERING, INTERNAL, UNKNOWN, TILL_UNREACHABLE, TIMEOUT_UNKNOWN;

    /** Outcome may already have happened: do not offer a blind Retry. */
    val needsSalesCheck: Boolean get() = this == UNDECIDED || this == TIMEOUT_UNKNOWN

    companion object {
        fun from(c: ChargeErrorCode): ErrorKind = valueOf(c.name)
    }
}

sealed class PosState {
    object Entering : PosState()
    data class WaitingForCard(val priceMsat: Long, val mustRemoveFirst: Boolean) : PosState()
    data class Signing(val priceMsat: Long, val chargeId: Int, val clientRef: String) : PosState()
    data class Receipt(val receipt: ChargeReceipt) : PosState()
    data class Error(val priceMsat: Long, val kind: ErrorKind, val detail: String, val clientRef: String) : PosState()
    object Sales : PosState()
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
    object SalesOpened : PosEvent()
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

    private fun sign(price: Long, ref: String): List<PosEffect> {
        val id = ++nextId
        state = PosState.Signing(price, id, ref)
        return listOf(PosEffect.StartCharge(id, price, ref))
    }

    private fun wait(price: Long, ref: String): List<PosEffect> {
        // The ref travels in the Waiting state via pendingRef so Retry keeps it.
        pendingRef = ref
        return if (cardPresent) sign(price, ref) else { state = PosState.WaitingForCard(price, false); emptyList() }
    }

    private var pendingRef = ""

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
                when (val r = e.result) {
                    is ChargeResult.Success -> { lingering = true; state = PosState.Receipt(r.receipt) }
                    is ChargeResult.Declined ->
                        state = PosState.Error(s.priceMsat, ErrorKind.from(r.code), r.detail, s.clientRef)
                    is ChargeResult.Failed -> state = PosState.Error(
                        s.priceMsat,
                        when (r.error) {
                            TillError.Unreachable -> ErrorKind.TILL_UNREACHABLE
                            TillError.Timeout -> ErrorKind.TIMEOUT_UNKNOWN
                            is TillError.BadResponse -> ErrorKind.INTERNAL
                        },
                        (r.error as? TillError.BadResponse)?.detail ?: "",
                        s.clientRef,
                    )
                }
            }
            e is PosEvent.RetryPressed && s is PosState.Error -> return wait(s.priceMsat, s.clientRef)
            e is PosEvent.BackPressed && (s is PosState.Error || s is PosState.Sales) -> state = PosState.Entering
            e is PosEvent.NewSalePressed && s is PosState.Receipt -> { pad.clear(); state = PosState.Entering }
            e is PosEvent.SalesOpened && (s is PosState.Entering || s is PosState.Error) -> state = PosState.Sales
        }
        return emptyList()
    }
}
