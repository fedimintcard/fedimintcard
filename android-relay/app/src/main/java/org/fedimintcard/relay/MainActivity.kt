package org.fedimintcard.relay

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.util.Log
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
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
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
    /** The phone's offline side (docs/POS-OFFLINE.md § Android): local charges, the kit, the sync. */
    private lateinit var offline: OfflineTill
    private val handler = Handler(Looper.getMainLooper())
    private var elapsed = 0
    private var failures = 0
    private var pollInFlight = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        setContentView(R.layout.activity_main)
        settings = Settings(this)
        client = TillClient({ settings.baseUrl }, UrlConnectionTransport(), { settings.pairing?.token })
        offline = OfflineTill(
            { OfflineRuntime.core }, KitStore(noBackupFilesDir), client,
            { settings.pairing?.let(::pairingId) }, { OfflineRuntime.failure },
        )
        flow = PosFlow()
        ui = PosUi(this, ::dispatch, ::onKey, ::onPinKey)
        status = ui.relayStatus; last = ui.relayLast; info = ui.relayInfo
        ui.onSettings { openSettings() }
        cardListener = { present -> dispatch(if (present) PosEvent.CardPresent else PosEvent.CardLost) }
        ui.setPad(flow.pad)
        ui.render(flow.state)
        // Opens the core's store once per process, off the main thread; the home screen then shows
        // what the phone holds (a sale held for its card included, after a restart).
        OfflineRuntime.start(noBackupFilesDir) {
            val c = offline.counts()
            runOnUiThread { ui.setPhone(c) }
        }
        nfc = NfcAdapter.getDefaultAdapter(this)
        if (nfc == null) status.text = "no NFC"
        else if (nfc?.isEnabled == false) last.text = "NFC is switched off"
        try {
            // While the phone's core drives the card, the USB relay has no card to offer anyone else.
            server = ApduServer(PORT, { if (offline.localCharging) null else isoDep.get()?.takeIf { it.isConnected }?.let { d -> cardOf(d) } }) { ev ->
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
        if (flow.unresolvedRef != null && !flow.unresolvedHeld) scheduleSalesPoll()
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

    /** Leaving the app drops any PIN typed or waiting for the card. */
    override fun onStop() {
        dispatch(PosEvent.Backgrounded)
        super.onStop()
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
                        tagGone(d)
                    }
                }
            }.start()
        } catch (e: TagLostException) {
            tagGone(d)
        } catch (e: Exception) {
            runOnUiThread { last.text = "connect failed: ${e.message}" }
        }
    }

    /** The held card, answering APDUs; a lost tag is reported once and rethrown. */
    private fun cardOf(d: IsoDep) = CardLink { a ->
        try { d.transceive(a) } catch (e: TagLostException) { tagGone(d); throw e }
    }

    private fun tagGone(d: IsoDep) {
        val lost = isoDep.compareAndSet(d, null)
        if (lost) activeLink?.cardLost()
        runOnUiThread { status.text = "no card"; if (lost) cardListener?.invoke(false) }
    }

    /** The card on the phone now, or one that has left (every APDU fails). */
    private fun currentCard(): CardLink = isoDep.get()?.let { cardOf(it) } ?: CardLink { throw TagLostException() }

    /** The link of the job in flight, if it runs over one. */
    @Volatile private var activeLink: TillLink? = null

    /**
     * Runs a card job. Paired: opens a card link answered from the card on the phone now, runs
     * [job] with its id (the till uses only that link as the card), and closes it. Unpaired: runs
     * [job] with null, and the till reaches the card through the USB relay as before. A link that
     * cannot be opened is [unreachable] (nothing was sent), told whether the failure lets a charge
     * go on the phone ([linkFailureFallsBack]).
     */
    private fun <T> withCard(job: (String?) -> T, unreachable: (fallsBack: Boolean) -> T): T {
        val p = settings.pairing ?: return job(null)
        val d = isoDep.get()
        val card = if (d != null) cardOf(d) else CardLink { throw TagLostException() }
        val link = try {
            TillLink.open(settings.baseUrl, p.token, card)
        } catch (e: Exception) {
            Log.w(TAG, "card link failed: ${e.javaClass.simpleName}")
            return unreachable(linkFailureFallsBack(e))
        }
        activeLink = link
        if (isoDep.get() !== d || d == null) link.cardLost()
        try {
            return job(link.id)
        } finally {
            activeLink = null
            link.close()
        }
    }

    /** Settings, with the phone's offline side read off the main thread (core calls). */
    private fun openSettings() {
        Thread {
            val view = offline.view()
            runOnUiThread {
                ui.settingsDialog(
                    settings.baseUrl,
                    settings.pairing?.url,
                    view,
                    {
                        val changed = it != settings.baseUrl
                        settings.baseUrl = it
                        if (changed) tillChanged() else { failures = 0; pollNow() }
                    },
                    { ui.showRelay() },
                    { startActivityForResult(Intent(this, ScanActivity::class.java), SCAN_REQUEST) },
                    { text -> pairWith(text) },
                    {
                        confirmPending(UNPAIR_TITLE, "Unpair") {
                            settings.pairing = null; tillChanged()
                            Toast.makeText(this, "Unpaired: using USB", Toast.LENGTH_SHORT).show()
                        }
                    },
                    { txid, done ->
                        Thread {
                            val ok = offline.dismiss(txid)
                            runOnUiThread { done(ok); showPhoneCounts() }
                        }.start()
                    },
                )
            }
        }.start()
    }

    /** Stores a pairing code (scanned or pasted). False if [text] is not one. */
    private fun pairWith(text: String): Boolean {
        val p = parsePairing(text) ?: return false
        val pair = {
            settings.pairing = p
            tillChanged()
            Toast.makeText(this, "Paired with ${p.url}", Toast.LENGTH_SHORT).show()
        }
        val now = settings.pairing
        if (now == null || now == p) pair() else confirmPending(REPAIR_TITLE, "Pair", pair)
        return true
    }

    /**
     * The phone left the till it was polling (another pairing, unpaired, or another URL saved): what
     * that till said goes (its balance and federation on the home screen, its double-spend count in
     * Settings), its failures with it, and the till set now is asked. Until it answers, nothing of a
     * till shows (as on iOS).
     */
    private fun tillChanged() {
        ui.setStatus(null)
        failures = 0
        pollNow()
    }

    /**
     * Payments waiting on this phone sync only to the till they were taken for: before unpairing or
     * pairing another till, say how many stay behind (they are kept and listed, never dropped).
     */
    private fun confirmPending(title: String, positive: String, action: () -> Unit) {
        Thread {
            val n = offline.view().pending
            runOnUiThread { if (n == 0) action() else ui.confirm(title, pendingCopy(n), positive, action) }
        }.start()
    }

    /** Reads the phone's counts off the main thread and shows them. */
    private fun showPhoneCounts() {
        Thread {
            val c = offline.counts()
            runOnUiThread { ui.setPhone(c) }
        }.start()
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != SCAN_REQUEST || resultCode != RESULT_OK) return
        val text = data?.getStringExtra(ScanActivity.EXTRA_TEXT) ?: return
        if (!pairWith(text)) Toast.makeText(this, "Not a till pairing code", Toast.LENGTH_SHORT).show()
    }

    private fun onKey(k: Int?) {
        val pad = when (flow.state) { PosState.Entering -> flow.pad; PosState.TopUpEntering -> flow.topUpPad; else -> return }
        when (k) { null -> pad.backspace(); -1 -> pad.clear(); else -> pad.press(k) }
        ui.setPad(pad)
    }

    /** The keypad shows the top-up amount while one is typed or its invoice is made. */
    private fun padFor(s: PosState) = if (s is PosState.TopUpEntering || s is PosState.TopUpCreating) flow.topUpPad else flow.pad

    /** PIN digits go to the pad only while the PIN screen shows; nothing about them is logged. */
    private fun onPinKey(k: Int?) {
        if (flow.state !is PosState.EnteringPin) return
        when (k) { null -> flow.pinPad.backspace(); -1 -> flow.pinPad.clear(); else -> flow.pinPad.press(k) }
        ui.setPinPad(flow.pinPad)
    }

    /** No screenshots, recents thumbnail or screen cast of the PIN screen. */
    private fun secureFor(s: PosState) {
        if (s is PosState.EnteringPin) window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        else window.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
    }

    private fun dispatch(e: PosEvent) {
        val before = flow.state
        val refBefore = flow.unresolvedRef
        val effects = flow.dispatch(e)
        val now = flow.state
        ui.setPinPad(flow.pinPad)
        secureFor(now)
        ui.render(now)
        ui.setPad(padFor(now)) // after render: the keypad's buttons depend on the state
        if (now is PosState.Signing && before !is PosState.Signing) {
            elapsed = 0; ui.resetElapsed()
            handler.removeCallbacks(tick); handler.postDelayed(tick, 1000)
        }
        if (now is PosState.Sales && before !is PosState.Sales) loadSales()
        val refNow = flow.unresolvedRef
        // A sale held on the phone is not in the till's sales: only its card's tap here settles it.
        if (refNow == null || flow.unresolvedHeld) salesGen.cancel()
        else if (refNow != refBefore) scheduleSalesPoll()
        if (now is PosState.Receipt || now is PosState.TopUpLoaded) pollNow()
        // The waiting top-up arriving changes the state, not the read: the countdown keeps going.
        if (now is PosState.BalanceShown && (before !is PosState.BalanceShown || before.readId != now.readId)) startBalanceCountdown(now.readId)
        if (now is PosState.TopUpInvoice && (before !is PosState.TopUpInvoice || before.topUp.topupId != now.topUp.topupId)) watchInvoice(now.topUp)
        for (fx in effects) when (fx) {
            // The till first, unless the phone holds the sale; on the phone when the link cannot
            // open and nothing was ever posted for the sale (OfflineTill.runCharge). Wipes fx.pin.
            is PosEffect.StartCharge -> Thread {
                val r = offline.runCharge(fx.priceMsat, fx.clientRef, fx.pin, ::currentCard) { sent ->
                    withCard(
                        { link -> sent(); client.charge(fx.priceMsat, fx.clientRef, fx.pin, link) },
                        { fallsBack -> if (fallsBack) null else ChargeResult.Failed(TillError.Unreachable) },
                    )
                }
                val counts = offline.counts()
                runOnUiThread { ui.setPhone(counts); dispatch(PosEvent.ChargeDone(fx.chargeId, r)) }
            }.start()
            is PosEffect.StartBalanceRead -> readCardBalance(fx.readId)
            is PosEffect.CreateTopUp -> Thread {
                val r = client.createTopUp(fx.amountMsat, fx.clientRef, fx.shortId)
                runOnUiThread { dispatch(PosEvent.TopUpCreated(fx.createId, r)) }
            }.start()
            is PosEffect.FetchWaiting -> Thread {
                val r = client.waitingTopUps(fx.shortId)
                runOnUiThread { dispatch(PosEvent.WaitingFetched(fx.readId, r)) }
            }.start()
            is PosEffect.LoadTopUp -> Thread {
                val r = withCard({ link -> client.loadTopUp(fx.topupId, link) }, { TopUpReply.Failed(TillError.Unreachable) }) // never offline
                runOnUiThread { dispatch(PosEvent.TopUpLoadDone(fx.loadId, r)) }
            }.start()
        }
    }

    /** Counts the shown balance down, then hands the screen back to the amount. */
    private fun startBalanceCountdown(readId: Int) {
        var left = (PosFlow.BALANCE_SHOWN_MS / 1000).toInt()
        ui.setBalanceCountdown(left)
        val step = object : Runnable {
            override fun run() {
                val s = flow.state
                if (s !is PosState.BalanceShown || s.readId != readId) return
                if (--left <= 0) dispatch(PosEvent.BalanceExpired(readId))
                else { ui.setBalanceCountdown(left); handler.postDelayed(this, 1000) }
            }
        }
        handler.postDelayed(step, 1000)
    }

    /**
     * While [t]'s invoice is on screen: the time left every second, and the till asked every
     * 1.5 s whether it is paid. Both stop when another screen (or another invoice) takes over.
     */
    private fun watchInvoice(t: TopUp) {
        val onScreen = { (flow.state as? PosState.TopUpInvoice)?.topUp?.topupId == t.topupId }
        ui.setTimeLeft(timeLeft(t.expiresAtMs, System.currentTimeMillis()))
        val clock = object : Runnable {
            override fun run() {
                if (!onScreen()) return
                ui.setTimeLeft(timeLeft(t.expiresAtMs, System.currentTimeMillis()))
                handler.postDelayed(this, 1000)
            }
        }
        handler.postDelayed(clock, 1000)
        val ask = object : Runnable {
            override fun run() {
                if (!onScreen()) return
                Thread {
                    val r = client.topUpStatus(t.topupId)
                    runOnUiThread {
                        if (!onScreen()) return@runOnUiThread
                        dispatch(PosEvent.TopUpPolled(t.topupId, r))
                        if (onScreen()) handler.postDelayed(this, INVOICE_POLL_MS)
                    }
                }.start()
            }
        }
        handler.postDelayed(ask, INVOICE_POLL_MS)
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
        if (!salesGen.isCurrent(gen) || flow.unresolvedHeld) return
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

    /**
     * Reads the card now on the phone. The flow only asks once a card is there, so the till's
     * relay call finds it. The till serialises this against charges: a read during a sale is
     * refused with "busy" rather than interleaved.
     */
    private fun readCardBalance(readId: Int) {
        Thread {
            val r = withCard({ link -> client.cardBalance(link) }, { BalanceResult.Failed(TillError.Unreachable) }) // never offline
            runOnUiThread { dispatch(PosEvent.BalanceDone(readId, r)) }
        }.start()
    }

    private fun pollNow() {
        if (flow.state is PosState.Signing || flow.state is PosState.TopUpLoading || pollInFlight) return
        pollInFlight = true
        val mark = offline.pollMark()
        // The till this poll asks. An answer that lands after another URL was set (tillChanged) is
        // that till's, not the one set now: dropped, failures included; the next poll asks again.
        val url = settings.baseUrl
        Thread {
            val r = client.status()
            runOnUiThread {
                pollInFlight = false
                if (url != settings.baseUrl) return@runOnUiThread
                if (r is TillResult.Ok) {
                    failures = 0
                    ui.setStatus(r.value)
                    // A paired phone brings its own card link on every job (withCard), so the
                    // till's probe of a fixed relay says nothing about it: on a StartOS till there
                    // is no such relay, and the probe always fails.
                    ui.setHealth(tillOk = true, relayOk = settings.pairing != null || r.value.cardRelayOk)
                    afterPoll(mark)
                } else if (++failures >= 2) ui.setHealth(tillOk = false, relayOk = false)
            }
        }.start()
    }

    private val afterPollQueued = AtomicBoolean(false)
    private val lastMark = AtomicLong(0)

    /**
     * The till answered a status poll (every 5 s, on reconnect, on foreground, after a charge): on
     * the core's thread, the charges synced before that poll stop counting as the phone's, the kit is
     * fetched when due, what waits is synced, and the home screen's counts are read again. At most
     * one is queued; it uses the newest poll's mark.
     */
    private fun afterPoll(mark: Long) {
        lastMark.set(mark)
        if (!afterPollQueued.compareAndSet(false, true)) return
        OfflineRuntime.post {
            afterPollQueued.set(false)
            offline.statusShown(lastMark.get())
            offline.refreshKit()
            offline.sync()
            val c = offline.counts()
            runOnUiThread { ui.setPhone(c) }
        }
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

    companion object {
        const val PORT = 9876
        const val SCAN_REQUEST = 7
        const val TAG = "Pos"
        const val INVOICE_POLL_MS = 1_500L
    }
}
