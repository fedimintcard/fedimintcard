package org.fedimintcard.relay

import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.IOException
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

private const val PHONE = "0123456789abcdef0123456789abcdef"

private fun kitAnswer(phone: String = PHONE, expires: Long = 9_000_000_000L) =
    """{"ok":true,"kit":{"version":1,"kit_id":"k1","phone_id":"$phone","issued_at_ms":1,"expires_at_ms":$expires,"cap_msat":100000000,"cap_count":200,"pool":[]}}"""

/** The till's answer to a drained body: [status] for every charge in it. */
private fun answerFor(body: String, status: String, retry: Boolean = false): String {
    val charges = JSONObject(body).getJSONArray("charges")
    val results = JSONArray()
    for (i in 0 until charges.length()) {
        results.put(JSONObject().put("txid", charges.getJSONObject(i).getString("txid")).put("status", status).put("retry", retry).put("code", "pool_entry_reused"))
    }
    return JSONObject().put("ok", true).put("results", results).toString()
}

/** The phone's core, faked: what it was asked, and a store of waiting, signing, refused and synced charges. */
private class FakeCore : OfflineCore {
    val charged = mutableListOf<String>()
    val pins = mutableListOf<String>()
    val kits = mutableListOf<String>()
    var onCharge: (String) -> CoreCharge = { ref -> paid(ref) }
    val signing = mutableSetOf<String>()
    var signingThrows = false
    val waiting = mutableListOf<String>()
    val synced = mutableListOf<String>()
    val refused = mutableListOf<RefusedEntry>()

    override fun phoneId() = PHONE
    override fun charge(card: CardLink, kitJson: String, priceMsat: Long, clientRef: String, pin: String): CoreCharge {
        charged += clientRef; pins += pin; kits += kitJson
        return onCharge(clientRef)
    }
    override fun stats() = CoreStats(waiting.size, 0, signing.size, refused.size, synced.size)
    override fun signingRefs(): List<String> {
        if (signingThrows) throw IllegalStateException("store unreadable")
        return signing.toList()
    }
    override fun drain(max: Int): CoreDrain {
        val batch = waiting.take(minOf(max, 50))
        val body = JSONObject().put("phone_id", PHONE).put("charges", JSONArray(batch.map { JSONObject().put("txid", it) })).toString()
        return CoreDrain(body, batch, refused.toList())
    }
    override fun markSynced(answerJson: String): CoreSync {
        val a = JSONObject(answerJson)
        if (!a.optBoolean("ok")) throw IllegalStateException("the till refused the sync")
        var s = 0; var r = 0; var t = 0
        val results = a.getJSONArray("results")
        for (i in 0 until results.length()) {
            val o = results.getJSONObject(i)
            val tx = o.getString("txid")
            if (tx !in waiting) continue
            when {
                o.getString("status") == "queued" || o.getString("status") == "already" -> { waiting -= tx; synced += tx; s++ }
                o.getString("status") == "refused" && !o.optBoolean("retry") -> { waiting -= tx; refused += RefusedEntry(tx, o.getString("code"), 21_000, 7); r++ }
                else -> t++
            }
        }
        return CoreSync(s, r, t)
    }
    override fun dismiss(txid: String) = refused.removeIf { it.txid == txid }

    companion object {
        fun paid(ref: String) = CoreCharge.Paid("tx-$ref", 5_000, 5_000, 0, 10, listOf(Note(0, 8_000)), listOf(Note(1, 3_000)), false)
    }
}

/** The till's two offline routes, faked at the transport: every call, and what each answers. */
private class FakeTill : Transport {
    val calls = mutableListOf<String>()
    var kit = RawResponse(200, kitAnswer())
    var charges: (String) -> RawResponse = { body -> RawResponse(200, answerFor(body, "queued")) }
    var fail: Exception? = null
    var gate: CountDownLatch? = null
    val posted get() = synchronized(calls) { calls.count { it == "POST /v1/offline/charges" } }
    val kitFetches get() = synchronized(calls) { calls.count { it.startsWith("GET /v1/offline/kit") } }

