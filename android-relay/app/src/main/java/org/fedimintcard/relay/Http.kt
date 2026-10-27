package org.fedimintcard.relay

import java.io.ByteArrayOutputStream
import java.io.IOException
import java.io.InputStream

const val MAX_BODY = 4096
private const val MAX_HEADER_BYTES = 8192

class HttpRequest(val method: String, val path: String, val headers: Map<String, String>, val body: ByteArray)

class HttpException(val status: Int, message: String) : IOException(message)

/** Minimal HTTP/1.1 request parser: request line, headers, Content-Length body capped at [MAX_BODY]. */
fun parseRequest(input: InputStream): HttpRequest {
    val head = ByteArrayOutputStream()
    var tail = 0
    while (true) {
        val b = input.read()
        if (b < 0) throw HttpException(400, "truncated request")
        head.write(b)
        tail = ((tail shl 8) or b) and 0xFFFFFFFF.toInt()
        if (tail == 0x0D0A0D0A) break
        if (head.size() > MAX_HEADER_BYTES) throw HttpException(431, "headers too large")
    }
    val lines = head.toString(Charsets.ISO_8859_1.name()).split("\r\n").filter { it.isNotEmpty() }
    val parts = lines.firstOrNull()?.split(" ") ?: throw HttpException(400, "empty request")
    if (parts.size != 3 || !parts[2].startsWith("HTTP/")) throw HttpException(400, "bad request line")
    val headers = HashMap<String, String>()
    for (l in lines.drop(1)) {
        val i = l.indexOf(':')
        if (i <= 0) throw HttpException(400, "bad header")
        headers[l.substring(0, i).trim().lowercase()] = l.substring(i + 1).trim()
    }
    val len = headers["content-length"]?.let { it.toIntOrNull() ?: throw HttpException(400, "bad content-length") } ?: 0
    if (len < 0) throw HttpException(400, "bad content-length")
    if (len > MAX_BODY) throw HttpException(413, "body too large")
    val body = ByteArray(len)
    var off = 0
    while (off < len) {
        val n = input.read(body, off, len - off)
        if (n < 0) throw HttpException(400, "truncated body")
        off += n
    }
    return HttpRequest(parts[0], parts[1].substringBefore('?'), headers, body)
}

private val REASONS = mapOf(
    200 to "OK", 400 to "Bad Request", 404 to "Not Found", 405 to "Method Not Allowed",
    413 to "Payload Too Large", 431 to "Request Header Fields Too Large", 503 to "Service Unavailable",
)

fun httpResponse(status: Int, json: String): ByteArray {
    val body = json.toByteArray(Charsets.UTF_8)
    val head = "HTTP/1.1 $status ${REASONS[status] ?: "Error"}\r\n" +
        "Content-Type: application/json\r\nContent-Length: ${body.size}\r\nConnection: close\r\n\r\n"
    return head.toByteArray(Charsets.ISO_8859_1) + body
}

// --- JSON (just enough for {"apdu":"<hex>"}) ---

fun jsonEscape(s: String): String {
    val sb = StringBuilder()
    for (c in s) when {
        c == '"' -> sb.append("\\\"")
        c == '\\' -> sb.append("\\\\")
        c == '\n' -> sb.append("\\n")
        c == '\r' -> sb.append("\\r")
        c == '\t' -> sb.append("\\t")
        c < ' ' -> sb.append(String.format("\\u%04x", c.code))
        else -> sb.append(c)
    }
    return sb.toString()
}

fun errorJson(msg: String) = "{\"error\":\"${jsonEscape(msg)}\"}"

/** Extracts the string value of "apdu" from a flat JSON object, or null if missing or malformed. */
fun extractApduHex(json: String): String? {
    val m = Regex("\"apdu\"\\s*:\\s*\"([^\"\\\\]*)\"").find(json) ?: return null
    return m.groupValues[1]
}

fun hexToBytes(s: String): ByteArray? {
    if (s.length % 2 != 0) return null
    val out = ByteArray(s.length / 2)
    for (i in out.indices) {
        val hi = Character.digit(s[2 * i], 16)
        val lo = Character.digit(s[2 * i + 1], 16)
        if (hi < 0 || lo < 0) return null
        out[i] = ((hi shl 4) or lo).toByte()
    }
    return out
}

fun bytesToHex(b: ByteArray): String {
    val sb = StringBuilder(b.size * 2)
    for (x in b) sb.append(String.format("%02x", x.toInt() and 0xFF))
    return sb.toString()
}
