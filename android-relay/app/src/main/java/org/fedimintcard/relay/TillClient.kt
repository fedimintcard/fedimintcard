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

/** Thrown by a transport for IO failures after the connection was established. */
class PostConnectException(cause: Throwable) : IOException(cause.message, cause)

fun interface Transport {
    /** Throws SocketTimeoutException on read timeout; any other IOException means unreachable. */
    @Throws(IOException::class)
    fun send(method: String, url: String, body: String?, connectMs: Int, readMs: Int): RawResponse

    /** [send] with extra request headers (the bearer, `X-Card-Link`). Fakes may ignore them. */
    @Throws(IOException::class)
    fun send(method: String, url: String, body: String?, connectMs: Int, readMs: Int, headers: Map<String, String>): RawResponse =
        send(method, url, body, connectMs, readMs)
}

sealed class TillError {
    object Unreachable : TillError()
    object Timeout : TillError()
    /** Failure after the request was (or may have been) written: outcome unknown. */
    object Interrupted : TillError()
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

/**
 * [token]: the POS token of the paired till, sent as a bearer (docs/POS-LINK.md); null when
 * unpaired. A job given a `link` id names the card link that holds its card (`X-Card-Link`).
 */
class TillClient(private val baseUrl: () -> String, private val transport: Transport, private val token: () -> String? = { null }) {
    private fun url(path: String) = baseUrl().trimEnd('/') + path

    private fun call(method: String, path: String, body: String?, readMs: Int, link: String? = null): TillResult<RawResponse> =
        try {
            val headers = HashMap<String, String>()
            token()?.let { headers["Authorization"] = "Bearer $it" }
            link?.let { headers["X-Card-Link"] = it }
            TillResult.Ok(transport.send(method, url(path), body, CONNECT_MS, readMs, headers))
        } catch (e: SocketTimeoutException) {
            TillResult.Err(TillError.Timeout)
        } catch (e: PostConnectException) {
            TillResult.Err(TillError.Interrupted)
        } catch (e: Exception) {
            TillResult.Err(TillError.Unreachable)
        }

    fun status(): TillResult<TillStatus> = when (val r = call("GET", "/v1/status", null, QUICK_READ_MS)) {
        is TillResult.Err -> r
        is TillResult.Ok -> parseStatus(r.value.body)
    }

    /**
     * Posts a charge. [pin] is consumed: it is wiped once the request has been sent (or has
     * failed), whatever the outcome, so the caller must not reuse it. The request body is the one
     * place the digits become a String, which the platform's HTTP stack needs.
     */
    fun charge(priceMsat: Long, clientRef: String, pin: Pin? = null, link: String? = null): ChargeResult = try {
        val o = JSONObject().put("price_msat", priceMsat).put("client_ref", clientRef)
        pin?.withChars { o.put("pin", String(it)) }
        val body = o.toString()
        when (val r = call("POST", "/v1/charge", body, CHARGE_READ_MS, link)) {
            is TillResult.Err -> ChargeResult.Failed(r.error)
            is TillResult.Ok -> parseCharge(r.value.status, r.value.body)
        }
    } finally {
        pin?.wipe()
    }

    fun sales(): TillResult<SalesList> = when (val r = call("GET", "/v1/sales", null, QUICK_READ_MS)) {
        is TillResult.Err -> r
        is TillResult.Ok -> parseSales(r.value.body)
    }

    /**
     * Reads the card. Long timeout on purpose: the till must reach the card over the relay and
     * reconcile every note against the federation before it can answer.
     */
    fun cardBalance(link: String? = null): BalanceResult =
        when (val r = call("POST", "/v1/card/balance", "", CARD_BALANCE_READ_MS, link)) {
            is TillResult.Err -> BalanceResult.Failed(r.error)
            is TillResult.Ok -> parseCardBalance(r.value.status, r.value.body)
        }

    /** Makes a top-up invoice (docs/TOPUP.md). The same [clientRef] answers the same top-up. */
    fun createTopUp(amountMsat: Long, clientRef: String): TopUpReply<TopUp> {
        val body = JSONObject().put("amount_msat", amountMsat).put("client_ref", clientRef).toString()
        return when (val r = call("POST", "/v1/topup", body, QUICK_READ_MS + 20_000)) {
            is TillResult.Err -> TopUpReply.Failed(r.error)
            is TillResult.Ok -> parseTopUp(r.value.status, r.value.body)
        }
    }

    fun topUpStatus(topupId: String): TopUpReply<TopUpStatus> =
        when (val r = call("GET", "/v1/topup/" + encode(topupId), null, QUICK_READ_MS)) {
            is TillResult.Err -> TopUpReply.Failed(r.error)
            is TillResult.Ok -> parseTopUpStatus(r.value.status, r.value.body)
        }

    /** Loads a paid top-up onto the card on [link]. Safe to repeat: a loaded top-up answers again. */
    fun loadTopUp(topupId: String, link: String? = null): TopUpReply<TopUpStatus> =
        when (val r = call("POST", "/v1/topup/" + encode(topupId) + "/load", "{}", CHARGE_READ_MS, link)) {
            is TillResult.Err -> TopUpReply.Failed(r.error)
            is TillResult.Ok -> parseTopUpStatus(r.value.status, r.value.body)
        }

    private fun encode(s: String) = java.net.URLEncoder.encode(s, "UTF-8")

