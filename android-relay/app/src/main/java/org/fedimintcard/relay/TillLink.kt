package org.fedimintcard.relay

import android.util.Log
import org.json.JSONException
import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.io.Closeable
import java.io.EOFException
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.net.InetSocketAddress
import java.net.Socket
import java.net.URI
import java.security.MessageDigest
import java.security.SecureRandom
import java.util.Base64
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference
import javax.net.ssl.SNIHostName
import javax.net.ssl.SSLSocket
import javax.net.ssl.SSLSocketFactory

/**
 * Where the card link goes for a till at [baseUrl]: `http://` → `ws://` (plain TCP), `https://` →
 * `wss://` (TLS, the certificate checked against [host]), as for a till behind a proxy that
 * terminates TLS (a StartOS package). [hostHeader] carries the port only when it is not the
 * scheme's default, as a browser would send it.
 */
internal data class LinkTarget(val host: String, val port: Int, val tls: Boolean, val path: String, val hostHeader: String)

@Throws(IOException::class)
internal fun linkTarget(baseUrl: String): LinkTarget {
    val u = try { URI(baseUrl.trimEnd('/') + "/v1/card/link") } catch (e: Exception) { throw IOException("bad till URL", e) }
    val tls = when (u.scheme) {
        "https" -> true
        "http" -> false
        else -> throw IOException("the till URL must be http:// or https://")
    }
    val host = u.host?.takeIf { it.isNotEmpty() } ?: throw IOException("the till URL has no host")
    val default = if (tls) 443 else 80
    val port = if (u.port > 0) u.port else default
    val hostHeader = if (port == default) host else "$host:$port"
    return LinkTarget(host.removePrefix("[").removeSuffix("]"), port, tls, u.rawPath, hostHeader)
}

/**
 * The card link (docs/POS-LINK.md): while a tap is in progress this phone holds a WebSocket open
 * to the till, and the till sends the card's APDUs down it. The phone answers each from the card
 * it holds, says `lost` if the card leaves first, and the till closes the link after the job.
 *
 * APDU bytes are never logged, only INS and SW.
 */
class LinkSession(private val card: CardLink, private val send: (String) -> Unit) {
    /** The link id from the till's `ready` frame, once it has come. */
    @Volatile var linkId: String? = null
        private set
    private val ready = CountDownLatch(1)
    private val lock = Any()
    private var lost = false
    private var inFlight = false
    private var lastId = 0L

    fun awaitReady(ms: Long): String? = if (ready.await(ms, TimeUnit.MILLISECONDS)) linkId else null

    /** One text frame from the till. */
    fun onText(text: String) {
        val o = try { JSONObject(text) } catch (e: JSONException) { return }
        when (o.optString("type")) {
            "ready" -> {
                linkId = o.optString("link").takeIf { it.isNotEmpty() }
                ready.countDown()
            }
            "apdu" -> {
                if (!o.has("id")) return
                val id = o.optLong("id")
                val apdu = hexToBytes(o.optString("apdu")) ?: return
                answer(id, apdu)
            }
        }
    }

    private fun answer(id: Long, apdu: ByteArray) {
        synchronized(lock) {
            lastId = id
            if (lost) { send(lostFrame(id)); return }
            inFlight = true
        }
        val ins = if (apdu.size > 1) "%02x".format(apdu[1].toInt() and 0xFF) else "--"
        val r = try {
            card.transceive(apdu).also { if (it.size < 2) throw IOException("short response") }
        } catch (e: Exception) {
            logI("INS $ins: card lost (${e.javaClass.simpleName})")
            synchronized(lock) { lost = true; inFlight = false }
            send(lostFrame(id))
            return
        }
        logI("INS $ins SW ${bytesToHex(r.copyOfRange(r.size - 2, r.size))}")
        synchronized(lock) { inFlight = false }
        send(JSONObject().put("type", "rapdu").put("id", id).put("rapdu", bytesToHex(r)).toString())
    }

    /**
     * The tag dropped. An APDU in flight will fail and say `lost` itself; with none in flight the
     * till is told now, so it ends the link instead of waiting on a card that is gone.
     */
    fun cardLost() {
        val idle = synchronized(lock) {
            if (lost) return
            lost = true
            !inFlight
        }
        if (idle) send(lostFrame(lastId))
    }

    private fun lostFrame(id: Long) = JSONObject().put("type", "lost").put("id", id).toString()

