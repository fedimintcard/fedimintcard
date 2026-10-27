package org.fedimintcard.relay

import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import java.net.InetAddress
import java.net.ServerSocket
import java.net.SocketTimeoutException

/** Tiny socket server (com.sun.net.httpserver is hidden by the Android compile classpath). */
class UrlConnectionTransportTest {
    private lateinit var ss: ServerSocket
    private val base get() = "http://127.0.0.1:${ss.localPort}"

    @Before fun up() {
        ss = ServerSocket(0, 5, InetAddress.getByName("127.0.0.1"))
        Thread {
            while (!ss.isClosed) {
                val s = try { ss.accept() } catch (e: Exception) { return@Thread }
                Thread { handle(s) }.start()
            }
        }.also { it.isDaemon = true }.start()
    }
    @After fun down() = ss.close()

    private fun handle(s: java.net.Socket) = s.use {
        val inp = it.getInputStream()
        val req = parseRequest(inp)
        val (code, body) = when (req.path) {
            "/echo" -> 200 to String(req.body)
            "/e409" -> 409 to """{"ok":false,"error":"x409"}"""
            "/e500" -> 500 to """{"ok":false,"error":"x500"}"""
            else -> { Thread.sleep(1500); 200 to "" }
        }
        val b = body.toByteArray()
        it.getOutputStream().apply {
            write("HTTP/1.1 $code X\r\nContent-Length: ${b.size}\r\nConnection: close\r\n\r\n".toByteArray()); write(b); flush()
        }
    }

    @Test fun postBodyRoundTrips() {
        val r = UrlConnectionTransport().send("POST", "$base/echo", """{"a":1}""", 1000, 2000)
        assertEquals(200, r.status); assertEquals("""{"a":1}""", r.body)
    }
    @Test fun errorBodiesAreRead() {
        for (code in listOf(409, 500)) {
            val r = UrlConnectionTransport().send("GET", "$base/e$code", null, 1000, 2000)
            assertEquals(code, r.status); assertTrue(r.body.contains("x$code"))
        }
    }
    @Test fun readTimeout() {
        assertThrows(SocketTimeoutException::class.java) { UrlConnectionTransport().send("GET", "$base/slow", null, 1000, 300) }
    }
}