    override fun send(method: String, url: String, body: String?, connectMs: Int, readMs: Int): RawResponse {
        val path = url.removePrefix("http://till")
        synchronized(calls) { calls += "$method $path" }
        fail?.let { throw it }
        return when {
            path == "/v1/offline/kit?phone=$PHONE" -> kit
            path == "/v1/offline/charges" -> { gate?.await(5, TimeUnit.SECONDS); charges(body!!) }
            else -> error("unexpected $method $url")
        }
    }
}

class OfflineTillTest {
    @get:Rule val tmp = TemporaryFolder()
    private val core = FakeCore()
    private val till = FakeTill()
    private var coreOpen = true
    private var clock = 1_000_000L
    private var pairing: String? = "pairing-1"
    private lateinit var kits: KitStore
    private lateinit var offline: OfflineTill
    private val card: () -> CardLink = { CardLink { it } }
    private val ok = ChargeResult.Success(ChargeReceipt(9, "online", 5_000, 5_000, 0, 0, emptyList(), emptyList(), false, emptyList(), 1))

    @Before fun up() {
        kits = KitStore(tmp.root)
        offline = OfflineTill({ if (coreOpen) core else null }, kits, TillClient({ "http://till" }, till), { pairing }, { "UnsatisfiedLinkError: no library" }, { clock })
    }

    private fun withKit() = assertNotNull(kits.save("pairing-1", PHONE, kitAnswer(), clock))
    private fun pin(): Pin = PinPad().apply { "1234".forEach { press(it - '0') } }.take()!!
    private fun charge(ref: String, pin: Pin? = pin(), online: (() -> Unit) -> ChargeResult?) = offline.runCharge(5_000, ref, pin, card, online)
    private val linkDown: (() -> Unit) -> ChargeResult? = { null }
    private val tillUnused: (() -> Unit) -> ChargeResult? = { fail("this sale must never reach the till"); null }
    private fun declined(r: ChargeResult) = r as ChargeResult.Declined

    // ---- when a charge goes on the phone ----

    @Test fun aLinkThatOpensNeverReachesTheCore() {
        withKit()
        assertEquals(ok, charge("r1") { sent -> sent(); ok })
        assertTrue(core.charged.isEmpty())
    }

    @Test fun aLinkThatCannotOpenChargesOnThePhoneWithTheSameRef() {
        withKit()
        val r = charge("r1", online = linkDown) as ChargeResult.Success
        assertEquals(listOf("r1"), core.charged)
        assertEquals(listOf(kitAnswer()), core.kits) // the till's whole answer, as the core takes it
        assertEquals(listOf("1234"), core.pins)
        assertTrue(r.receipt.offline); assertTrue(r.receipt.collectionPending)
        assertEquals("tx-r1", r.receipt.txid); assertEquals(5_000L, r.receipt.priceMsat); assertEquals(10L, r.receipt.feesMsat)
        assertEquals(listOf(Note(1, 3_000)), r.receipt.change)
    }

    @Test fun aSaleOncePostedNeverGoesToThePhone() {
        withKit()
        assertEquals(ChargeResult.Failed(TillError.Timeout), charge("r1") { sent -> sent(); ChargeResult.Failed(TillError.Timeout) })
        // The retry's link does not open: the earlier reply may be lost, so the phone must not take it.
        assertEquals(ChargeResult.Failed(TillError.Unreachable), charge("r1", online = linkDown))
        assertTrue(core.charged.isEmpty())
        // Another sale still goes on the phone.
        assertTrue(charge("r2", online = linkDown) is ChargeResult.Success)
    }

    @Test fun aDeadPairingNeverChargesOnThePhone() {
        withKit()
        // withCard answers a 401 or 403 at link open as unreachable, not as a link that did not open.
        assertEquals(ChargeResult.Failed(TillError.Unreachable), charge("r1") { ChargeResult.Failed(TillError.Unreachable) })
        assertTrue(core.charged.isEmpty())
    }

