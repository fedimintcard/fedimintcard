package org.fedimintcard.relay

import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.net.ServerSocket
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

class TillLinkTest {
    private fun session(card: CardLink): Pair<LinkSession, MutableList<JSONObject>> {
        val sent = mutableListOf<JSONObject>()
        return LinkSession(card) { sent.add(JSONObject(it)) } to sent
    }

    @Test fun readyThenApduIsAnswered() {
        val (s, sent) = session { a -> assertArrayEquals(byteArrayOf(0, 0xA4.toByte(), 4, 0), a); byteArrayOf(0x90.toByte(), 0) }
        s.onText("""{"type":"ready","link":"0123456789abcdef"}""")
        assertEquals("0123456789abcdef", s.awaitReady(10))
        s.onText("""{"type":"apdu","id":1,"apdu":"00a40400"}""")
        assertEquals(1, sent.size)
        assertEquals("rapdu", sent[0].getString("type")); assertEquals(1L, sent[0].getLong("id")); assertEquals("9000", sent[0].getString("rapdu"))
    }

    @Test fun aCardThatFailsSaysLostAndStaysLost() {
        var calls = 0
        val (s, sent) = session { calls++; throw IOException("tag was lost") }
        s.onText("""{"type":"apdu","id":4,"apdu":"b0110000"}""")
        s.onText("""{"type":"apdu","id":5,"apdu":"b0110100"}""")
        assertEquals(1, calls) // the card is not asked again once it is gone
        assertEquals(listOf("lost" to 4L, "lost" to 5L), sent.map { it.getString("type") to it.getLong("id") })
    }

    @Test fun aShortAnswerIsLost() {
        val (s, sent) = session { byteArrayOf(0x90.toByte()) }
        s.onText("""{"type":"apdu","id":2,"apdu":"b011"}""")
        assertEquals("lost", sent.single().getString("type"))
    }

    @Test fun tagDroppedWhileIdleTellsTheTillOnce() {
        val (s, sent) = session { byteArrayOf(0x90.toByte(), 0) }
        s.onText("""{"type":"apdu","id":3,"apdu":"b0110000"}""")
        s.cardLost(); s.cardLost()
        assertEquals(listOf("rapdu" to 3L, "lost" to 3L), sent.map { it.getString("type") to it.getLong("id") })
    }

    @Test fun malformedFramesAreIgnored() {
        val (s, sent) = session { fail("card asked"); ByteArray(0) }
        for (t in listOf("nope", """{"type":"apdu","id":1,"apdu":"zz"}""", """{"type":"apdu","apdu":"00"}""", """{"type":"other"}""")) s.onText(t)
        assertTrue(sent.isEmpty())
        assertNull(s.awaitReady(1))
    }

    @Test fun framesRoundTripMaskedAndLong() {
        for (n in listOf(5, 200, 70_000)) {
            val payload = ByteArray(n) { (it * 7).toByte() }
            val out = ByteArrayOutputStream()
            Ws.writeFrame(out, Ws.TEXT, payload, byteArrayOf(1, 2, 3, 4))
            val f = Ws.readMessage(ByteArrayInputStream(out.toByteArray()))
            assertEquals(Ws.TEXT, f.opcode); assertArrayEquals(payload, f.payload)
        }
        // Fragmented: text "ab" + continuation "cd".
        val raw = byteArrayOf(0x01, 2, 'a'.code.toByte(), 'b'.code.toByte(), 0x80.toByte(), 2, 'c'.code.toByte(), 'd'.code.toByte())
        assertEquals("abcd", String(Ws.readMessage(ByteArrayInputStream(raw)).payload))
        // RFC 6455 § 1.3's example key.
        assertEquals("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=", Ws.accept("dGhlIHNhbXBsZSBub25jZQ=="))
    }

