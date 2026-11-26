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
    /**
     * [suggestedMsat]: `suggested_msat`, a price the card can pay instead (`offline_no_change_room`,
     * `offline_pool_exhausted`). [local]: the phone's own core answered (an offline charge; no
     * HTTP status, 0). [held]: the phone holds the sale `signing` after this answer, so only that
     * card's next tap on this phone finishes it (docs/POS-OFFLINE.md); never the till.
     */
    data class Declined(
        val code: ChargeErrorCode,
        val detail: String,
        val httpStatus: Int,
        val suggestedMsat: Long? = null,
        val held: Boolean = false,
        val local: Boolean = false,
        /** `wrong_pin`: the wrong tries in a row. */
        val wrongTries: Int? = null,
        /** `wrong_pin`: seconds the next try waits; `pin_wait`: seconds still owed. */
        val waitS: Int? = null,
    ) : ChargeResult()
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

    /**
     * Makes a top-up invoice (docs/TOPUP.md). The same [clientRef] answers the same top-up. With
     * [shortId] the till fits the amount to that card's free slots, or refuses a full card
     * (`card_full`) before any invoice exists.
     */
    fun createTopUp(amountMsat: Long, clientRef: String, shortId: String? = null): TopUpReply<TopUp> {
        val o = JSONObject().put("amount_msat", amountMsat).put("client_ref", clientRef).put("fee_quote_version", 1)
        if (shortId != null) o.put("short_id", shortId)
        val body = o.toString()
        return when (val r = call("POST", "/v1/topup/fee-inclusive", body, QUICK_READ_MS + 20_000)) {
            is TillResult.Err -> TopUpReply.Failed(r.error)
            is TillResult.Ok -> parseTopUp(r.value.status, r.value.body)
        }
    }

    fun topUpStatus(topupId: String): TopUpReply<TopUpStatus> =
        when (val r = call("GET", "/v1/topup/" + encode(topupId), null, QUICK_READ_MS)) {
            is TillResult.Err -> TopUpReply.Failed(r.error)
            is TillResult.Ok -> parseTopUpStatus(r.value.status, r.value.body)
        }

    /**
     * The paid top-ups waiting for [shortId] (or for no card), oldest first. Null on any error or
     * a till without the route: the screen then just shows no button.
     */
    fun waitingTopUps(shortId: String): List<WaitingTopUp>? =
        when (val r = call("GET", "/v1/topups/waiting?short_id=" + encode(shortId), null, QUICK_READ_MS)) {
            is TillResult.Err -> null
            is TillResult.Ok -> parseWaitingTopUps(r.value.status, r.value.body)
        }

    /** Loads a paid top-up onto the card on [link]. Safe to repeat: a loaded top-up answers again. */
    fun loadTopUp(topupId: String, link: String? = null): TopUpReply<TopUpStatus> =
        when (val r = call("POST", "/v1/topup/" + encode(topupId) + "/load", "{}", CHARGE_READ_MS, link)) {
            is TillResult.Err -> TopUpReply.Failed(r.error)
            is TillResult.Ok -> parseTopUpStatus(r.value.status, r.value.body, paidLoad = true)
        }

    /**
     * `GET /v1/offline/kit?phone=`: this phone's offline kit (docs/TILL-API.md). The till's whole
     * answer on a 200, which the core takes as it is; any other status is a [TillError.BadResponse].
     */
    fun offlineKit(phoneId: String): TillResult<String> =
        answer(call("GET", "/v1/offline/kit?phone=" + encode(phoneId), null, OFFLINE_READ_MS))

    /**
     * `POST /v1/offline/charges` with [body], the core's `drain` as it is: the till's answer on a
     * 200, for the core's `mark_synced`; any other status is a [TillError.BadResponse]. Safe to
     * repeat: the till answers a charge it has as `already`.
     */
    fun postOfflineCharges(body: String): TillResult<String> =
        answer(call("POST", "/v1/offline/charges", body, OFFLINE_READ_MS))

    private fun answer(r: TillResult<RawResponse>): TillResult<String> = when (r) {
        is TillResult.Err -> r
        is TillResult.Ok -> if (r.value.status == 200) TillResult.Ok(r.value.body)
        else TillResult.Err(TillError.BadResponse("HTTP ${r.value.status}" + (errorCode(r.value.body)?.let { " $it" } ?: "")))
    }

    private fun encode(s: String) = java.net.URLEncoder.encode(s, "UTF-8")

    companion object {
        const val CONNECT_MS = 3_000
        const val QUICK_READ_MS = 10_000
        const val CHARGE_READ_MS = 125_000 // contract: at least 120 s
        const val CARD_BALANCE_READ_MS = 60_000
        /** A kit is at most 128 KB, a sync at most 50 charges (1 MiB). */
        const val OFFLINE_READ_MS = 30_000
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

/** The `error` code of a till's `{"ok":false,"error":…}` answer, or null. */
internal fun errorCode(body: String): String? = try {
    JSONObject(body).let { if (it.isNull("error")) null else it.optString("error").ifEmpty { null } }
} catch (e: JSONException) {
    null
}

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
            offlineQueued = o.optJSONObject("offline")?.optLong("queued", 0) ?: 0,
            offlineQueuedMsat = o.optJSONObject("offline")?.optLong("queued_msat", 0) ?: 0,
            offlineDoubleSpent = (o.optJSONObject("offline")?.opt("double_spent") as? Number)?.toLong(),
        )
    )
} catch (e: JSONException) {
    TillResult.Err(TillError.BadResponse(e.message ?: "bad json"))
}