    // ---- item 10: a sale held on the phone is retried there only ----

    @Test fun aSaleTheCoreHoldsGoesToTheCoreEvenWithTheTillUp() {
        withKit()
        core.signing += "r1"
        assertTrue(charge("r1", online = tillUnused) is ChargeResult.Success)
        assertEquals(listOf("r1"), core.charged)
    }

    @Test fun aLocalUndecidedIsRetriedOnThePhoneWhileTheTillAnswers() {
        withKit()
        core.onCharge = { ref -> core.signing += ref; CoreCharge.Refused("undecided", "the card left mid-sign") }
        val first = declined(charge("r1", online = linkDown))
        assertEquals(ChargeErrorCode.UNDECIDED, first.code); assertTrue(first.held); assertTrue(first.local)
        core.onCharge = { ref -> core.signing -= ref; FakeCore.paid(ref) }
        val again = charge("r1", online = tillUnused) as ChargeResult.Success
        assertTrue(again.receipt.offline)
        assertEquals(listOf("r1", "r1"), core.charged)
        // Paid: the sale is no longer the phone's alone.
        assertEquals(ok, charge("r1") { sent -> sent(); ok })
    }

    @Test fun aThrowIsHeldAndRetriedOnThePhoneWithTheSameRef() {
        withKit()
        core.onCharge = { throw RuntimeException("Internal: the core failed unexpectedly") }
        val first = declined(charge("r1", online = linkDown))
        assertEquals(ChargeErrorCode.UNDECIDED, first.code); assertTrue(first.held); assertTrue(first.local)
        assertTrue(core.signing.isEmpty()) // nothing listed signing: the throw alone keeps it on the phone
        core.onCharge = { ref -> FakeCore.paid(ref) }
        assertTrue(charge("r1", online = tillUnused) is ChargeResult.Success)
        assertEquals(listOf("r1", "r1"), core.charged)
    }

    @Test fun aSigningListThatCannotBeReadLeavesItToTheSalesChargedHere() {
        withKit()
        core.signingThrows = true
        // A sale never charged here goes to the till first.
        assertEquals(ok, charge("r1") { sent -> sent(); ok })
        assertTrue(core.charged.isEmpty())
        // One charged here whose answer the list cannot confirm counts as held, and stays on the phone.
        core.onCharge = { CoreCharge.Refused("no_card", "the card left") }
        assertTrue(declined(charge("r2", online = linkDown)).held)
        core.onCharge = { ref -> FakeCore.paid(ref) }
        assertTrue(charge("r2", online = tillUnused) is ChargeResult.Success)
    }

    @Test fun aHeldRetapThatFailsTheCardsChecksStaysHeld() {
        withKit()
        core.signing += "r1"
        for ((code, detail) in listOf("no_card" to "the card left", "wrong_pin" to "wrong PIN; 2 tries left before the card blocks", "offline_kit_expired" to "expired")) {
            core.onCharge = { CoreCharge.Refused(code, detail) }
            val r = declined(charge("r1", online = tillUnused))
            assertEquals(ChargeErrorCode.fromWire(code), r.code); assertTrue(code, r.held); assertTrue(r.local)
        }
        assertEquals(List(3) { "r1" }, core.charged)
    }

    @Test fun aWrongPinCarriesTheTriesLeft() {
        withKit()
        core.onCharge = { CoreCharge.Refused("wrong_pin", "refused", triesLeft = 2) }
        val r = declined(charge("r1", online = linkDown))
        assertEquals(ChargeErrorCode.WRONG_PIN, r.code); assertFalse(r.held)
        assertEquals(2, parseTriesLeft(r.detail))
    }

    @Test fun aRefusalThatHoldsNothingReleasesTheSaleAndKeepsItsPrice() {
        withKit()
        core.onCharge = { CoreCharge.Refused("offline_pool_exhausted", "no 32768 msat entry left", suggestedMsat = 49_000) }
        val r = declined(charge("r1", online = linkDown))
        assertEquals(ChargeErrorCode.OFFLINE_POOL_EXHAUSTED, r.code); assertEquals(49_000L, r.suggestedMsat)
        assertFalse(r.held); assertTrue(r.local); assertEquals(0, r.httpStatus)
        // Released: its next attempt tries the till first again.
        assertEquals(ok, charge("r1") { sent -> sent(); ok })
    }

