package org.fedimintcard.relay

import android.app.Activity
import android.content.Context
import android.net.wifi.WifiManager
import android.nfc.NfcAdapter
import android.nfc.Tag
import android.nfc.TagLostException
import android.nfc.tech.IsoDep
import android.os.Bundle
import android.view.Gravity
import android.view.WindowManager
import android.widget.LinearLayout
import android.widget.TextView
import java.util.concurrent.atomic.AtomicReference

class MainActivity : Activity(), NfcAdapter.ReaderCallback {
    private val isoDep = AtomicReference<IsoDep?>(null)
    private var server: ApduServer? = null
    private lateinit var status: TextView
    private lateinit var last: TextView
    private lateinit var info: TextView
    private var nfc: NfcAdapter? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        status = TextView(this).apply { textSize = 40f; gravity = Gravity.CENTER; text = "no card" }
        last = TextView(this).apply { textSize = 18f; gravity = Gravity.CENTER; text = "" }
        info = TextView(this).apply { textSize = 16f; gravity = Gravity.CENTER }
        setContentView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER
            setPadding(32, 32, 32, 32)
            addView(status); addView(last); addView(info)
        })
        nfc = NfcAdapter.getDefaultAdapter(this)
        if (nfc == null) status.text = "no NFC"
        else if (nfc?.isEnabled == false) last.text = "NFC is switched off"
        try {
            server = ApduServer(PORT, { isoDep.get()?.takeIf { it.isConnected }?.let { d -> CardLink { a -> try { d.transceive(a) } catch (e: TagLostException) { isoDep.compareAndSet(d, null); runOnUiThread { status.text = "no card" }; throw e } } } }) { ev ->
                runOnUiThread { last.text = ev }
            }.also { it.start() }
        } catch (e: Exception) {
            last.text = "server failed: ${e.message}"
        }
        info.text = "port $PORT\nWi-Fi IP: ${wifiIp()}"
    }

    override fun onResume() {
        super.onResume()
        nfc?.enableReaderMode(
            this, this,
            NfcAdapter.FLAG_READER_NFC_A or NfcAdapter.FLAG_READER_NFC_B or
                NfcAdapter.FLAG_READER_SKIP_NDEF_CHECK or NfcAdapter.FLAG_READER_NO_PLATFORM_SOUNDS,
            null,
        )
    }

    override fun onPause() {
        nfc?.disableReaderMode(this)
        super.onPause()
    }

    override fun onDestroy() {
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
            runOnUiThread { status.text = "card present" }
            // Poll for removal so the status returns to "no card".
            Thread {
                while (isoDep.get() === d) {
                    try {
                        if (!d.isConnected) throw TagLostException()
                        Thread.sleep(300)
                    } catch (e: Exception) {
                        isoDep.compareAndSet(d, null)
                        runOnUiThread { status.text = "no card" }
                    }
                }
            }.start()
        } catch (e: TagLostException) {
            isoDep.compareAndSet(d, null)
            runOnUiThread { status.text = "no card" }
        } catch (e: Exception) {
            runOnUiThread { last.text = "connect failed: ${e.message}" }
        }
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
