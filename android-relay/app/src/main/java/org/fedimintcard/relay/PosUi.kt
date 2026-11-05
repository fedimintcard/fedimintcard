package org.fedimintcard.relay

import android.app.Activity
import android.app.AlertDialog
import android.graphics.drawable.GradientDrawable
import android.text.InputType
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.view.animation.OvershootInterpolator
import android.view.animation.PathInterpolator
import android.widget.BaseAdapter
import android.widget.Button
import android.widget.EditText
import android.widget.ListView
import android.widget.TextView
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/** Binds views, switches screens, renders [PosState]. All calls on the UI thread. */
class PosUi(
    private val act: Activity,
    private val onEvent: (PosEvent) -> Unit,
    private val onKey: (Int?) -> Unit,
    private val onPinKey: (Int?) -> Unit,
) {
    private fun <T : View> v(id: Int): T = act.findViewById(id)

    private val screens = listOf(R.id.screen_amount, R.id.screen_pin, R.id.screen_tap, R.id.screen_receipt, R.id.screen_error, R.id.screen_sales, R.id.screen_card_balance, R.id.screen_relay, R.id.screen_invoice)
    private val statusAlert: TextView = v(R.id.status_alert)
    private val fedName: TextView = v(R.id.federation_name)
    private val balance: TextView = v(R.id.balance_line)
    private val cardBalanceAmount: TextView = v(R.id.card_balance_amount)
    private val loadWaiting: Button = v(R.id.btn_load_waiting)
    private val cardBalancePanel: View = v(R.id.card_balance_panel)
    private val cardBalanceCountdown: TextView = v(R.id.card_balance_countdown)
    private val amountText: TextView = v(R.id.amount_text)
    private val charge: Button = v(R.id.btn_charge)
    private val checkBalance: Button = v(R.id.btn_check_balance)
    private val topUpTitle: View = v(R.id.topup_title)
    private val invoiceAmount: TextView = v(R.id.invoice_amount)
    private val invoiceQr: android.widget.ImageView = v(R.id.invoice_qr)
    private val invoiceTimeLeft: TextView = v(R.id.invoice_time_left)
    private var qrFor: String? = null
    private val pinAmount: TextView = v(R.id.pin_amount)
    private val pinNotice: TextView = v(R.id.pin_notice)
    private val pinDots: TextView = v(R.id.pin_dots)
    private val pinConfirm: Button = v(R.id.btn_pin_confirm)
    private val tapAmount: TextView = v(R.id.tap_amount)
    private val tapStatus: TextView = v(R.id.tap_status)
    private val cancel: Button = v(R.id.btn_cancel)
    private val motion: TapMotionView = v(R.id.tap_motion)
    private val tapCard: View = v(R.id.tap_card)
    private val receiptCheck: View = v(R.id.receipt_check)
    private val receiptAmount: View = v(R.id.receipt_amount)
    private val errTitle: TextView = v(R.id.error_title)
    private val errDetail: TextView = v(R.id.error_detail)
    private val retry: Button = v(R.id.btn_retry)
    private val checkSales: Button = v(R.id.btn_check_sales)
    private val errBack: View = v(R.id.btn_error_back)
    private val abandon: View = v(R.id.btn_abandon)
    private val salesHeader: TextView = v(R.id.sales_header)
    private val salesList: ListView = v(R.id.sales_list)
    private val salesAdapter = SalesAdapter()

    val relayStatus: TextView = v(R.id.relay_status)
    val relayLast: TextView = v(R.id.relay_last)
    val relayInfo: TextView = v(R.id.relay_info)
    private val relayTill: TextView = v(R.id.relay_till)

    private var relayOpen = false
    private var last: PosState = PosState.Entering
    private var lastPad = ""
    private var elapsedS = 0
    private var shownTarget = 0
    private val decelerate = PathInterpolator(0.2f, 0f, 0f, 1f)

    init {
        salesList.adapter = salesAdapter
        val digits = listOf(R.id.key_0, R.id.key_1, R.id.key_2, R.id.key_3, R.id.key_4, R.id.key_5, R.id.key_6, R.id.key_7, R.id.key_8, R.id.key_9)
        digits.forEachIndexed { d, id -> v<View>(id).setOnClickListener { onKey(d) } }
        v<View>(R.id.key_c).setOnClickListener { onKey(-1) }
        v<View>(R.id.key_back).setOnClickListener { onKey(null) }
        val pinDigits = listOf(R.id.pkey_0, R.id.pkey_1, R.id.pkey_2, R.id.pkey_3, R.id.pkey_4, R.id.pkey_5, R.id.pkey_6, R.id.pkey_7, R.id.pkey_8, R.id.pkey_9)
        pinDigits.forEachIndexed { d, id -> v<View>(id).setOnClickListener { onPinKey(d) } }
        v<View>(R.id.pkey_c).setOnClickListener { onPinKey(-1) }
        v<View>(R.id.pkey_back).setOnClickListener { onPinKey(null) }
        pinConfirm.setOnClickListener { onEvent(PosEvent.PinConfirmed) }
        v<View>(R.id.btn_pin_cancel).setOnClickListener { onEvent(PosEvent.CancelPressed) }
        charge.setOnClickListener { onEvent(if (topUpMode) PosEvent.CreateInvoicePressed else PosEvent.ChargePressed) }
        cancel.setOnClickListener { onEvent(PosEvent.CancelPressed) }
        retry.setOnClickListener { onEvent(PosEvent.RetryPressed) }
        v<View>(R.id.btn_error_back).setOnClickListener { onEvent(PosEvent.BackPressed) }
        abandon.setOnClickListener {
            AlertDialog.Builder(act)
                .setMessage("Abandon sale? If the payment did go through, the customer will have paid without a receipt. Check Sales first.")
                .setPositiveButton("Abandon") { _, _ -> onEvent(PosEvent.AbandonPressed) }
                .setNegativeButton("Keep", null)
                .show()
        }
        checkSales.setOnClickListener { onEvent(PosEvent.SalesOpened) }
        v<View>(R.id.btn_sales).setOnClickListener { onEvent(PosEvent.SalesOpened) }
        v<View>(R.id.btn_sales_back).setOnClickListener { onEvent(PosEvent.BackPressed) }
        v<View>(R.id.btn_new_sale).setOnClickListener { onEvent(PosEvent.NewSalePressed) }
        v<View>(R.id.btn_relay_back).setOnClickListener { relayOpen = false; show() }
        checkBalance.setOnClickListener { onEvent(if (topUpMode) PosEvent.CancelPressed else PosEvent.CardBalanceOpened) }
        v<View>(R.id.btn_charge_card).setOnClickListener { onEvent(PosEvent.ChargeCardPressed) }
        v<View>(R.id.btn_top_up_card).setOnClickListener { onEvent(PosEvent.TopUpPressed) }
        loadWaiting.setOnClickListener { onEvent(PosEvent.LoadWaitingPressed) }
        v<View>(R.id.btn_invoice_cancel).setOnClickListener { onEvent(PosEvent.CancelPressed) }
        v<View>(R.id.btn_card_balance_done).setOnClickListener { onEvent(PosEvent.DonePressed) }
        statusAlert.setOnClickListener { showRelay() }
    }

    fun onSettings(handler: () -> Unit) {
        v<View>(R.id.btn_gear).setOnClickListener { handler() }
        v<View>(R.id.federation_name).setOnLongClickListener { handler(); true }
    }

    /**
     * The home screen says nothing about the till's link while it works; a merchant only needs to
     * hear about it when it is broken. [tillOk] false: the till service did not answer; [relayOk]
     * false: the till answered but cannot reach this phone's card relay. The pill opens the relay
     * screen, which keeps the detail (relay state, last APDU, port, Wi-Fi IP, till line).
     */
    fun setHealth(tillOk: Boolean, relayOk: Boolean) {
        val problem = when {
            !tillOk -> "Till offline"
            !relayOk -> "Card reader not linked"
            else -> null
        }
        statusAlert.visibility = if (problem == null) View.GONE else View.VISIBLE
        statusAlert.text = problem ?: ""
        statusAlert.contentDescription = problem?.let { "$it. Open connection details" }
        relayTill.text = when {
            !tillOk -> "Till service: not answering"
            !relayOk -> "Till service: up · card relay: not reachable from the till"
            else -> "Till service: up · card relay: linked"
        }
    }

    fun setStatus(s: TillStatus?) {
        fedName.text = s?.federationName ?: act.getString(R.string.app_name)
        balance.text = if (s == null) "" else "Till balance: ${formatSats(s.walletBalanceMsat)} sats"
    }

    /** The amount keypad types a sale, or a top-up on [PosState.TopUpEntering] and while its invoice is made. */
    private val topUpMode: Boolean get() = last is PosState.TopUpEntering || last is PosState.TopUpCreating

    fun setPad(pad: AmountPad) {
        lastPad = pad.display
        amountText.text = "${pad.display} sats"
        val creating = last is PosState.TopUpCreating
        val ok = pad.isChargeable && !creating
        charge.isEnabled = ok
        charge.alpha = if (ok) 1f else 0.4f
        charge.text = when {
            creating -> "Creating invoice…"
            topUpMode -> act.getString(R.string.create_invoice)
            pad.isChargeable -> "Charge ${pad.display} sats"
            else -> act.getString(R.string.charge)
        }
        topUpTitle.visibility = if (topUpMode) View.VISIBLE else View.GONE
        checkBalance.text = act.getString(if (topUpMode) R.string.cancel else R.string.check_balance)
        checkBalance.isEnabled = !creating
    }

    /** Time left on the invoice on screen. */
    fun setTimeLeft(text: String) { invoiceTimeLeft.text = "Expires in $text" }

    private fun renderInvoice(s: PosState.TopUpInvoice) {
        invoiceAmount.text = "Top up ${formatSats(s.topUp.amountMsat)} sats"
        if (qrFor != s.topUp.invoice) {
            qrFor = s.topUp.invoice
            invoiceQr.setImageBitmap(qrBitmap(invoiceQrText(s.topUp.invoice)))
        }
    }

    /** Dots only: the digits never reach a view. */
    fun setPinPad(pad: PinPad) {
        pinDots.text = if (pad.length == 0) "" else pad.masked
        pinConfirm.isEnabled = pad.isComplete
        pinConfirm.alpha = if (pad.isComplete) 1f else 0.4f
    }

    fun setElapsed(seconds: Int) { elapsedS = seconds; if (last is PosState.Signing) renderTapStatus() }

    fun render(state: PosState) {
        last = state
        if (state is PosState.Receipt || state is PosState.Error || state is PosState.BalanceShown || state is PosState.BalanceFailed ||
            state is PosState.TopUpLoaded || state is PosState.TopUpFailed || state is PosState.TopUpInvoice) relayOpen = false
        when (state) {
            PosState.BalanceWaiting, is PosState.BalanceReading -> tapAmount.text = act.getString(R.string.card_balance_label)
            is PosState.BalanceShown -> {
                val amount = "${formatSats(state.liveMsat)} sats"
                cardBalanceAmount.text = amount
                cardBalancePanel.contentDescription = "Card balance: $amount on this card"
                val w = state.waiting
                if (w == null) loadWaiting.visibility = View.GONE
                else {
                    loadWaiting.text = act.getString(R.string.load_waiting_top_up, formatSats(w.amountMsat))
                    if (loadWaiting.visibility != View.VISIBLE) {
                        // Arrives after the balance: fade it in rather than pop.
                        loadWaiting.alpha = 0f
                        loadWaiting.visibility = View.VISIBLE
                        loadWaiting.animate().alpha(1f).setDuration(220).start()
                    }
                }
            }
            is PosState.BalanceFailed -> renderBalanceError(state)
            is PosState.EnteringPin -> {
                pinAmount.text = "Pay ${formatSats(state.priceMsat)} sats"
                pinNotice.visibility = if (state.wrongPin) View.VISIBLE else View.GONE
                pinNotice.text = if (state.wrongPin) wrongPinCopy(state.triesLeft) else ""
            }
            is PosState.WaitingForCard -> tapAmount.text = "${formatSats(state.priceMsat)} sats"
            is PosState.Signing -> tapAmount.text = "${formatSats(state.priceMsat)} sats"
            is PosState.Receipt -> renderReceipt(state.receipt)
            is PosState.Error -> renderError(state)
            is PosState.TopUpInvoice -> renderInvoice(state)
            is PosState.TopUpWaitingForCard -> tapAmount.text = "${formatSats(state.topUp.amountMsat)} sats"
            is PosState.TopUpLoading -> tapAmount.text = "${formatSats(state.topUp.amountMsat)} sats"
            is PosState.TopUpLoaded -> {
                v<TextView>(R.id.receipt_amount).text = "Loaded ${formatSats(state.amountMsat)} sats"
                v<TextView>(R.id.receipt_txid).text = shortTxid(state.loadTxid)
            }
            is PosState.TopUpFailed -> renderTopUpError(state)
            else -> {}
        }
        renderTapStatus()
        show()
    }

    fun resetElapsed() { elapsedS = 0 }

    private fun renderTapStatus() {
        tapStatus.text = when (val s = last) {
            is PosState.WaitingForCard -> if (s.mustRemoveFirst) "Remove the card, then hold it again" else "Hold card to phone"
            is PosState.Signing -> "Paying… keep the card on the phone"
            PosState.BalanceWaiting -> act.getString(R.string.hold_card)
            is PosState.BalanceReading -> act.getString(R.string.reading_card)
            is PosState.TopUpWaitingForCard -> "Hold the card to load ${formatSats(s.topUp.amountMsat)} sats"
            is PosState.TopUpLoading -> "Loading… keep the card on the phone"
            else -> ""
        }
        settle(last is PosState.Signing || last is PosState.BalanceReading || last is PosState.TopUpLoading)
        val l = last
        cancel.visibility = if (l is PosState.WaitingForCard || l is PosState.BalanceWaiting || l is PosState.BalanceReading || l is PosState.TopUpWaitingForCard) View.VISIBLE else View.INVISIBLE
    }

    /** Seconds left before the shown balance gives way to the amount. */
    fun setBalanceCountdown(seconds: Int) {
        cardBalanceCountdown.text = "Back to the amount in $seconds s"
    }

    private var working = false

    /**
     * The card on the phone: the drawing turns from rings to a running arc, and the card picture
     * straightens and lifts a little, as if it had landed. Back to waiting, it tilts again.
     */
    private fun settle(on: Boolean) {
        if (on == working) return
        working = on
        motion.setMode(if (on) TapMotionView.Mode.Working else TapMotionView.Mode.Waiting)
        tapCard.animate().cancel()
        tapCard.animate()
            .rotation(if (on) 0f else -6f)
            .scaleX(if (on) 1.04f else 1f).scaleY(if (on) 1.04f else 1f)
            .translationY(if (on) -6f * act.resources.displayMetrics.density else 0f)
            .setDuration(if (on) 380 else 300).setInterpolator(decelerate).start()
    }

    /** The receipt's tick lands with a small overshoot, then the amount rises in under it. */
    private fun celebrate() {
        val d = act.resources.displayMetrics.density
        receiptCheck.apply {
            animate().cancel(); scaleX = 0.4f; scaleY = 0.4f; alpha = 0f
            animate().scaleX(1f).scaleY(1f).alpha(1f).setDuration(460).setInterpolator(OvershootInterpolator(1.8f)).start()
        }
        receiptAmount.apply {
            animate().cancel(); alpha = 0f; translationY = 12 * d
            animate().alpha(1f).translationY(0f).setStartDelay(140).setDuration(320).setInterpolator(decelerate).start()
        }
    }

    private fun renderReceipt(r: ChargeReceipt) {
        // The price only. Change and the sub-sat rounding the planner adds to save card slots
        // (plan.rs) stay in the till's sales record; at the counter they read like an error.
        v<TextView>(R.id.receipt_amount).text = "Paid ${formatSats(r.priceMsat)} sats"
        v<TextView>(R.id.receipt_txid).text = shortTxid(r.txid)
    }

    private fun renderError(e: PosState.Error) {
        val (title, detail) = errorCopy(e.kind, e.detail)
        errTitle.text = title
        errDetail.text = detail
        val unresolved = e.unresolvedRef != null
        abandon.visibility = if (unresolved) View.VISIBLE else View.GONE
        retry.text = act.getString(if (unresolved) R.string.try_again else R.string.retry)
        retry.visibility = if (e.kind.retryable) View.VISIBLE else View.GONE
        errBack.visibility = if (unresolved) View.GONE else View.VISIBLE
        (errBack as TextView).text = act.getString(R.string.back)
        checkSales.visibility = if (unresolved) View.VISIBLE else View.GONE
    }

    /** A failed top-up step borrows the error screen too. */
    private fun renderTopUpError(e: PosState.TopUpFailed) {
        val (title, detail) = topUpErrorCopy(e.problem, e.paid, e.detail)
        errTitle.text = title
        errDetail.text = detail
        abandon.visibility = View.GONE
        checkSales.visibility = View.GONE
        retry.text = act.getString(
            when {
                e.problem == TopUpProblem.EXPIRED -> R.string.new_invoice
                e.topUp != null -> R.string.tap_again
                else -> R.string.try_again
            }
        )
        retry.visibility = if (e.problem == TopUpProblem.FAILED || e.problem == TopUpProblem.NO_ROOM) View.GONE else View.VISIBLE
        errBack.visibility = View.VISIBLE
        (errBack as TextView).text = act.getString(R.string.done)
    }

    /** A failed balance read borrows the error screen: Try again and Done, nothing about sales. */
    private fun renderBalanceError(e: PosState.BalanceFailed) {
        val (title, detail) = balanceErrorCopy(e.problem)
        errTitle.text = title
        errDetail.text = detail
        abandon.visibility = View.GONE
        checkSales.visibility = View.GONE
        retry.text = act.getString(R.string.try_again)
        retry.visibility = View.VISIBLE
        errBack.visibility = View.VISIBLE
        (errBack as TextView).text = act.getString(R.string.done)
    }

    fun showSales(r: TillResult<SalesList>?) {
        when (r) {
            is TillResult.Ok -> {
                salesHeader.text = "${act.resources.getQuantityString(R.plurals.sales_count, r.value.count.toInt(), r.value.count)} · ${formatSats(r.value.totalPriceMsat)} sats"
                salesAdapter.rows = r.value.sales.reversed()
            }
            is TillResult.Err -> { salesHeader.text = "Could not load sales"; salesAdapter.rows = emptyList() }
            null -> { salesHeader.text = "Loading…"; salesAdapter.rows = emptyList() }
        }
    }

    fun closeRelay(): Boolean { if (!relayOpen) return false; relayOpen = false; show(); return true }
    fun destroy() { motion.stop() }

    fun showRelay() { relayOpen = true; show() }

    private fun show() {
        val target = when {
            relayOpen -> R.id.screen_relay
            else -> when (last) {
                PosState.Entering, PosState.TopUpEntering, is PosState.TopUpCreating -> R.id.screen_amount
                is PosState.TopUpInvoice -> R.id.screen_invoice
                is PosState.TopUpWaitingForCard, is PosState.TopUpLoading -> R.id.screen_tap
                is PosState.TopUpLoaded -> R.id.screen_receipt
                is PosState.TopUpFailed -> R.id.screen_error
                is PosState.EnteringPin -> R.id.screen_pin
                is PosState.WaitingForCard, is PosState.Signing, PosState.BalanceWaiting, is PosState.BalanceReading -> R.id.screen_tap
                is PosState.BalanceShown -> R.id.screen_card_balance
                is PosState.BalanceFailed -> R.id.screen_error
                is PosState.Receipt -> R.id.screen_receipt
                is PosState.Error -> R.id.screen_error
                is PosState.Sales -> R.id.screen_sales
            }
        }
        screens.forEach { v<View>(it).visibility = if (it == target) View.VISIBLE else View.GONE }
        if (target == R.id.screen_tap) motion.start() else motion.stop()
        if (target == R.id.screen_receipt && shownTarget != R.id.screen_receipt) celebrate()
        shownTarget = target
    }

    /**
     * Settings: the till URL, pairing (scan the till's QR code, or paste its text), and the relay.
     * [paired] is the paired till's URL, or null. [onPairText] returns false for text that is not
     * a pairing code.
     */
    fun settingsDialog(
        current: String,
        paired: String?,
        onSave: (String) -> Unit,
        onRelay: () -> Unit,
        onScan: () -> Unit,
        onPairText: (String) -> Boolean,
        onUnpair: () -> Unit,
    ) {
        val pad = (16 * act.resources.displayMetrics.density).toInt()
        val input = EditText(act).apply { setText(current); inputType = InputType.TYPE_TEXT_VARIATION_URI; setSingleLine() }
        val state = TextView(act).apply {
            text = if (paired != null) "Paired with $paired: the card goes to the till over the network." else "Not paired: the till reaches the card over USB (adb)."
            setPadding(0, pad / 2, 0, pad / 2)
        }
        val pasted = EditText(act).apply { hint = "or paste fedimintcard-till:…"; setSingleLine(); inputType = InputType.TYPE_TEXT_VARIATION_URI }
        var dialog: AlertDialog? = null
        val scan = Button(act).apply { text = "Pair with till (scan QR)"; setOnClickListener { dialog?.dismiss(); onScan() } }
        val usePasted = Button(act).apply {
            text = "Pair with pasted text"
            setOnClickListener {
                if (onPairText(pasted.text.toString())) dialog?.dismiss() else pasted.error = "Not a till pairing code"
            }
        }
        val unpair = Button(act).apply {
            text = "Unpair (use USB)"
            visibility = if (paired != null) View.VISIBLE else View.GONE
            setOnClickListener { onUnpair(); dialog?.dismiss() }
        }
        val body = android.widget.LinearLayout(act).apply {
            orientation = android.widget.LinearLayout.VERTICAL
            setPadding(pad, pad / 2, pad, 0)
            addView(input); addView(state); addView(scan); addView(pasted); addView(usePasted); addView(unpair)
        }
        AlertDialog.Builder(act)
            .setTitle("Till")
            .setView(android.widget.ScrollView(act).apply { addView(body) })
            .setPositiveButton("Save", null)
            .setNeutralButton("Card relay", { _, _ -> onRelay() })
            .setNegativeButton("Cancel", null)
            .create().also { d ->
                dialog = d
                d.setOnShowListener {
                    d.getButton(AlertDialog.BUTTON_POSITIVE).setOnClickListener {
                        val n = normalizeBaseUrl(input.text.toString())
                        if (n == null) input.error = "Enter a URL like http://127.0.0.1:9877"
                        else { onSave(n); d.dismiss() }
                    }
                }
            }.show()
    }

    private inner class SalesAdapter : BaseAdapter() {
        var rows: List<Sale> = emptyList()
            set(x) { field = x; notifyDataSetChanged() }
        private val fmt = SimpleDateFormat("HH:mm:ss", Locale.getDefault())
        override fun getCount() = rows.size
        override fun getItem(i: Int) = rows[i]
        override fun getItemId(i: Int) = rows[i].id
        override fun getView(i: Int, convert: View?, parent: ViewGroup): View {
            val row = convert ?: LayoutInflater.from(act).inflate(R.layout.row_sale, parent, false)
            val s = rows[i]
            row.findViewById<TextView>(R.id.row_time).text = fmt.format(Date(s.tsUnixMs))
            row.findViewById<TextView>(R.id.row_price).text = "${formatSats(s.priceMsat)} sats"
            row.findViewById<TextView>(R.id.row_received).text = "received ${formatSats(s.receivedMsat)}"
            row.findViewById<TextView>(R.id.row_txid).text = shortTxid(s.txid)
            return row
        }
    }
}

