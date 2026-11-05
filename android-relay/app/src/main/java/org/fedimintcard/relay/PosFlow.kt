package org.fedimintcard.relay

import java.util.UUID

enum class ErrorKind {
    BAD_REQUEST, BUSY, NO_CARD, RELAY_UNREACHABLE, CARD_ERROR, INSUFFICIENT, NO_EXACT_PLAN,
    DECLINED, UNDECIDED, RECOVERING, CONFLICT, PIN_REQUIRED, WRONG_PIN, PIN_BLOCKED, CARD_CHANGED,
    UNKNOWN_CARD, OPS_UNREACHABLE, OPS_REFUSED, INTERNAL, UNKNOWN, TILL_UNREACHABLE, TIMEOUT_UNKNOWN;

    /** Outcome may already have happened: starts an unresolved sale. */
    val needsSalesCheck: Boolean get() = this == UNDECIDED || this == TIMEOUT_UNKNOWN

    /**
     * Nothing moved: clears an unresolved sale. A PIN refusal comes from inside the spend, after
     * the till has settled any earlier attempt with the same ref, so it proves nothing is pending.
     * `pin_required` and the ops codes are answered before that check, so they prove nothing.
     */
    val definitive: Boolean get() = this in setOf(INSUFFICIENT, NO_EXACT_PLAN, DECLINED, BAD_REQUEST, NO_CARD, CARD_ERROR, WRONG_PIN, PIN_BLOCKED)

    /** Whether trying the same sale again can help. */
    val retryable: Boolean get() = this != PIN_BLOCKED

    companion object {
        fun from(c: ChargeErrorCode): ErrorKind = valueOf(c.name)
    }
}

sealed class PosState {
    object Entering : PosState()
    /**
     * The holder types the PIN for [priceMsat]. The digits are in [PosFlow.pinPad], never here.
     * [wrongPin] after a refused PIN, with the card's [triesLeft] when the till said it.
     * [returnTo] is the unresolved Error that asked for the PIN (cancel goes back to it); null
     * means cancel goes back to the amount. [fresh] is a new sale from the amount screen, so a
     * card left on the phone by the last sale must be lifted before it is charged.
     */
    data class EnteringPin(
        val priceMsat: Long,
        val clientRef: String,
        val wrongPin: Boolean = false,
        val triesLeft: Int? = null,
        val returnTo: Error? = null,
        val fresh: Boolean = false,
    ) : PosState()
    data class WaitingForCard(val priceMsat: Long, val mustRemoveFirst: Boolean) : PosState()
    data class Signing(val priceMsat: Long, val chargeId: Int, val clientRef: String, val unresolvedRef: String? = null) : PosState()
    data class Receipt(val receipt: ChargeReceipt) : PosState()
    data class Error(val priceMsat: Long, val kind: ErrorKind, val detail: String, val clientRef: String, val unresolvedRef: String? = null) : PosState()
    /** [returnTo] is the unresolved Error to go back to, or null to go back to Entering. */
    data class Sales(val returnTo: Error? = null) : PosState()

    /** Check card balance: waiting for the holder to hold a card to the phone. */
    object BalanceWaiting : PosState()
    /** A card is on the phone and the till is reading it; [readId] matches the reply. */
    data class BalanceReading(val readId: Int) : PosState()
    /** What the card holds, shown to the holder until Done or [PosFlow.BALANCE_SHOWN_MS] pass. */
    data class BalanceShown(val readId: Int, val liveMsat: Long) : PosState()
    /** The read failed; [detail] is the till's own words, kept for diagnosis and never shown. */
    data class BalanceFailed(val problem: BalanceProblem, val detail: String = "") : PosState()
}

/** Why a balance read failed, in the terms the holder's screen needs. */
enum class BalanceProblem { CARD_MOVED, NOT_OUR_CARD, TILL_UNREACHABLE, RELAY_UNREACHABLE, BUSY, CARD_SERVICE, TIMEOUT, OTHER }

