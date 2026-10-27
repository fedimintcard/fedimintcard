package org.fedimintcard.relay

import org.junit.Assert.*
import org.junit.Test
import java.io.IOException
import java.net.SocketTimeoutException

class TillClientTest {
    private class Fake(var resp: RawResponse? = null, var ex: Exception? = null) : Transport {
        var method = ""; var url = ""; var body: String? = null; var readMs = 0
        override fun send(method: String, url: String, body: String?, connectMs: Int, readMs: Int): RawResponse {
            this.method = method; this.url = url; this.body = body; this.readMs = readMs
            ex?.let { throw it }
            return resp!!
        }
    }
    private fun client(f: Fake, base: String = "http://h:1") = TillClient({ base }, f)
    private fun raw(s: Int, b: String) = RawResponse(s, b)

    private val okCharge = """{"ok":true,"sale_id":3,"txid":"abcdef0123456789","price_msat":5000000,"received_msat":5000000,"overpaid_msat":0,"fees_msat":10,
        "inputs":[{"slot":0,"msat":4000000},{"slot":2,"msat":4000000}],"change":[{"slot":1,"msat":3000000}],"collection_pending":true,"warnings":["w"],"duration_ms":900}"""

    @Test fun statusOk() {
        val f = Fake(raw(200, """{"federation_id":"ab","federation_name":null,"wallet_balance_msat":42,"card_relay_ok":true,"busy":false}"""))
        val r = client(f).status() as TillResult.Ok
        assertNull(r.value.federationName); assertEquals(42L, r.value.walletBalanceMsat); assertTrue(r.value.cardRelayOk)
        assertEquals("http://h:1/v1/status", f.url); assertEquals("GET", f.method)
    }
    @Test fun statusMissingFieldsIsBadResponse() {
        val r = client(Fake(raw(200, """{"federation_id":"ab"}"""))).status()
        assertTrue((r as TillResult.Err).error is TillError.BadResponse)
    }
    @Test fun chargeSuccess() {
        val s = client(Fake(raw(200, okCharge))).charge(5_000_000, "r") as ChargeResult.Success
        assertEquals(3L, s.receipt.saleId); assertEquals(2, s.receipt.inputs.size); assertEquals(1, s.receipt.change.size)
        assertEquals(3_000_000L, s.receipt.change[0].msat); assertTrue(s.receipt.collectionPending); assertEquals(10L, s.receipt.feesMsat)
        assertEquals(0L, s.receipt.overpaidMsat); assertEquals(listOf("w"), s.receipt.warnings)
    }
    @Test fun chargeEmptyChange() {
        val s = parseCharge(200, """{"ok":true,"txid":"t","price_msat":1,"received_msat":2,"overpaid_msat":1,"change":[]}""") as ChargeResult.Success
        assertTrue(s.receipt.change.isEmpty()); assertEquals(1L, s.receipt.overpaidMsat)
    }
    @Test fun everyErrorCodeMaps() {
        for (c in ChargeErrorCode.values()) {
            if (c == ChargeErrorCode.UNKNOWN) continue
            val d = parseCharge(409, """{"ok":false,"error":"${c.wire}","detail":"x"}""") as ChargeResult.Declined
            assertEquals(c, d.code); assertEquals("x", d.detail); assertEquals(409, d.httpStatus)
        }
    }
    @Test fun unknownCode() {
        assertEquals(ChargeErrorCode.UNKNOWN, (parseCharge(500, """{"ok":false,"error":"wat"}""") as ChargeResult.Declined).code)
        assertEquals(ChargeErrorCode.UNKNOWN, ChargeErrorCode.fromWire(""))
    }
    @Test fun okTrueWithMissingFieldsIsStillSuccess() {
        val r = parseCharge(200, """{"ok":true}""")
        assertTrue(r is ChargeResult.Success)
    }
    @Test fun unparsable2xxIsOutcomeUnknown() {
        assertEquals(ChargeResult.Failed(TillError.Interrupted), parseCharge(200, "<html>"))
        assertEquals(ChargeResult.Declined(ChargeErrorCode.UNDECIDED, "", 202), parseCharge(202, "oops"))
        assertTrue((parseCharge(502, "<html>") as ChargeResult.Failed).error is TillError.BadResponse)
    }
    @Test fun postConnectFailureIsInterrupted() {
        assertEquals(ChargeResult.Failed(TillError.Interrupted), client(Fake(ex = PostConnectException(IOException("reset")))).charge(1, "r"))
    }
    @Test fun nonJsonIsBadResponse() {
        val r = client(Fake(raw(502, "<html>"))).charge(1, "r")
        assertTrue((r as ChargeResult.Failed).error is TillError.BadResponse)
    }
    @Test fun http500WithOkFalseIsDeclined() {
        val r = client(Fake(raw(500, """{"ok":false,"error":"internal","detail":"boom"}"""))).charge(1, "r") as ChargeResult.Declined
        assertEquals(ChargeErrorCode.INTERNAL, r.code); assertEquals(500, r.httpStatus)
    }
    @Test fun http202Undecided() {
        val r = client(Fake(raw(202, """{"ok":false,"error":"undecided","detail":"pending"}"""))).charge(1, "r") as ChargeResult.Declined
        assertEquals(ChargeErrorCode.UNDECIDED, r.code)
    }
    @Test fun salesEmptyAndMany() {
        val e = client(Fake(raw(200, """{"count":0,"total_price_msat":0,"total_received_msat":0,"sales":[]}"""))).sales() as TillResult.Ok
        assertEquals(0, e.value.sales.size)
        val m = client(Fake(raw(200, """{"count":2,"total_price_msat":3000,"total_received_msat":3000,"sales":[
          {"id":1,"ts_unix_ms":10,"price_msat":1000,"received_msat":1000,"overpaid_msat":0,"fees_msat":0,"txid":"aa","inputs":[],"change":[],"client_ref":null},
          {"id":2,"ts_unix_ms":20,"price_msat":2000,"received_msat":2000,"overpaid_msat":0,"fees_msat":1,"txid":"bb","inputs":[],"change":[],"client_ref":"u"}]}"""))).sales() as TillResult.Ok
        assertEquals(2, m.value.sales.size); assertNull(m.value.sales[0].clientRef); assertEquals("u", m.value.sales[1].clientRef); assertEquals(3000L, m.value.totalPriceMsat)
    }
    @Test fun transportExceptions() {
        assertEquals(ChargeResult.Failed(TillError.Timeout), client(Fake(ex = SocketTimeoutException())).charge(1, "r"))
        assertEquals(ChargeResult.Failed(TillError.Unreachable), client(Fake(ex = IOException("x"))).charge(1, "r"))
        assertEquals(TillResult.Err(TillError.Unreachable), client(Fake(ex = IOException())).status())
        assertEquals(TillResult.Err(TillError.Timeout), client(Fake(ex = SocketTimeoutException())).sales())
    }
    @Test fun requestShape() {
        val f = Fake(raw(200, okCharge))
        client(f, "http://h:1///").charge(7000, "ref-1")
        assertEquals("http://h:1/v1/charge", f.url); assertEquals("POST", f.method)
        assertTrue(f.body!!.contains("\"price_msat\":7000")); assertTrue(f.body!!.contains("\"client_ref\":\"ref-1\""))
        assertTrue(f.readMs >= 120_000)
    }
}