    companion object {
        const val CONNECT_MS = 3_000
        const val QUICK_READ_MS = 10_000
        const val CHARGE_READ_MS = 125_000 // contract: at least 120 s
        const val CARD_BALANCE_READ_MS = 60_000
    }
}

class UrlConnectionTransport : Transport {
    override fun send(method: String, url: String, body: String?, connectMs: Int, readMs: Int): RawResponse =
        send(method, url, body, connectMs, readMs, emptyMap())

    override fun send(method: String, url: String, body: String?, connectMs: Int, readMs: Int, headers: Map<String, String>): RawResponse {
        val c = URL(url).openConnection() as HttpURLConnection
        try {
            for ((k, v) in headers) c.setRequestProperty(k, v)
            c.requestMethod = method
            c.connectTimeout = connectMs
            c.readTimeout = readMs
            c.useCaches = false
            c.setRequestProperty("Connection", "close")
            c.setRequestProperty("Accept", "application/json")
            if (body != null) {
                c.doOutput = true
                c.setFixedLengthStreamingMode(body.toByteArray(Charsets.UTF_8).size)
                c.setRequestProperty("Content-Type", "application/json")
            }
            try {
                c.connect()
            } catch (e: SocketTimeoutException) {
                throw ConnectException("connect timed out") // unreachable, not "outcome unknown"
            }
            try {
                if (body != null) c.outputStream.use { it.write(body.toByteArray(Charsets.UTF_8)) }
                val status = c.responseCode
                val stream = if (status >= 400) c.errorStream else c.inputStream
                val text = stream?.use { String(it.readBytes(), Charsets.UTF_8) } ?: ""
                return RawResponse(status, text)
            } catch (e: SocketTimeoutException) {
                throw e
            } catch (e: IOException) {
                throw PostConnectException(e) // request may have reached the till
            }
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
                txid = o.optString("txid", ""),
                priceMsat = o.optL("price_msat"),
                receivedMsat = o.optL("received_msat"),
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
    when (httpStatus) {
        202 -> ChargeResult.Declined(ChargeErrorCode.UNDECIDED, "", httpStatus)
        in 200..299 -> ChargeResult.Failed(TillError.Interrupted) // accepted but unreadable: outcome unknown
        else -> ChargeResult.Failed(TillError.BadResponse(e.message ?: "bad json"))
    }
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

/** `{"ok":true,"live_msat":N,"notes":[{"slot","msat","status"}],"free_slots":[..]}`. */
internal fun parseCardBalance(httpStatus: Int, body: String): BalanceResult = try {
    val o = JSONObject(body)
    if (!o.optBoolean("ok", false)) {
        // A card error or a missing card is a normal answer from this route, not a parse failure.
        val code = if (o.isNull("error")) null else o.optString("error")
        BalanceResult.Refused(ChargeErrorCode.fromWire(code), if (o.isNull("detail")) code ?: "card unreadable" else o.optString("detail"))
    } else {
        val a = o.optJSONArray("notes")
        val rows = if (a == null) emptyList() else (0 until a.length()).map {
            val n = a.getJSONObject(it)
            CardNote(n.reqLong("slot").toInt(), n.reqLong("msat"), n.optString("status", ""))
        }
        val f = o.optJSONArray("free_slots")
        val free = if (f == null) emptyList() else (0 until f.length()).map { f.getInt(it) }
        BalanceResult.Read(CardBalance(o.optL("live_msat"), rows, free))
    }
} catch (e: JSONException) {
    BalanceResult.Failed(TillError.BadResponse(e.message ?: "bad json"))
}

private fun refused(httpStatus: Int, o: JSONObject): TopUpReply.Refused {
    val code = if (o.isNull("error")) "" else o.optString("error", "")
    return TopUpReply.Refused(code, if (o.isNull("detail")) "" else o.optString("detail", ""), httpStatus)
}

/** `POST /v1/topup`: `{topup_id, amount_msat, invoice, expires_at_ms, state}`, or `{ok:false,…}`. */
internal fun parseTopUp(httpStatus: Int, body: String): TopUpReply<TopUp> = try {
    val o = JSONObject(body)
    if (o.has("ok") && !o.optBoolean("ok", true) || httpStatus >= 400) refused(httpStatus, o)
    else TopUpReply.Ok(TopUp(o.reqStr("topup_id"), o.reqLong("amount_msat"), o.reqStr("invoice"), o.reqLong("expires_at_ms"), o.optString("state", "waiting")))
} catch (e: JSONException) {
    TopUpReply.Failed(TillError.BadResponse(e.message ?: "bad json"))
}

/** `GET /v1/topup/{id}` and `POST …/load`: `{topup_id, amount_msat, state, load_txid?, detail?}`. */
internal fun parseTopUpStatus(httpStatus: Int, body: String): TopUpReply<TopUpStatus> = try {
    val o = JSONObject(body)
    if (o.has("ok") && !o.optBoolean("ok", true) || httpStatus >= 400) refused(httpStatus, o)
    else TopUpReply.Ok(
        TopUpStatus(
            o.reqStr("topup_id"), o.reqLong("amount_msat"), o.reqStr("state"),
            if (o.isNull("load_txid")) null else o.optString("load_txid").ifEmpty { null },
            if (o.isNull("detail")) null else o.optString("detail").ifEmpty { null },
        )
    )
} catch (e: JSONException) {
    TopUpReply.Failed(TillError.BadResponse(e.message ?: "bad json"))
}