    companion object { const val TAG = "TillLink" }
}

/** Log.i, quiet in JVM unit tests (where android.util.Log is a stub that throws). */
private fun logI(msg: String) {
    try { Log.i(LinkSession.TAG, msg) } catch (_: RuntimeException) {}
}

/** Just enough RFC 6455 for a client: masked frames out, text/close/ping in. */
object Ws {
    const val TEXT = 1
    const val CLOSE = 8
    const val PING = 9
    const val PONG = 10
    private const val GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
    private const val MAX_FRAME = 1 shl 20

    class Frame(val opcode: Int, val payload: ByteArray)

    fun accept(key: String): String =
        Base64.getEncoder().encodeToString(MessageDigest.getInstance("SHA-1").digest((key + GUID).toByteArray(Charsets.ISO_8859_1)))

    fun writeFrame(out: OutputStream, opcode: Int, payload: ByteArray, mask: ByteArray?) {
        val b = ByteArrayOutputStream()
        b.write(0x80 or opcode)
        val m = if (mask != null) 0x80 else 0
        when {
            payload.size < 126 -> b.write(m or payload.size)
            payload.size < 65536 -> { b.write(m or 126); b.write(payload.size shr 8); b.write(payload.size and 0xFF) }
            else -> { b.write(m or 127); for (i in 7 downTo 0) b.write(((payload.size.toLong() shr (8 * i)) and 0xFF).toInt()) }
        }
        if (mask != null) {
            b.write(mask)
            b.write(ByteArray(payload.size) { (payload[it].toInt() xor mask[it % 4].toInt()).toByte() })
        } else b.write(payload)
        out.write(b.toByteArray())
        out.flush()
    }

    private fun readFully(input: InputStream, n: Int): ByteArray {
        val buf = ByteArray(n)
        var off = 0
        while (off < n) {
            val r = input.read(buf, off, n - off)
            if (r < 0) throw EOFException("socket closed")
            off += r
        }
        return buf
    }

    /** One message: continuation frames are joined; control frames come back on their own. */
    fun readMessage(input: InputStream): Frame {
        var opcode = -1
        val data = ByteArrayOutputStream()
        while (true) {
            val h = readFully(input, 2)
            val fin = (h[0].toInt() and 0x80) != 0
            val op = h[0].toInt() and 0x0F
            val masked = (h[1].toInt() and 0x80) != 0
            var len = (h[1].toInt() and 0x7F).toLong()
            if (len == 126L) { val e = readFully(input, 2); len = ((e[0].toInt() and 0xFF) shl 8 or (e[1].toInt() and 0xFF)).toLong() }
            else if (len == 127L) { val e = readFully(input, 8); len = e.fold(0L) { a, x -> (a shl 8) or (x.toLong() and 0xFF) } }
            if (len > MAX_FRAME) throw IOException("frame too large")
            val mask = if (masked) readFully(input, 4) else null
            val p = readFully(input, len.toInt())
            if (mask != null) for (i in p.indices) p[i] = (p[i].toInt() xor mask[i % 4].toInt()).toByte()
            if (op >= 8) return Frame(op, p) // control frames are never fragmented
            if (op != 0) opcode = op
            data.write(p)
            if (fin) return Frame(opcode, data.toByteArray())
        }
    }
}

/**
 * One link to the till over a WebSocket. [open] blocks until the till's `ready`; a reader thread
 * then answers APDUs from [card] until the till (or [close]) ends the link.
 */
class TillLink private constructor(private val socket: Socket, val id: String, private val session: LinkSession, private val writer: (Int, ByteArray) -> Unit) : Closeable {
    @Volatile var closed = false
        private set

    /** The tag left the field. */
    fun cardLost() { if (!closed) session.cardLost() }

    override fun close() {
        if (closed) return
        closed = true
        try { writer(Ws.CLOSE, ByteArray(0)) } catch (_: Exception) {}
        try { socket.close() } catch (_: Exception) {}
    }