    @Test fun noCoreNoKitOrAnotherPairingsKitIsNotReady() {
        // No kit.
        val noKit = declined(charge("r1", online = linkDown))
        assertEquals(ChargeErrorCode.OFFLINE_NOT_READY, noKit.code); assertTrue(noKit.local); assertFalse(noKit.held)
        // Another pairing's kit.
        assertNotNull(kits.save("pairing-0", PHONE, kitAnswer(), clock))
        assertEquals(ChargeErrorCode.OFFLINE_NOT_READY, declined(charge("r2", online = linkDown)).code)
        // Unpaired.
        withKit(); pairing = null
        assertEquals(ChargeErrorCode.OFFLINE_NOT_READY, declined(charge("r3", online = linkDown)).code)
        // No core: the reason it is off is the detail.
        pairing = "pairing-1"; coreOpen = false
        val noCore = declined(charge("r4", online = linkDown))
        assertEquals(ChargeErrorCode.OFFLINE_NOT_READY, noCore.code); assertEquals("UnsatisfiedLinkError: no library", noCore.detail)
        assertTrue(core.charged.isEmpty())
    }

    @Test fun thePinIsWipedOnEveryPath() {
        withKit()
        val paths = listOf<Pair<String, (() -> Unit) -> ChargeResult?>>(
            "online" to { sent -> sent(); ok },
            "paid on the phone" to linkDown,
            "refused on the phone" to { core.onCharge = { CoreCharge.Refused("insufficient", "") }; null },
            "the core threw" to { core.onCharge = { throw RuntimeException("boom") }; null },
            "a dead pairing" to { ChargeResult.Failed(TillError.Unreachable) },
        )
        for ((i, p) in paths.withIndex()) {
            core.onCharge = { ref -> FakeCore.paid(ref) }
            val pin = pin()
            charge("w$i", pin, p.second)
            assertTrue(p.first, pin.isWiped)
        }
        // A sale posted before, then unreachable; and no kit at all.
        val posted = pin()
        charge("w0", posted, linkDown)
        assertTrue(posted.isWiped)
        pairing = "pairing-2"
        val notReady = pin()
        charge("w9", notReady, linkDown)
        assertTrue(notReady.isWiped)
        // A held sale, on the phone only.
        pairing = "pairing-1"; core.signing += "w8"; core.onCharge = { ref -> FakeCore.paid(ref) }
        val held = pin()
        charge("w8", held, tillUnused)
        assertTrue(held.isWiped)
    }

    @Test fun theUsbRelayHasNoCardWhileTheCoreChargesIt() {
        withKit()
        var during = false
        core.onCharge = { ref -> during = offline.localCharging; FakeCore.paid(ref) }
        assertFalse(offline.localCharging)
        charge("r1", online = linkDown)
        assertTrue(during); assertFalse(offline.localCharging)
    }

    // ---- the kit ----

    @Test fun theKitIsFetchedWhenMissingThenEveryTenMinutes() {
        assertTrue(offline.refreshKit())
        assertEquals(1, till.kitFetches)
        assertEquals(clock, kits.read("pairing-1")!!.fetchedAtMs)
        assertFalse(offline.refreshKit()); assertEquals(1, till.kitFetches)
        clock += OfflineTill.KIT_MAX_AGE_MS - 1
        assertFalse(offline.refreshKit()); assertEquals(1, till.kitFetches)
        clock += 1
        assertTrue(offline.refreshKit()); assertEquals(2, till.kitFetches)
    }

