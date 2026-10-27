package org.fedimintcard.relay

import android.app.Activity
import android.content.Context
import android.net.wifi.WifiManager
import android.nfc.NfcAdapter
import android.nfc.Tag
import android.nfc.TagLostException
import android.nfc.tech.IsoDep
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.widget.Toast
import android.view.WindowManager
import android.widget.TextView
import java.util.concurrent.atomic.AtomicReference

class MainActivity : Activity(), NfcAdapter.ReaderCallback {
    private val isoDep = AtomicReference<IsoDep?>(null)
    private var server: ApduServer? = null
    private lateinit var status: TextView
    private lateinit var last: TextView
    private lateinit var info: TextView
    private var nfc: NfcAdapter? = null

    /** Called on the UI thread with true when a card is connected, false when it is gone. */
    @Volatile var cardListener: ((Boolean) -> Unit)? = null

    private lateinit var flow: PosFlow
    private lateinit var ui: PosUi
    private lateinit var client: TillClient
    private lateinit var settings: Settings
    private val handler = Handler(Looper.getMainLooper())
    private var elapsed = 0
    private var failures = 0
    private var pollInFlight = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        setContentView(R.layout.activity_main)
        settings = Settings(this)
        client = TillClient({ settings.baseUrl }, UrlConnectionTransport())
        flow = PosFlow()
        ui = PosUi(this, ::dispatch, ::onKey)
        status = ui.relayStatus; last = ui.relayLast; info = ui.relayInfo
        ui.onSettings { ui.settingsDialog(settings.baseUrl, { settings.baseUrl = it; failures = 0; pollNow() }, { ui.showRelay() }) }
        cardListener = { present -> ui.setCard(present); dispatch(if (present) PosEvent.CardPresent else PosEvent.CardLost) }
        ui.setPad(flow.pad)
        ui.render(flow.state)
        nfc = NfcAdapter.getDefaultAdapter(this)
        if (nfc == null) status.text = "no NFC"
        else if (nfc?.isEnabled == false) last.text = "NFC is switched off"
        try {
            server = ApduServer(PORT, { isoDep.get()?.takeIf { it.isConnected }?.let { d -> CardLink { a -> try { d.transceive(a) } catch (e: TagLostException) { val lost = isoDep.compareAndSet(d, null); runOnUiThread { status.text = "no card"; if (lost) cardListener?.invoke(false) }; throw e } } } }) { ev ->
                runOnUiThread { last.text = ev }
            }.also { it.start() }
        } catch (e: Exception) {
            last.text = "server failed: ${e.message}"
        }
        info.text = "port $PORT\nWi-Fi IP: ${wifiIp()}"
    }

    override fun onResume() {
        super.onResume()
        handler.post(poll)
        resumed = true
        if (flow.unresolvedRef != null) scheduleSalesPoll()
        nfc?.enableReaderMode(
            this, this,
            NfcAdapter.FLAG_READER_NFC_A or NfcAdapter.FLAG_READER_NFC_B or
                NfcAdapter.FLAG_READER_SKIP_NDEF_CHECK or NfcAdapter.FLAG_READER_NO_PLATFORM_SOUNDS,
            null,
        )
    }

    override fun onPause() {
        resumed = false; salesGen.cancel(); handler.removeCallbacks(poll)
        nfc?.disableReaderMode(this)
        super.onPause()
    }

    override fun onDestroy() {
        ui.destroy()
        handler.removeCallbacksAndMessages(null)
        cardListener = null
        server?.stop()
        isoDep.getAndSet(null)?.let { try { it.close() } catch (_: Exception) {} }
        super.onDestroy()
    }

    override fun onTagDiscovered(tag: Tag) {
        val d = IsoDep.get(tag) ?: return
        try {
            d.connect()
            d.timeout = 5000
            isoDep.getAndSet(d)?.let { try { it.close() } catch (_: Exception) {} }
            runOnUiThread { status.text = "card present"; cardListener?.invoke(true) }
            // Poll for removal so the status returns to "no card".
            Thread {
                while (isoDep.get() === d) {
                    try {
                        if (!d.isConnected) throw TagLostException()
                        Thread.sleep(300)
                    } catch (e: Exception) {
                        val lost = isoDep.compareAndSet(d, null)
                        runOnUiThread { status.text = "no card"; if (lost) cardListener?.invoke(false) }
                    }
                }
            }.start()
        } catch (e: TagLostException) {
            val lost = isoDep.compareAndSet(d, null)
            runOnUiThread { status.text = "no card"; if (lost) cardListener?.invoke(false) }
        } catch (e: Exception) {
            runOnUiThread { last.text = "connect failed: ${e.message}" }
        }
    }

    private fun onKey(k: Int?) {
        if (flow.state != PosState.Entering) return
        when (k) { null -> flow.pad.backspace(); -1 -> flow.pad.clear(); else -> flow.pad.press(k) }
        ui.setPad(flow.pad)
    }

    private fun dispatch(e: PosEvent) {
        val before = flow.state
        val refBefore = flow.unresolvedRef
        val effects = flow.dispatch(e)
        val now = flow.state
        ui.setPad(flow.pad)
        ui.render(now)
        if (now is PosState.Signing && before !is PosState.Signing) {
            elapsed = 0; ui.resetElapsed()
            handler.removeCallbacks(tick); handler.postDelayed(tick, 1000)
        }
        if (now is PosState.Sales && before !is PosState.Sales) loadSales()
        val refNow = flow.unresolvedRef
        if (refNow == null) salesGen.cancel()
        else if (refNow != refBefore) scheduleSalesPoll()
        if (now is PosState.Receipt) pollNow()
        for (fx in effects) if (fx is PosEffect.StartCharge) {
            Thread {
                val r = client.charge(fx.priceMsat, fx.clientRef)
                runOnUiThread { dispatch(PosEvent.ChargeDone(fx.chargeId, r)) }
            }.start()
        }
    }

    private val tick = object : Runnable {
        override fun run() {
            if (flow.state !is PosState.Signing) return
            ui.setElapsed(++elapsed)
            handler.postDelayed(this, 1000)
        }
    }

    private val poll = object : Runnable {
        override fun run() {
            pollNow()
            handler.postDelayed(this, 5000)
        }
    }

    /** While a sale's outcome is unknown, look for its client_ref in the sales ledger every 3 s. */
    private val salesGen = PollGeneration()
    private var resumed = false

    private fun scheduleSalesPoll() {
        val gen = salesGen.next() // supersedes any running loop
        if (resumed) handler.postDelayed({ runSalesPoll(gen) }, 3000)
    }

    private fun runSalesPoll(gen: Int) {
        if (!salesGen.isCurrent(gen)) return
        val ref = flow.unresolvedRef ?: return
        Thread {
            val r = client.sales()
            runOnUiThread {
                if (!salesGen.isCurrent(gen)) return@runOnUiThread
                val hit = (r as? TillResult.Ok)?.value?.sales?.firstOrNull { it.clientRef == ref }
                if (hit != null && flow.unresolvedRef == ref) dispatch(PosEvent.SaleFound(hit))
                else if (flow.unresolvedRef != null) handler.postDelayed({ runSalesPoll(gen) }, 3000)
            }
        }.start()
    }

    private fun pollNow() {
        if (flow.state is PosState.Signing || pollInFlight) return
        pollInFlight = true
        Thread {
            val r = client.status()
            runOnUiThread {
                pollInFlight = false
                if (r is TillResult.Ok) {
                    failures = 0
                    ui.setStatus(r.value)
                    ui.setConnection(r.value.cardRelayOk)
                } else if (++failures >= 2) ui.setConnection(false)
            }
        }.start()
    }

    private fun loadSales() {
        ui.showSales(null)
        Thread {
            val r = client.sales()
            runOnUiThread { ui.showSales(r) }
        }.start()
    }

    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        if (ui.closeRelay()) return
        val ev = flow.systemBack()
        if (ev != null) dispatch(ev)
        else if (flow.state == PosState.Entering) Toast.makeText(this, "Use the app controls", Toast.LENGTH_SHORT).show()
    }

    @Suppress("DEPRECATION")
    private fun wifiIp(): String {
        val wm = applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager ?: return "unknown"
        val ip = wm.connectionInfo?.ipAddress ?: 0
        if (ip == 0) return "not connected"
        return "${ip and 0xFF}.${ip shr 8 and 0xFF}.${ip shr 16 and 0xFF}.${ip shr 24 and 0xFF}"
    }

    companion object { const val PORT = 9876 }
}