/** Title and detail for a failed balance read, in words the card holder can act on. */
fun balanceErrorCopy(p: BalanceProblem): Pair<String, String> = when (p) {
    BalanceProblem.CARD_MOVED -> "The card moved away" to "Hold the card still against the back of the phone until the balance shows."
    BalanceProblem.NOT_OUR_CARD -> "Not a card for this till" to "This card isn't set up here. Try another card."
    BalanceProblem.TILL_UNREACHABLE -> "Can't reach the till" to "Check that the till is running, then try again."
    BalanceProblem.RELAY_UNREACHABLE -> "The till can't reach this phone" to "Check the connection in Settings, then try again."
    BalanceProblem.BUSY -> "The till is busy" to "Try again in a moment."
    BalanceProblem.CARD_SERVICE -> "Card service unreachable" to "Check the network, then try again."
    BalanceProblem.TIMEOUT -> "The till took too long" to "Hold the card to the phone and try again."
    BalanceProblem.OTHER -> "Couldn't read the card" to "Hold the card to the phone and try again."
}

/** Title and detail copy for a charge error. */
fun errorCopy(kind: ErrorKind, detail: String): Pair<String, String> = when (kind) {
    ErrorKind.NO_CARD -> "Card not detected" to "Hold card flat on the phone's NFC antenna"
    ErrorKind.INSUFFICIENT -> "Card balance too low" to ""
    ErrorKind.NO_EXACT_PLAN -> "Card needs a top up" to ""
    ErrorKind.DECLINED -> "Card declined" to detail
    ErrorKind.RELAY_UNREACHABLE -> "Phone relay unreachable" to "Check adb forward tcp:9876 tcp:9876"
    ErrorKind.BUSY -> "Till is busy" to "Try again in a moment"
    ErrorKind.RECOVERING -> "Till is recovering" to "Try again in a few seconds"
    ErrorKind.CARD_ERROR -> "Card error" to detail
    ErrorKind.UNDECIDED -> "Payment pending" to "Waiting for the federation. This screen updates when the sale is recorded; Try again re-checks without charging twice."
    ErrorKind.TIMEOUT_UNKNOWN -> "Outcome unknown" to "The till did not answer. The card may have been charged. This screen updates when the sale is recorded; Try again re-checks without charging twice."
    ErrorKind.TILL_UNREACHABLE -> "Till service unreachable" to "Check adb reverse tcp:9877 tcp:9877"
    ErrorKind.WRONG_PIN -> wrongPinCopy(parseTriesLeft(detail)) to "Ask the card holder to enter it again"
    ErrorKind.PIN_BLOCKED -> "PIN blocked" to "This card's PIN is blocked. The card can't pay."
    ErrorKind.PIN_REQUIRED -> "Something went wrong" to "The app sent the charge without the holder's PIN. This is an app bug; the card was not touched."
    ErrorKind.CONFLICT -> "Something went wrong" to detail
    ErrorKind.CARD_CHANGED -> "Card changed" to "The card was used elsewhere. Hold it to the phone and try again."
    ErrorKind.UNKNOWN_CARD -> "Card not recognised" to "This card is not set up for this till"
    ErrorKind.OPS_UNREACHABLE -> "Card service unreachable" to "Check the network, then try again"
    ErrorKind.OPS_REFUSED -> "Card service refused the till" to "Check the till token"
    ErrorKind.BAD_REQUEST, ErrorKind.INTERNAL, ErrorKind.UNKNOWN -> "Something went wrong" to detail
}