internal fun parseCharge(httpStatus: Int, body: String): ChargeResult = try {
    val o = JSONObject(body)
    if (!o.has("ok")) throw JSONException("missing ok")
    if (!o.getBoolean("ok")) {
        val detail = if (o.isNull("detail")) "" else o.optString("detail", "")
        ChargeResult.Declined(
            ChargeErrorCode.fromTill(o.optString("error", ""), detail),
            detail,
            httpStatus,
            if (o.isNull("suggested_msat")) null else o.optLong("suggested_msat", 0L).takeIf { it > 0 },
            wrongTries = if (o.has("wrong_tries") && !o.isNull("wrong_tries")) o.optInt("wrong_tries") else null,
            waitS = if (o.has("wait_s") && !o.isNull("wait_s")) o.optInt("wait_s") else null,
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
                offline = o.optBoolean("offline", false),
            )
        )
    }
} catch (e: JSONException) {
    when (httpStatus) {
        202 -> ChargeResult.Declined(ChargeErrorCode.UNDECIDED, "", httpStatus)
        in 200..299, in 500..599 -> ChargeResult.Failed(TillError.Interrupted) // unreadable: a 2xx was accepted, a 5xx may have been; outcome unknown
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

/**
 * `{"ok":true,"live_msat":N,"stranded_msat":N,"stranded_until_ms":N|null,"notes":[{"slot","msat","status"}],"free_slots":[..]}`.
 * A till from before these fields sends neither, and a value that is not a number reads as
 * absent: nothing on hold, no date.
 */
internal fun parseCardBalance(httpStatus: Int, body: String): BalanceResult = try {
    val o = JSONObject(body)
    if (!o.optBoolean("ok", false)) {
        // A card error or a missing card is a normal answer from this route, not a parse failure.
        val code = if (o.isNull("error")) null else o.optString("error")
        val detail = if (o.isNull("detail")) code ?: "card unreadable" else o.optString("detail")
        BalanceResult.Refused(ChargeErrorCode.fromTill(code, detail), detail)
    } else {
        val a = o.optJSONArray("notes")
        val rows = if (a == null) emptyList() else (0 until a.length()).map {
            val n = a.getJSONObject(it)
            CardNote(n.reqLong("slot").toInt(), n.reqLong("msat"), n.optString("status", ""))
        }
        val f = o.optJSONArray("free_slots")
        val free = if (f == null) emptyList() else (0 until f.length()).map { f.getInt(it) }
        val shortId = if (o.isNull("short_id")) null else o.optString("short_id").ifEmpty { null }
        BalanceResult.Read(
            CardBalance(
                o.optL("live_msat"), rows, free, shortId,
                strandedMsat = (o.opt("stranded_msat") as? Number)?.toLong() ?: 0,
                strandedUntilMs = (o.opt("stranded_until_ms") as? Number)?.toLong(),
            )
        )
    }
} catch (e: JSONException) {
    BalanceResult.Failed(TillError.BadResponse(e.message ?: "bad json"))
}

// Till :11's paid load names both federation IDs and the card short ID in this form.
private val paidLoadFederationMismatch = Regex("card [0-9a-f]{8} belongs to federation ([0-9a-f]{64}), and this wallet is in ([0-9a-f]{64})")

private fun wrongPaidLoadScope(detail: String): Boolean {
    if (detail == "the registry mint version differs from this wallet") return true
    val match = paidLoadFederationMismatch.matchEntire(detail) ?: return false
    return match.groupValues[1] != match.groupValues[2]
}

private fun refused(httpStatus: Int, o: JSONObject, paidLoad: Boolean = false): TopUpReply.Refused {
    val code = if (o.isNull("error")) "" else o.optString("error", "")
    val detail = if (o.isNull("detail")) "" else o.optString("detail", "")
    if (ChargeErrorCode.fromTill(code, detail) == ChargeErrorCode.WRONG_FEDERATION ||
        (paidLoad && httpStatus == 500 && code == "internal" && wrongPaidLoadScope(detail))
    ) return TopUpReply.Refused(ChargeErrorCode.WRONG_FEDERATION.wire, "", httpStatus)
    return TopUpReply.Refused(code, detail, httpStatus)
}

/** JSON integers only. The platform getLong can also truncate, coerce strings or saturate. */
private fun JSONObject.quoteInteger(key: String): Long? = when (val n = opt(key)) {
    is Int -> n.toLong()
    is Long -> n
    else -> null
}

private fun JSONObject.topUpFace(): Long = quoteInteger("amount_msat")?.takeIf { it > 0 }
    ?: throw JSONException("invalid top-up amount")

internal fun parseTopUp(httpStatus: Int, body: String): TopUpReply<TopUp> {
    val invalid = TopUpReply.Refused("invalid_topup_quote", "", httpStatus)
    if (httpStatus == 404 || httpStatus == 405) return TopUpReply.Refused("topup_quote_version", "", httpStatus)
    return try {
        val o = JSONObject(body)
        if (o.has("ok") && !o.optBoolean("ok", true) || httpStatus >= 400) return refused(httpStatus, o)
        val id = o.reqStr("topup_id")
        val state = o.reqStr("state")
        val face = o.quoteInteger("amount_msat") ?: return invalid
        if (id.isEmpty() || face <= 0) return invalid
        // An idempotent replay may find an older, already-paid invoice. It only needs loading.
        if (state in setOf("paid", "loaded", "expired", "failed")) {
            return TopUpReply.Ok(TopUp(id, face, "", 0, state, loadTxid = o.opt("load_txid") as? String))
        }
        val fee = o.quoteInteger("topup_fee_msat") ?: return invalid
        val total = o.quoteInteger("invoice_amount_msat") ?: return invalid
        val invoice = o.opt("invoice") as? String ?: return invalid
        val expiry = o.quoteInteger("expires_at_ms") ?: return invalid
        if (state != "waiting" || o.quoteInteger("fee_quote_version") != 1L || fee < 0 || total < face ||
            total - face != fee || expiry <= 0 || !verifiedInvoiceTotal(invoice, total)) return invalid
        TopUpReply.Ok(TopUp(id, face, invoice, expiry, state, TopUpQuote(total, fee)))
    } catch (e: JSONException) {
        TopUpReply.Failed(TillError.BadResponse(e.message ?: "bad json"))
    }
}

/** `GET /v1/topups/waiting`: `{ok:true, topups:[{topup_id, amount_msat, for_card, created_at_ms}]}`. */
internal fun parseWaitingTopUps(httpStatus: Int, body: String): List<WaitingTopUp>? = try {
    val o = JSONObject(body)
    if (httpStatus >= 400 || !o.optBoolean("ok", true)) null
    else {
        val a = o.getJSONArray("topups")
        (0 until a.length()).map { i ->
            val t = a.getJSONObject(i)
            WaitingTopUp(
                t.reqStr("topup_id"), t.topUpFace(),
                if (t.isNull("for_card")) null else t.optString("for_card").ifEmpty { null },
                t.optL("created_at_ms"),
            )
        }
    }
} catch (e: JSONException) {
    null
}

/** `GET /v1/topup/{id}` and `POST …/load`: `{topup_id, amount_msat, state, load_txid?, detail?}`. */
internal fun parseTopUpStatus(httpStatus: Int, body: String, paidLoad: Boolean = false): TopUpReply<TopUpStatus> = try {
    val o = JSONObject(body)
    if (o.has("ok") && !o.optBoolean("ok", true) || httpStatus >= 400) refused(httpStatus, o, paidLoad)
    else TopUpReply.Ok(
        TopUpStatus(
            o.reqStr("topup_id"), o.topUpFace(), o.reqStr("state"),
            if (o.isNull("load_txid")) null else o.optString("load_txid").ifEmpty { null },
            if (o.isNull("detail")) null else o.optString("detail").ifEmpty { null },
        )
    )
} catch (e: JSONException) {
    TopUpReply.Failed(TillError.BadResponse(e.message ?: "bad json"))
}
