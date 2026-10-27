package org.fedimintcard.relay

import org.junit.Assert.*
import org.junit.Test

class HttpTest {
    private fun parse(s: String) = parseRequest(s.toByteArray(Charsets.ISO_8859_1).inputStream())

    @Test fun parsesPostWithBody() {
        val body = "{\"apdu\":\"b01000000\"}"
        val r = parse("POST /apdu HTTP/1.1\r\nHost: x\r\nContent-Length: ${body.length}\r\n\r\n$body")
        assertEquals("POST", r.method)
        assertEquals("/apdu", r.path)
        assertEquals(body, String(r.body))
    }

    @Test fun parsesGetWithoutBodyAndStripsQuery() {
        val r = parse("GET /health?x=1 HTTP/1.1\r\n\r\n")
        assertEquals("/health", r.path)
        assertEquals(0, r.body.size)
    }

    @Test fun headerNamesCaseInsensitive() {
        val r = parse("POST /reset HTTP/1.1\r\ncontent-length: 2\r\n\r\n{}")
        assertEquals(2, r.body.size)
    }

    @Test fun rejectsOversizeBody() {
        val e = assertThrows(HttpException::class.java) { parse("POST /apdu HTTP/1.1\r\nContent-Length: 4097\r\n\r\n") }
        assertEquals(413, e.status)
    }

    @Test fun rejectsBadRequestLine() {
        assertEquals(400, assertThrows(HttpException::class.java) { parse("GARBAGE\r\n\r\n") }.status)
    }

    @Test fun rejectsTruncatedBody() {
        assertEquals(400, assertThrows(HttpException::class.java) { parse("POST /a HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc") }.status)
    }

    @Test fun rejectsBadContentLength() {
        assertEquals(400, assertThrows(HttpException::class.java) { parse("POST /a HTTP/1.1\r\nContent-Length: abc\r\n\r\n") }.status)
    }

    @Test fun extractsApdu() {
        assertEquals("b010000000", extractApduHex("{ \"apdu\" : \"b010000000\" }"))
        assertNull(extractApduHex("{\"x\":1}"))
        assertNull(extractApduHex("not json"))
    }

    @Test fun hexRoundTrip() {
        val b = hexToBytes("00ff10Ab")!!
        assertEquals("00ff10ab", bytesToHex(b))
        assertNull(hexToBytes("abc"))
        assertNull(hexToBytes("zz"))
    }

    @Test fun jsonEscapesErrors() {
        assertEquals("{\"error\":\"a\\\"b\\\\c\\n\"}", errorJson("a\"b\\c\n"))
    }

    @Test fun responseFormat() {
        val s = String(httpResponse(503, errorJson("no card")), Charsets.ISO_8859_1)
        assertTrue(s.startsWith("HTTP/1.1 503 Service Unavailable\r\n"))
        assertTrue(s.contains("Content-Length: 19\r\n"))
        assertTrue(s.endsWith("\r\n\r\n{\"error\":\"no card\"}"))
    }
}