/** The till's reply to `POST /v1/card/balance`. */
sealed class BalanceResult {
    data class Read(val balance: CardBalance) : BalanceResult()
    data class Refused(val code: ChargeErrorCode, val detail: String) : BalanceResult()
    data class Failed(val error: TillError) : BalanceResult()
}

/** Sorts a failed read into what the holder can do about it. */
fun balanceProblem(r: BalanceResult): BalanceProblem = when (r) {
    is BalanceResult.Read -> BalanceProblem.OTHER
    is BalanceResult.Failed -> when (r.error) {
        TillError.Unreachable -> BalanceProblem.TILL_UNREACHABLE
        TillError.Timeout, TillError.Interrupted -> BalanceProblem.TIMEOUT
        is TillError.BadResponse -> BalanceProblem.OTHER
    }
    is BalanceResult.Refused -> when (r.code) {
        ChargeErrorCode.NO_CARD, ChargeErrorCode.CARD_CHANGED -> BalanceProblem.CARD_MOVED
        ChargeErrorCode.UNKNOWN_CARD -> BalanceProblem.NOT_OUR_CARD
        // A key that does not match, or an applet that is not there, is a card this till does
        // not know; anything else from the card mid-read is the card leaving the field.
        ChargeErrorCode.CARD_ERROR ->
            if (Regex("different card|6a82|6e00|6d00", RegexOption.IGNORE_CASE).containsMatchIn(r.detail)) BalanceProblem.NOT_OUR_CARD
            else BalanceProblem.CARD_MOVED
        ChargeErrorCode.RELAY_UNREACHABLE -> BalanceProblem.RELAY_UNREACHABLE
        ChargeErrorCode.BUSY, ChargeErrorCode.RECOVERING -> BalanceProblem.BUSY
        ChargeErrorCode.OPS_UNREACHABLE, ChargeErrorCode.OPS_REFUSED -> BalanceProblem.CARD_SERVICE
        else -> BalanceProblem.OTHER
    }
}

sealed class PosEvent {
    object ChargePressed : PosEvent()
    /** The holder confirmed the PIN on the pad. */
    object PinConfirmed : PosEvent()
    /** The activity stopped: any PIN typed or held is dropped. */
    object Backgrounded : PosEvent()
    object CancelPressed : PosEvent()
    object CardPresent : PosEvent()
    object CardLost : PosEvent()
    data class ChargeDone(val chargeId: Int, val result: ChargeResult) : PosEvent()
    object RetryPressed : PosEvent()
    object NewSalePressed : PosEvent()
    object BackPressed : PosEvent()
    object AbandonPressed : PosEvent()
    object SalesOpened : PosEvent()
    /** Check card balance, from the amount screen. The amount typed so far is kept. */
    object CardBalanceOpened : PosEvent()
    data class BalanceDone(val readId: Int, val result: BalanceResult) : PosEvent()
    /** The shown balance's time is up; ignored unless that same read is still on screen. */
    data class BalanceExpired(val readId: Int) : PosEvent()
    /** Done on the balance screens: back to the amount. */
    object DonePressed : PosEvent()
    /** The sales poll found a row carrying the unresolved sale's client_ref. */
    data class SaleFound(val sale: Sale) : PosEvent()
}

sealed class PosEffect {
    /** [pin] is handed over to the request, which wipes it once sent. */
    data class StartCharge(val chargeId: Int, val priceMsat: Long, val clientRef: String, val pin: Pin? = null) : PosEffect()
    /** Ask the till to read the card now on the phone. */
    data class StartBalanceRead(val readId: Int) : PosEffect()
}