/** The line over the PIN pad after a refused PIN. Never includes the PIN. */
fun wrongPinCopy(triesLeft: Int?): String = when (triesLeft) {
    null -> "Wrong PIN"
    1 -> "Wrong PIN — 1 try left"
    else -> "Wrong PIN — $triesLeft tries left"
}

/** A crisp QR bitmap, one pixel per module scaled up by the view (filter off keeps edges hard). */
fun qrBitmap(text: String): android.graphics.Bitmap {
    val m = com.google.zxing.qrcode.QRCodeWriter().encode(
        text, com.google.zxing.BarcodeFormat.QR_CODE, 0, 0,
        mapOf(com.google.zxing.EncodeHintType.MARGIN to 0, com.google.zxing.EncodeHintType.ERROR_CORRECTION to com.google.zxing.qrcode.decoder.ErrorCorrectionLevel.M),
    )
    val scale = 12
    val w = m.width * scale
    val px = IntArray(w * w)
    for (y in 0 until w) for (x in 0 until w) px[y * w + x] = if (m.get(x / scale, y / scale)) 0xFF0A163C.toInt() else 0xFFFFFFFF.toInt()
    return android.graphics.Bitmap.createBitmap(px, w, w, android.graphics.Bitmap.Config.ARGB_8888)
}
