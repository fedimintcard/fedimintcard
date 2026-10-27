package org.fedimintcard.relay

import android.util.Log
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket

/** Transport to the card; returns data||SW. Throws on failure. */
fun interface CardLink {
    fun transceive(apdu: ByteArray): ByteArray
}

class ApduServer(
    private val port: Int,
    /** Returns the current card link, or null if no card is present. */
    private val card: () -> CardLink?,
    private val onEvent: (String) -> Unit,
) {
    private val lock = Any()
    @Volatile private var server: ServerSocket? = null
    private var thread: Thread? = null

    fun start() {
        val ss = ServerSocket()
        ss.reuseAddress = true
        ss.bind(InetSocketAddress(port)) // all interfaces
        server = ss
        thread = Thread({
            while (!ss.isClosed) {
                val s = try { ss.accept() } catch (e: Exception) { break }
                Thread { handle(s) }.start()
            }
        }, "apdu-server").also { it.isDaemon = true; it.start() }
    }

    fun stop() {
        try { server?.close() } catch (_: Exception) {}
    }

    private fun handle(s: Socket) {
        s.use {
            try {
                it.soTimeout = 10_000
                val out = try {
                    route(parseRequest(it.getInputStream()))
                } catch (e: HttpException) {
                    httpResponse(e.status, errorJson(e.message ?: "bad request"))
                }
                it.getOutputStream().apply { write(out); flush() }
            } catch (e: Exception) {
                Log.w(TAG, "connection error: ${e.javaClass.simpleName}")
            }
        }
    }

    internal fun route(req: HttpRequest): ByteArray = when (req.path) {
        "/health" -> if (req.method == "GET") httpResponse(200, "{\"ok\":true}") else httpResponse(405, errorJson("method not allowed"))
        "/reset" -> if (req.method == "POST") httpResponse(200, "{\"ok\":true}") else httpResponse(405, errorJson("method not allowed"))
        "/apdu" -> if (req.method == "POST") apdu(req) else httpResponse(405, errorJson("method not allowed"))
        else -> httpResponse(404, errorJson("not found"))
    }

    private fun apdu(req: HttpRequest): ByteArray {
        val hex = extractApduHex(req.body.toString(Charsets.UTF_8)) ?: return httpResponse(400, errorJson("expected {\"apdu\":\"<hex>\"}"))
        val apdu = hexToBytes(hex) ?: return httpResponse(400, errorJson("apdu is not valid hex"))
        if (apdu.size < 4) return httpResponse(400, errorJson("apdu shorter than 4 bytes"))
        val ins = "%02x".format(apdu[1].toInt() and 0xFF)
        synchronized(lock) {
            val link = card() ?: run {
                onEvent("INS $ins: no card")
                return httpResponse(503, errorJson("no card"))
            }
            return try {
                val resp = link.transceive(apdu)
                if (resp.size < 2) throw java.io.IOException("short response")
                val sw = bytesToHex(resp.copyOfRange(resp.size - 2, resp.size))
                Log.i(TAG, "INS $ins SW $sw")
                onEvent("INS $ins -> SW $sw")
                httpResponse(200, "{\"response\":\"${bytesToHex(resp)}\"}")
            } catch (e: Exception) {
                Log.w(TAG, "INS $ins failed: ${e.javaClass.simpleName}")
                onEvent("INS $ins failed")
                httpResponse(503, errorJson(e.message ?: e.javaClass.simpleName))
            }
        }
    }

    companion object { const val TAG = "ApduServer" }
}
