package org.fedimintcard.relay

import android.animation.ObjectAnimator
import android.app.Activity
import android.app.AlertDialog
import android.graphics.drawable.GradientDrawable
import android.text.InputType
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.view.animation.LinearInterpolator
import android.animation.PropertyValuesHolder
import android.widget.BaseAdapter
import android.widget.Button
import android.widget.EditText
import android.widget.ListView
import android.widget.TextView
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/** Binds views, switches screens, renders [PosState]. All calls on the UI thread. */
class PosUi(private val act: Activity, private val onEvent: (PosEvent) -> Unit, private val onKey: (Int?) -> Unit) {
    private fun <T : View> v(id: Int): T = act.findViewById(id)

    private val screens = listOf(R.id.screen_amount, R.id.screen_tap, R.id.screen_receipt, R.id.screen_error, R.id.screen_sales, R.id.screen_relay)
    private val dot: View = v(R.id.dot)
    private val fedName: TextView = v(R.id.federation_name)
    private val balance: TextView = v(R.id.balance_line)
    private val amountText: TextView = v(R.id.amount_text)
    private val charge: Button = v(R.id.btn_charge)
    private val tapAmount: TextView = v(R.id.tap_amount)
    private val tapStatus: TextView = v(R.id.tap_status)
    private val cancel: Button = v(R.id.btn_cancel)
    private val pulse: View = v(R.id.pulse)
    private val errTitle: TextView = v(R.id.error_title)
    private val errDetail: TextView = v(R.id.error_detail)
    private val retry: Button = v(R.id.btn_retry)
    private val checkSales: Button = v(R.id.btn_check_sales)
    private val errBack: View = v(R.id.btn_error_back)
    private val salesHeader: TextView = v(R.id.sales_header)
    private val salesList: ListView = v(R.id.sales_list)
    private val salesAdapter = SalesAdapter()

    val relayStatus: TextView = v(R.id.relay_status)
    val relayLast: TextView = v(R.id.relay_last)
    val relayInfo: TextView = v(R.id.relay_info)

    private var relayOpen = false
    private var last: PosState = PosState.Entering
    private var lastPad = ""
    private var elapsedS = 0
    private val pulser = ObjectAnimator.ofPropertyValuesHolder(
        pulse, PropertyValuesHolder.ofFloat("scaleX", 0.7f, 1.1f), PropertyValuesHolder.ofFloat("scaleY", 0.7f, 1.1f),
        PropertyValuesHolder.ofFloat("alpha", 1f, 0.35f),
    ).apply { duration = 900; repeatCount = ObjectAnimator.INFINITE; repeatMode = ObjectAnimator.REVERSE; interpolator = LinearInterpolator() }

    init {
        salesList.adapter = salesAdapter
        val digits = listOf(R.id.key_0, R.id.key_1, R.id.key_2, R.id.key_3, R.id.key_4, R.id.key_5, R.id.key_6, R.id.key_7, R.id.key_8, R.id.key_9)
        digits.forEachIndexed { d, id -> v<View>(id).setOnClickListener { onKey(d) } }
        v<View>(R.id.key_c).setOnClickListener { onKey(-1) }
        v<View>(R.id.key_back).setOnClickListener { onKey(null) }
        charge.setOnClickListener { onEvent(PosEvent.ChargePressed) }
        cancel.setOnClickListener { onEvent(PosEvent.CancelPressed) }
        retry.setOnClickListener { onEvent(PosEvent.RetryPressed) }
        v<View>(R.id.btn_error_back).setOnClickListener { onEvent(PosEvent.BackPressed) }
        checkSales.setOnClickListener { onEvent(PosEvent.SalesOpened) }
        v<View>(R.id.btn_sales).setOnClickListener { onEvent(PosEvent.SalesOpened) }
        v<View>(R.id.btn_sales_back).setOnClickListener { onEvent(PosEvent.BackPressed) }
        v<View>(R.id.btn_new_sale).setOnClickListener { onEvent(PosEvent.NewSalePressed) }
        v<View>(R.id.btn_relay_back).setOnClickListener { relayOpen = false; show() }
    }

    fun onSettings(handler: () -> Unit) {
        v<View>(R.id.btn_gear).setOnClickListener { handler() }
        v<View>(R.id.federation_name).setOnLongClickListener { handler(); true }
    }

    fun setConnection(ok: Boolean?) {
        val c = when (ok) { true -> R.color.ok; false -> R.color.bad; null -> R.color.muted }
        (dot.background.mutate() as GradientDrawable).setColor(act.getColor(c))
    }

    fun setStatus(s: TillStatus?) {
        fedName.text = s?.federationName ?: act.getString(R.string.app_name)
        balance.text = if (s == null) "" else "Till balance: ${formatSats(s.walletBalanceMsat)} sats"
    }