    /** A till that serves one link: handshake, ready, one APDU, then close. */
    @Test fun aLinkAgainstAFakeTill() {
        val server = ServerSocket(0)
        val got = LinkedBlockingQueue<Any>()
        val till = Thread {
            server.accept().use { s ->
                val input = s.getInputStream(); val out = s.getOutputStream()
                val head = StringBuilder()
                while (!head.endsWith("\r\n\r\n")) head.append(input.read().toChar())
                got.add(head.toString())
                val key = head.lines().first { it.startsWith("Sec-WebSocket-Key:") }.substringAfter(':').trim()
                out.write("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${Ws.accept(key)}\r\n\r\n".toByteArray())
                Ws.writeFrame(out, Ws.TEXT, """{"type":"ready","link":"00aa11bb22cc33dd"}""".toByteArray(), null)
                Ws.writeFrame(out, Ws.TEXT, """{"type":"apdu","id":1,"apdu":"00ca","x":0}""".toByteArray(), null)
                got.add(String(Ws.readMessage(input).payload)) // the phone's answer (masked)
                Ws.writeFrame(out, Ws.CLOSE, ByteArray(0), null)
                got.add(Ws.readMessage(input).opcode)
            }
        }.apply { start() }
        val link = TillLink.open("http://127.0.0.1:${server.localPort}", "f".repeat(64), CardLink { byteArrayOf(1, 0x90.toByte(), 0) })
        assertEquals("00aa11bb22cc33dd", link.id)
        val head = got.poll(5, TimeUnit.SECONDS) as String
        assertTrue(head, head.startsWith("GET /v1/card/link HTTP/1.1\r\n"))
        assertTrue(head.contains("Authorization: Bearer ${"f".repeat(64)}\r\n"))
        val answer = JSONObject(got.poll(5, TimeUnit.SECONDS) as String)
        assertEquals("rapdu", answer.getString("type")); assertEquals("019000", answer.getString("rapdu"))
        assertEquals(Ws.CLOSE, got.poll(5, TimeUnit.SECONDS))
        till.join(5000)
        // The till closed it: the reader thread closes the phone's side too.
        val deadline = System.currentTimeMillis() + 5000
        while (!link.closed && System.currentTimeMillis() < deadline) Thread.sleep(20)
        assertTrue(link.closed)
        server.close()
    }

    @Test fun aRefusedLinkThrows() {
        val server = ServerSocket(0)
        Thread {
            server.accept().use { s ->
                val input = s.getInputStream()
                val head = StringBuilder()
                while (!head.endsWith("\r\n\r\n")) head.append(input.read().toChar())
                s.getOutputStream().write("HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n".toByteArray())
            }
        }.start()
        try {
            TillLink.open("http://127.0.0.1:${server.localPort}", null, CardLink { ByteArray(0) })
            fail("opened")
        } catch (e: IOException) {
            assertTrue(e.message!!.contains("401"))
        }
        server.close()
    }

    @Test fun pairingText() {
        val t = "ab".repeat(32)
        assertEquals(Pairing("http://192.168.1.20:9877", t), parsePairing("fedimintcard-till:http://192.168.1.20:9877?t=$t"))
        assertEquals(Pairing("http://10.0.0.2:9877", t), parsePairing("  fedimintcard-till:http://10.0.0.2:9877/?t=${t.uppercase()}\n"))
        for (bad in listOf(
            "", "http://10.0.0.2:9877?t=$t", "fedimintcard-till:http://10.0.0.2:9877", "fedimintcard-till:http://10.0.0.2:9877?t=abc",
            "fedimintcard-till:http://10.0.0.2:9877?t=${"zz".repeat(32)}", "fedimintcard-till:10.0.0.2:9877?t=$t", "fedimintcard-till:ftp://h?t=$t",
        )) assertNull(bad, parsePairing(bad))
        assertFalse(Pairing("u", t).toString().contains(t))
    }

    @Test fun clientSendsBearerAndLink() {
        var seen: Map<String, String> = emptyMap()
        val t = object : Transport {
            override fun send(method: String, url: String, body: String?, connectMs: Int, readMs: Int) = error("headers path expected")
            override fun send(method: String, url: String, body: String?, connectMs: Int, readMs: Int, headers: Map<String, String>): RawResponse {
                seen = headers; return RawResponse(409, """{"ok":false,"error":"card_link_gone"}""")
            }
        }
        val r = TillClient({ "http://h:1" }, t, { "tok" }).cardBalance("00aa11bb22cc33dd") as BalanceResult.Refused
        assertEquals(mapOf("Authorization" to "Bearer tok", "X-Card-Link" to "00aa11bb22cc33dd"), seen)
        assertEquals(ChargeErrorCode.NO_CARD, r.code)
        TillClient({ "http://h:1" }, t).status()
        assertTrue(seen.isEmpty())
    }
}