    @Test fun aLocalKitPoolOrCapRefusalFetchesAgainAtTheNextPoll() {
        for (code in listOf("offline_kit_expired", "offline_pool_exhausted", "offline_cap_reached")) {
            withKit()
            val before = till.kitFetches
            assertFalse(offline.refreshKit()) // fresh
            core.onCharge = { CoreCharge.Refused(code, "") }
            charge("r-$code", online = linkDown)
            assertTrue(code, offline.refreshKit())
            assertEquals(before + 1, till.kitFetches)
            assertFalse(offline.refreshKit()) // fresh again
        }
        // Other refusals leave the kit alone.
        withKit()
        core.onCharge = { CoreCharge.Refused("insufficient", "") }
        charge("r-insufficient", online = linkDown)
        val before = till.kitFetches
        assertFalse(offline.refreshKit()); assertEquals(before, till.kitFetches)
    }

    @Test fun aFailedFetchKeepsTheKitAndWaitsAMinute() {
        withKit()
        clock += OfflineTill.KIT_MAX_AGE_MS
        till.kit = RawResponse(503, """{"ok":false,"error":"offline_not_ready","detail":"no certificate key yet"}""")
        assertFalse(offline.refreshKit())
        assertEquals("HTTP 503 offline_not_ready", offline.kitError)
        assertEquals(kitAnswer(), kits.read("pairing-1")!!.answer)
        assertFalse(offline.refreshKit()); assertEquals(1, till.kitFetches)
        clock += OfflineTill.RETRY_MS
        till.kit = RawResponse(200, kitAnswer())
        assertTrue(offline.refreshKit()); assertEquals(2, till.kitFetches)
        assertNull(offline.kitError)
    }

    @Test fun aKitForAnotherPhoneOrUnpairedIsNotKept() {
        till.kit = RawResponse(200, kitAnswer(phone = "f".repeat(32)))
        assertFalse(offline.refreshKit())
        assertNull(kits.read("pairing-1"))
        assertEquals("the till's answer is not a kit for this phone", offline.kitError)
        pairing = null
        clock += OfflineTill.RETRY_MS
        assertFalse(offline.refreshKit())
        assertEquals(1, till.kitFetches)
    }

    // ---- the sync ----

    @Test fun aSyncPostsWhatWaitsMarksItOnA200AndCountsItUntilTheTillShowsIt() {
        core.waiting += listOf("t1", "t2")
        val beforeSync = offline.pollMark()
        assertEquals(2, offline.sync())
        assertEquals(1, till.posted)
        assertEquals(listOf("t1", "t2"), core.synced)
        // The status poll that started before the sync does not include them: they still count.
        offline.statusShown(beforeSync)
        assertEquals(PhoneCounts(waiting = 2), offline.counts())
        val afterSync = offline.pollMark()
        offline.statusShown(afterSync)
        assertEquals(PhoneCounts(waiting = 0), offline.counts())
        assertNull(offline.syncError)
    }

    @Test fun aSyncLoopsInBatchesUntilNothingWaits() {
        core.waiting += (1..120).map { "t$it" }
        assertEquals(120, offline.sync())
        assertEquals(3, till.posted)
        assertTrue(core.waiting.isEmpty())
        assertEquals(0, offline.sync()) // nothing waits: no request at all
        assertEquals(3, till.posted)
    }

    @Test fun aSyncStopsOnAnHttpErrorMarksNothingAndWaitsAMinute() {
        core.waiting += listOf("t1")
        till.charges = { RawResponse(503, """{"ok":false,"error":"offline_not_ready"}""") }
        assertEquals(0, offline.sync())
        assertEquals(listOf("t1"), core.waiting)
        assertEquals("HTTP 503 offline_not_ready", offline.syncError)
        assertEquals(0, offline.sync()); assertEquals(1, till.posted)
        clock += OfflineTill.RETRY_MS
        till.charges = { body -> RawResponse(200, answerFor(body, "already")) }
        assertEquals(1, offline.sync()); assertEquals(2, till.posted)
        assertNull(offline.syncError)
    }