/** Pure state machine for one till session. Not thread-safe: call from the UI thread. */
class PosFlow(private val newRef: () -> String = { UUID.randomUUID().toString() }) {
    companion object {
        /** How long the holder sees their balance before the amount comes back. */
        const val BALANCE_SHOWN_MS = 15_000L
    }
    val pad = AmountPad()
    val pinPad = PinPad()
    /** The confirmed PIN between the pad and the charge request; handed to [PosEffect.StartCharge]. */
    private var heldPin: Pin? = null
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
            is PosState.EnteringPin -> s.returnTo?.unresolvedRef
            else -> null
        }

    /** True while any PIN digits are typed on the pad or held for the card. */
    val holdsPin: Boolean get() = heldPin != null || pinPad.length > 0

    /** Wipes whatever PIN is typed or held. */
    private fun dropPin() {
        pinPad.clear()
        heldPin?.wipe()
        heldPin = null
    }

    private fun askPin(price: Long, ref: String, wrongPin: Boolean = false, triesLeft: Int? = null, returnTo: PosState.Error? = null, fresh: Boolean = false) {
        dropPin()
        state = PosState.EnteringPin(price, ref, wrongPin, triesLeft, returnTo, fresh)
    }

    /** What the Android back button should do in the current state (null = swallow it). */
    fun systemBack(): PosEvent? = when (val s = state) {
        is PosState.WaitingForCard -> PosEvent.CancelPressed
        is PosState.EnteringPin -> PosEvent.CancelPressed
        is PosState.Receipt -> PosEvent.NewSalePressed
        is PosState.Error -> if (s.unresolvedRef != null) null else PosEvent.BackPressed
        is PosState.Sales -> PosEvent.BackPressed
        PosState.BalanceWaiting, is PosState.BalanceReading -> PosEvent.CancelPressed
        is PosState.BalanceShown, is PosState.BalanceFailed -> PosEvent.DonePressed
        else -> null
    }

    /**
     * The read waits for a card: one already on the phone is read at once (reading is harmless,
     * and a holder who just paid may want the figure); otherwise the screen asks for it.
     */
    private fun balance(): List<PosEffect> {
        if (!cardPresent) { state = PosState.BalanceWaiting; return emptyList() }
        val id = ++nextId
        state = PosState.BalanceReading(id)
        return listOf(PosEffect.StartBalanceRead(id))
    }

    private fun sign(price: Long, ref: String, unresolved: String? = null): List<PosEffect> {
        val id = ++nextId
        state = PosState.Signing(price, id, ref, unresolved)
        val pin = heldPin
        heldPin = null // the request owns it now, and wipes it
        return listOf(PosEffect.StartCharge(id, price, ref, pin))
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
                if (s is PosState.BalanceWaiting) return balance()
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
                askPin(pad.msat, newRef(), fresh = true)
            }
            e is PosEvent.PinConfirmed && s is PosState.EnteringPin -> {
                val pin = pinPad.take() ?: return emptyList()
                heldPin?.wipe()
                heldPin = pin
                // Unresolved: re-POST the same ref at once; the till settles the earlier attempt
                // first and only signs if it is proven to have left nothing behind.
                if (s.returnTo != null) return sign(s.priceMsat, s.clientRef, s.returnTo.unresolvedRef)
                if (!s.fresh) return wait(s.priceMsat, s.clientRef)
                pendingRef = s.clientRef
                return if (cardPresent && !lingering) sign(s.priceMsat, s.clientRef)
                else { state = PosState.WaitingForCard(s.priceMsat, cardPresent && lingering); emptyList() }
            }
            e is PosEvent.CancelPressed && s is PosState.EnteringPin -> { dropPin(); state = s.returnTo ?: PosState.Entering }
            e is PosEvent.CancelPressed && s is PosState.WaitingForCard -> { dropPin(); state = PosState.Entering }
            e is PosEvent.Backgrounded -> {
                dropPin()
                // Without its PIN the sale cannot go on: put the amount back on screen.
                if (s is PosState.EnteringPin) state = s.returnTo ?: PosState.Entering
                if (s is PosState.WaitingForCard) state = PosState.Entering
                if (s.isBalance) state = PosState.Entering
            }
            e is PosEvent.CardBalanceOpened && s is PosState.Entering -> return balance()
            e is PosEvent.CancelPressed && (s is PosState.BalanceWaiting || s is PosState.BalanceReading) -> state = PosState.Entering
            e is PosEvent.BalanceDone && s is PosState.BalanceReading && e.readId == s.readId -> state = when (val r = e.result) {
                is BalanceResult.Read -> PosState.BalanceShown(s.readId, r.balance.liveMsat)
                is BalanceResult.Refused -> PosState.BalanceFailed(balanceProblem(r), r.detail)
                is BalanceResult.Failed -> PosState.BalanceFailed(balanceProblem(r), (r.error as? TillError.BadResponse)?.detail ?: "")
            }
            e is PosEvent.BalanceExpired && s is PosState.BalanceShown && e.readId == s.readId -> state = PosState.Entering
            e is PosEvent.DonePressed && (s is PosState.BalanceShown || s is PosState.BalanceFailed) -> state = PosState.Entering
            e is PosEvent.BackPressed && s is PosState.BalanceFailed -> state = PosState.Entering
            e is PosEvent.RetryPressed && s is PosState.BalanceFailed -> return balance()
            e is PosEvent.ChargeDone && s is PosState.Signing && e.chargeId == s.chargeId -> {
                lingering = true // the card is still on the phone after any outcome
                dropPin()
                when (val r = e.result) {
                    is ChargeResult.Success -> state = PosState.Receipt(r.receipt)
                    is ChargeResult.Declined -> if (r.code == ChargeErrorCode.WRONG_PIN) {
                        // Nothing moved; the same ref is a fresh attempt (TILL-API, wrong_pin).
                        val left = parseTriesLeft(r.detail)
                        if (left == 0) state = PosState.Error(s.priceMsat, ErrorKind.PIN_BLOCKED, "", s.clientRef, null)
                        else askPin(s.priceMsat, s.clientRef, wrongPin = true, triesLeft = left)
                    } else state = error(s, ErrorKind.from(r.code), r.detail)
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
            // Every attempt carries the holder's PIN (a till on ops refuses one without it, before it
            // can replay or settle anything unfinished), and the last one was wiped once sent.
            e is PosEvent.RetryPressed && s is PosState.Error && s.unresolvedRef != null ->
                askPin(s.priceMsat, s.clientRef, returnTo = s)
            e is PosEvent.RetryPressed && s is PosState.Error -> if (s.kind.retryable) askPin(s.priceMsat, s.clientRef)
            e is PosEvent.BackPressed && s is PosState.Error -> if (s.unresolvedRef == null) state = PosState.Entering
            e is PosEvent.AbandonPressed && s is PosState.Error && s.unresolvedRef != null -> { pad.clear(); state = PosState.Entering }
            e is PosEvent.BackPressed && s is PosState.Sales -> state = s.returnTo ?: PosState.Entering
            e is PosEvent.NewSalePressed && s is PosState.Receipt -> { pad.clear(); state = PosState.Entering }
            e is PosEvent.SalesOpened && s is PosState.Entering -> state = PosState.Sales(null)
            e is PosEvent.SalesOpened && s is PosState.Error -> state = PosState.Sales(s)
            e is PosEvent.SaleFound -> {
                val ref = unresolvedRef
                if (ref != null && e.sale.clientRef == ref) {
                    dropPin()
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

private val PosState.isBalance: Boolean
    get() = this is PosState.BalanceWaiting || this is PosState.BalanceReading || this is PosState.BalanceShown || this is PosState.BalanceFailed

/** Generation counter so only the newest sales-poll loop keeps running. */
class PollGeneration {
    private var gen = 0
    fun next(): Int = ++gen
    fun cancel() { gen++ }
    fun isCurrent(g: Int): Boolean = g == gen
}