    companion object {
        private const val CONNECT_MS = 3_000
        /** The till ends a link after 120 s; a little margin before the read gives up. */
        private const val READ_MS = 130_000

        /** Open a link to the till at [baseUrl]. Throws IOException if it cannot (incl. 401). */
        @Throws(IOException::class)
        fun open(baseUrl: String, token: String?, card: CardLink, connectMs: Int = CONNECT_MS): TillLink {
            val t = linkTarget(baseUrl)
            val s = if (t.tls) SSLSocketFactory.getDefault().createSocket() else Socket()
            try {
                s.connect(InetSocketAddress(t.host, t.port), connectMs)
                if (s is SSLSocket) {
                    // A bare SSLSocket checks the chain but not the name: ask for the HTTPS name
                    // check (and send SNI, which a proxy routes on) before any byte is written.
                    val p = s.sslParameters
                    p.endpointIdentificationAlgorithm = "HTTPS"
                    if (!isIpLiteral(t.host)) p.serverNames = listOf(SNIHostName(t.host))
                    s.sslParameters = p
                    s.soTimeout = connectMs
                    s.startHandshake()
                }
                s.soTimeout = READ_MS
                s.tcpNoDelay = true
                val out = s.getOutputStream()
                val input = s.getInputStream().buffered()
                val key = Base64.getEncoder().encodeToString(ByteArray(16).also { SecureRandom().nextBytes(it) })
                val req = StringBuilder()
                    .append("GET ${t.path} HTTP/1.1\r\n")
                    .append("Host: ${t.hostHeader}\r\n")
                    .append("Upgrade: websocket\r\nConnection: Upgrade\r\n")
                    .append("Sec-WebSocket-Key: $key\r\nSec-WebSocket-Version: 13\r\n")
                if (token != null) req.append("Authorization: Bearer $token\r\n")
                req.append("\r\n")
                out.write(req.toString().toByteArray(Charsets.ISO_8859_1)); out.flush()
                val head = readHead(input)
                val status = head.firstOrNull()?.split(" ")?.getOrNull(1)?.toIntOrNull()
                if (status != 101) throw IOException("till refused the card link: HTTP $status")
                val accept = head.drop(1).firstOrNull { it.lowercase().startsWith("sec-websocket-accept:") }?.substringAfter(':')?.trim()
                if (accept != Ws.accept(key)) throw IOException("bad WebSocket handshake")
                val rng = SecureRandom()
                val writeLock = Any()
                val writer: (Int, ByteArray) -> Unit = { op, p ->
                    synchronized(writeLock) { Ws.writeFrame(out, op, p, ByteArray(4).also { rng.nextBytes(it) }) }
                }
                val session = LinkSession(card) { text -> try { writer(Ws.TEXT, text.toByteArray(Charsets.UTF_8)) } catch (_: IOException) {} }
                val link = AtomicReference<TillLink?>(null)
                val reader = Thread({
                    try {
                        while (true) {
                            val f = Ws.readMessage(input)
                            when (f.opcode) {
                                Ws.TEXT -> session.onText(String(f.payload, Charsets.UTF_8))
                                Ws.PING -> writer(Ws.PONG, f.payload)
                                Ws.CLOSE -> break
                            }
                        }
                    } catch (_: Exception) {
                    } finally {
                        // The till ended the link (or the socket failed): answer its close and close
                        // this side as close() does, even before open() has handed the link out.
                        try { writer(Ws.CLOSE, ByteArray(0)) } catch (_: Exception) {}
                        try { s.close() } catch (_: Exception) {}
                        link.get()?.closed = true
                    }
                }, "till-link")
                reader.isDaemon = true
                reader.start()
                val id = session.awaitReady(connectMs.toLong() + 2_000) ?: run {
                    try { s.close() } catch (_: Exception) {}
                    throw IOException("no ready from the till")
                }
                return TillLink(s, id, session, writer).also { link.set(it); if (s.isClosed) it.closed = true }
            } catch (e: Exception) {
                try { s.close() } catch (_: Exception) {}
                throw e as? IOException ?: IOException(e.message, e)
            }
        }

        private fun isIpLiteral(host: String) =
            host.contains(':') || host.all { it.isDigit() || it == '.' }

        private fun readHead(input: InputStream): List<String> {
            val head = ByteArrayOutputStream()
            var tail = 0
            while (true) {
                val b = input.read()
                if (b < 0) throw EOFException("till closed during the handshake")
                head.write(b)
                tail = ((tail shl 8) or b)
                if (tail == 0x0D0A0D0A) break
                if (head.size() > 8192) throw IOException("handshake headers too large")
            }
            return head.toString(Charsets.ISO_8859_1.name()).split("\r\n").filter { it.isNotEmpty() }
        }
    }
}