    @Test fun aSyncStopsOnATransportErrorAndAnAnswerTheCoreRejects() {
        core.waiting += listOf("t1")
        till.fail = IOException("no route")
        assertEquals(0, offline.sync())
        assertEquals("the till did not answer", offline.syncError)
        till.fail = null; clock += OfflineTill.RETRY_MS
        till.charges = { RawResponse(200, """{"ok":false,"error":"internal"}""") }
        assertEquals(0, offline.sync())
        assertEquals(listOf("t1"), core.waiting)
        assertEquals("the till refused the sync", offline.syncError)
    }

    @Test fun aRoundTheTillTookNoneOfStopsTheSync() {
        core.waiting += listOf("t1", "t2")
        till.charges = { body -> RawResponse(200, answerFor(body, "refused", retry = true)) }
        assertEquals(0, offline.sync())
        assertEquals(1, till.posted) // not again in a loop
        assertEquals(listOf("t1", "t2"), core.waiting)
        assertNotNull(offline.syncError)
        assertEquals(0, offline.sync()); assertEquals(1, till.posted)
    }

    @Test fun chargesTheTillRefusesAreListedUntilDismissed() {
        withKit()
        core.waiting += listOf("t1")
        till.charges = { body -> RawResponse(200, answerFor(body, "refused")) }
        assertEquals(0, offline.sync()) // refused is progress, but nothing synced
        assertNull(offline.syncError)
        val v = offline.view()
        assertEquals(listOf(RefusedEntry("t1", "pool_entry_reused", 21_000, 7)), v.refused)
        assertTrue(offline.dismiss("t1"))
        assertTrue(offline.view().refused.isEmpty())
        assertFalse(offline.dismiss("t1"))
    }

    @Test fun aSyncThatSyncedFetchesANewKit() {
        withKit() // fresh
        core.waiting += listOf("t1")
        assertEquals(1, offline.sync())
        assertEquals(1, till.kitFetches)
    }

    @Test fun oneSyncAtATime() {
        core.waiting += listOf("t1")
        val gate = CountDownLatch(1)
        till.gate = gate
        val first = Thread { offline.sync() }.apply { start() }
        val deadline = System.currentTimeMillis() + 5_000
        while (till.posted == 0 && System.currentTimeMillis() < deadline) Thread.sleep(5)
        assertEquals(0, offline.sync()) // the first is still posting
        gate.countDown()
        first.join(5_000)
        assertEquals(1, till.posted)
        assertEquals(listOf("t1"), core.synced)
    }

    @Test fun nothingSyncsUnpairedOrWithoutACore() {
        core.waiting += listOf("t1")
        pairing = null
        assertEquals(0, offline.sync())
        pairing = "pairing-1"; coreOpen = false
        assertEquals(0, offline.sync())
        assertTrue(till.calls.isEmpty())
    }

    // ---- what the screens show ----

    @Test fun theCombinedCount() {
        core.waiting += listOf("t1", "t2"); core.signing += "r9"
        assertEquals(PhoneCounts(waiting = 2, signing = 1), offline.counts())
        val status = TillStatus("ab", null, 1, true, false, offlineQueued = 3)
        assertEquals(5L, waitingToSync(status, offline.counts()))
        assertEquals(2L, waitingToSync(null, offline.counts())) // the till not reached yet
        assertEquals(3L, waitingToSync(status, PhoneCounts()))
    }

    @Test fun theViewSaysWhyOfflineIsOffAndWhatIsPending() {
        coreOpen = false
        assertEquals("UnsatisfiedLinkError: no library", offline.view().off)
        assertFalse(offline.view().starting)
        assertEquals(0, offline.view().pending)
        val opening = OfflineTill({ null }, kits, TillClient({ "http://till" }, till), { pairing })
        assertNull(opening.view().off); assertTrue(opening.view().starting)
        coreOpen = true
        withKit()
        core.waiting += listOf("t1", "t2"); core.signing += "r1"
        val v = offline.view()
        assertNull(v.off); assertTrue(v.paired)
        assertEquals("k1", v.kit!!.kitId)
        assertEquals(3, v.pending)
    }
}