    fun setPad(pad: AmountPad) {
        lastPad = pad.display
        amountText.text = "${pad.display} sats"
        charge.isEnabled = pad.isChargeable
        charge.alpha = if (pad.isChargeable) 1f else 0.4f
        charge.text = if (pad.isChargeable) "Charge ${pad.display} sats" else act.getString(R.string.charge)
    }

    fun setElapsed(seconds: Int) { elapsedS = seconds; if (last is PosState.Signing) renderTapStatus() }

    fun render(state: PosState) {
        last = state
        if (state is PosState.Receipt || state is PosState.Error) relayOpen = false
        when (state) {
            is PosState.WaitingForCard -> tapAmount.text = "${formatSats(state.priceMsat)} sats"
            is PosState.Signing -> tapAmount.text = "${formatSats(state.priceMsat)} sats"
            is PosState.Receipt -> renderReceipt(state.receipt)
            is PosState.Error -> renderError(state)
            else -> {}
        }
        renderTapStatus()
        show()
    }

    fun resetElapsed() { elapsedS = 0 }

    private fun renderTapStatus() {
        tapStatus.text = when (val s = last) {
            is PosState.WaitingForCard -> if (s.mustRemoveFirst) "Remove the card, then hold it again" else "Hold card to phone"
            is PosState.Signing -> if (elapsedS == 0) "Card present" else "Signing… ${elapsedS}s"
            else -> ""
        }
        cancel.visibility = if (last is PosState.WaitingForCard) View.VISIBLE else View.INVISIBLE
    }

    private fun renderReceipt(r: ChargeReceipt) {
        v<TextView>(R.id.receipt_amount).text = "${formatSats(r.priceMsat)} sats"
        val change = v<TextView>(R.id.receipt_change)
        val cm = r.change.sumOf { it.msat }
        change.visibility = if (r.change.isEmpty()) View.GONE else View.VISIBLE
        change.text = "Change returned to card: ${formatSats(cm)} sats across ${r.change.size} notes"
        val over = v<TextView>(R.id.receipt_overpaid)
        over.visibility = if (r.overpaidMsat == 0L) View.GONE else View.VISIBLE
        over.text = "Overpaid: ${formatSats(r.overpaidMsat)} sats"
        v<TextView>(R.id.receipt_txid).text = shortTxid(r.txid)
    }

    private fun renderError(e: PosState.Error) {
        val (title, detail) = errorCopy(e.kind, e.detail)
        errTitle.text = title
        errDetail.text = detail
        retry.text = act.getString(if (e.kind.needsSalesCheck) R.string.try_again else R.string.retry)
        errBack.visibility = if (e.kind.needsSalesCheck) View.GONE else View.VISIBLE
        checkSales.visibility = if (e.kind.needsSalesCheck) View.VISIBLE else View.GONE
    }

    fun showSales(r: TillResult<SalesList>?) {
        when (r) {
            is TillResult.Ok -> {
                salesHeader.text = "${r.value.count} sales · ${formatSats(r.value.totalPriceMsat)} sats"
                salesAdapter.rows = r.value.sales.reversed()
            }
            is TillResult.Err -> { salesHeader.text = "Could not load sales"; salesAdapter.rows = emptyList() }
            null -> { salesHeader.text = "Loading…"; salesAdapter.rows = emptyList() }
        }
    }

    fun closeRelay(): Boolean { if (!relayOpen) return false; relayOpen = false; show(); return true }
    fun destroy() { pulser.cancel() }

    fun showRelay() { relayOpen = true; show() }

    private fun show() {
        val target = when {
            relayOpen -> R.id.screen_relay
            else -> when (last) {
                PosState.Entering -> R.id.screen_amount
                is PosState.WaitingForCard, is PosState.Signing -> R.id.screen_tap
                is PosState.Receipt -> R.id.screen_receipt
                is PosState.Error -> R.id.screen_error
                is PosState.Sales -> R.id.screen_sales
            }
        }
        screens.forEach { v<View>(it).visibility = if (it == target) View.VISIBLE else View.GONE }
        if (target == R.id.screen_tap) { if (!pulser.isStarted) pulser.start() } else pulser.cancel()
    }

    fun settingsDialog(current: String, onSave: (String) -> Unit, onRelay: () -> Unit) {
        val input = EditText(act).apply { setText(current); inputType = InputType.TYPE_TEXT_VARIATION_URI; setSingleLine() }
        AlertDialog.Builder(act)
            .setTitle("Till service URL")
            .setView(input)
            .setPositiveButton("Save", null)
            .setNeutralButton("Card relay", { _, _ -> onRelay() })
            .setNegativeButton("Cancel", null)
            .create().also { d ->
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
    ErrorKind.BAD_REQUEST, ErrorKind.INTERNAL, ErrorKind.UNKNOWN -> "Something went wrong" to detail
}
