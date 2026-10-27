package org.fedimintcard.relay

import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject
import java.net.ConnectException
import java.io.IOException
import java.net.HttpURLConnection
import java.net.SocketTimeoutException
import java.net.URL

data class RawResponse(val status: Int, val body: String)

fun interface Transport {
    /** Throws SocketTimeoutException on read timeout; any other IOException means unreachable. */
    @Throws(IOException::class)
    fun send(method: String, url: String, body: String?, connectMs: Int, readMs: Int): RawResponse
}

sealed class TillError {
    object Unreachable : TillError()
    object Timeout : TillError()
    data class BadResponse(val detail: String) : TillError()
}

sealed class TillResult<out T> {
    data class Ok<T>(val value: T) : TillResult<T>()
    data class Err(val error: TillError) : TillResult<Nothing>()
}

sealed class ChargeResult {
    data class Success(val receipt: ChargeReceipt) : ChargeResult()
    data class Declined(val code: ChargeErrorCode, val detail: String, val httpStatus: Int) : ChargeResult()
    data class Failed(val error: TillError) : ChargeResult()
}

class TillClient(private val baseUrl: () -> String, private val transport: Transport) {
    private fun url(path: String) = baseUrl().trimEnd('/') + path

    private fun call(method: String, path: String, body: String?, readMs: Int): TillResult<RawResponse> =
        try {
            TillResult.Ok(transport.send(method, url(path), body, CONNECT_MS, readMs))
        } catch (e: SocketTimeoutException) {
            TillResult.Err(TillError.Timeout)
        } catch (e: Exception) {
            TillResult.Err(TillError.Unreachable)
        }

    fun status(): TillResult<TillStatus> = when (val r = call("GET", "/v1/status", null, QUICK_READ_MS)) {
        is TillResult.Err -> r
        is TillResult.Ok -> parseStatus(r.value.body)
    }

    fun charge(priceMsat: Long, clientRef: String): ChargeResult {
        val body = JSONObject().put("price_msat", priceMsat).put("client_ref", clientRef).toString()
        return when (val r = call("POST", "/v1/charge", body, CHARGE_READ_MS)) {
            is TillResult.Err -> ChargeResult.Failed(r.error)
            is TillResult.Ok -> parseCharge(r.value.status, r.value.body)
        }
    }

    fun sales(): TillResult<SalesList> = when (val r = call("GET", "/v1/sales", null, QUICK_READ_MS)) {
        is TillResult.Err -> r
        is TillResult.Ok -> parseSales(r.value.body)
    }

    companion object {
        const val CONNECT_MS = 3_000
        const val QUICK_READ_MS = 10_000
        const val CHARGE_READ_MS = 125_000 // contract: at least 120 s
    }
}

class UrlConnectionTransport : Transport {
    override fun send(method: String, url: String, body: String?, connectMs: Int, readMs: Int): RawResponse {
        val c = URL(url).openConnection() as HttpURLConnection
        try {
            c.requestMethod = method
            c.connectTimeout = connectMs
            c.readTimeout = readMs
            c.useCaches = false
            c.setRequestProperty("Connection", "close")
            c.setRequestProperty("Accept", "application/json")
            if (body != null) {
                c.doOutput = true
                c.setRequestProperty("Content-Type", "application/json")
            }
            try {
                c.connect()
            } catch (e: SocketTimeoutException) {
                throw ConnectException("connect timed out") // unreachable, not "outcome unknown"
            }
            if (body != null) {
                c.outputStream.use { it.write(body.toByteArray(Charsets.UTF_8)) }
            }
            val status = c.responseCode
            val stream = if (status >= 400) c.errorStream else c.inputStream
            val text = stream?.use { String(it.readBytes(), Charsets.UTF_8) } ?: ""
            return RawResponse(status, text)
        } finally {
            c.disconnect()
        }
    }
}

// ---- pure parsing ----

private fun JSONObject.reqLong(k: String): Long {
    if (!has(k) || isNull(k)) throw JSONException("missing $k")
    return getLong(k)
}
private fun JSONObject.optL(k: String, d: Long = 0L): Long = if (has(k) && !isNull(k)) getLong(k) else d
private fun JSONObject.reqStr(k: String): String {
    if (!has(k) || isNull(k)) throw JSONException("missing $k")
    return getString(k)
}
private fun JSONObject.reqBool(k: String): Boolean {
    if (!has(k) || isNull(k)) throw JSONException("missing $k")
    return getBoolean(k)
}

private fun notes(a: JSONArray?): List<Note> =
    if (a == null) emptyList() else (0 until a.length()).map { val o = a.getJSONObject(it); Note(o.reqLong("slot").toInt(), o.reqLong("msat")) }

internal fun parseStatus(body: String): TillResult<TillStatus> = try {
    val o = JSONObject(body)
    TillResult.Ok(
        TillStatus(
            o.reqStr("federation_id"),
            if (o.isNull("federation_name")) null else o.optString("federation_name"),
            o.reqLong("wallet_balance_msat"),
            o.reqBool("card_relay_ok"),
            o.reqBool("busy"),
        )
    )
} catch (e: JSONException) {
    TillResult.Err(TillError.BadResponse(e.message ?: "bad json"))
}

internal fun parseCharge(httpStatus: Int, body: String): ChargeResult = try {
    val o = JSONObject(body)
    if (!o.has("ok")) throw JSONException("missing ok")
    if (!o.getBoolean("ok")) {
        ChargeResult.Declined(
            ChargeErrorCode.fromWire(o.optString("error", "")),
            if (o.isNull("detail")) "" else o.optString("detail", ""),
            httpStatus,
        )
    } else {
        val w = o.optJSONArray("warnings")
        ChargeResult.Success(
            ChargeReceipt(
                saleId = o.optL("sale_id"),
                txid = o.reqStr("txid"),
                priceMsat = o.reqLong("price_msat"),
                receivedMsat = o.reqLong("received_msat"),
                overpaidMsat = o.optL("overpaid_msat"),
                feesMsat = o.optL("fees_msat"),
                inputs = notes(o.optJSONArray("inputs")),
                change = notes(o.optJSONArray("change")),
                collectionPending = o.optBoolean("collection_pending", false),
                warnings = if (w == null) emptyList() else (0 until w.length()).map { w.optString(it) },
                durationMs = o.optL("duration_ms"),
            )
        )
    }
} catch (e: JSONException) {
    ChargeResult.Failed(TillError.BadResponse(e.message ?: "bad json"))
}

internal fun parseSales(body: String): TillResult<SalesList> = try {
    val o = JSONObject(body)
    val a = o.getJSONArray("sales")
    val rows = (0 until a.length()).map {
        val s = a.getJSONObject(it)
        Sale(
            s.reqLong("id"), s.optL("ts_unix_ms"), s.reqLong("price_msat"), s.optL("received_msat"),
            s.optL("overpaid_msat"), s.optL("fees_msat"), s.optString("txid", ""),
            if (s.isNull("client_ref")) null else s.optString("client_ref"),
        )
    }
    TillResult.Ok(SalesList(o.optL("count", rows.size.toLong()), o.reqLong("total_price_msat"), o.reqLong("total_received_msat"), rows))
} catch (e: JSONException) {
    TillResult.Err(TillError.BadResponse(e.message ?: "bad json"))
}
