package org.fedimintcard.relay

import org.json.JSONException
import org.json.JSONObject
import java.util.concurrent.atomic.AtomicBoolean

/** What the phone's core answered a charge ([PosCoreAdapter] maps the generated `ChargeOutcome`). */
sealed class CoreCharge {
    /** Paid offline: the signed transaction waits in the phone's store for the till. */
    data class Paid(
        val txid: String,
        val priceMsat: Long,
        /** What the till gets: the price and the overpay. */
        val tillMsat: Long,
        val overpayMsat: Long,
        val feesMsat: Long,
        val inputs: List<Note>,
        val change: List<Note>,
        val replayed: Boolean,
    ) : CoreCharge()

    /** Not charged: [code] is the till API's; [suggestedMsat] a price that pays instead; [triesLeft] `wrong_pin`'s. */
    data class Refused(val code: String, val detail: String, val suggestedMsat: Long? = null, val triesLeft: Int? = null) : CoreCharge()
}

/** The phone's store: [waiting] to sync, [signing] (held for their cards), [refused] by the till, [synced]. */
data class CoreStats(val waiting: Int = 0, val waitingMsat: Long = 0, val signing: Int = 0, val refused: Int = 0, val synced: Int = 0)

/** A charge the till refused at sync: the card paid, the till will not take it. Kept until dismissed. */
data class RefusedEntry(val txid: String, val code: String, val priceMsat: Long, val tsUnixMs: Long)

/** [body] to post to `POST /v1/offline/charges` (the charges [txids]; none when nothing waits), and the refused charges. */
data class CoreDrain(val body: String, val txids: List<String>, val refused: List<RefusedEntry>)

/** What `mark_synced` did with the till's answer. */
data class CoreSync(val synced: Int, val refused: Int, val retry: Int)

/**
 * The phone's offline core, host/pos-core through UniFFI ([PosCoreAdapter] is the real one; tests
 * fake it). Every call blocks, so none runs on the main thread, and any may throw.
 */
interface OfflineCore {
    fun phoneId(): String

    /** Takes [priceMsat] from [card] with [kitJson] for the sale [clientRef]. Blocks on the card. */
    fun charge(card: CardLink, kitJson: String, priceMsat: Long, clientRef: String, pin: String): CoreCharge
    fun stats(): CoreStats
    fun signingRefs(): List<String>
    fun drain(max: Int): CoreDrain
    fun markSynced(answerJson: String): CoreSync
    fun dismiss(txid: String): Boolean
}

/**
 * The phone's side of the home screen's counts: [waiting] to sync (charges it synced included until
 * the till's status shows them) and [signing], sales held for their cards.
 */
data class PhoneCounts(val waiting: Long = 0, val signing: Int = 0)

/** "N payments waiting to sync": the till's offline queue and the phone's (docs/POS-OFFLINE.md § Sync). */
fun waitingToSync(till: TillStatus?, phone: PhoneCounts): Long = (till?.offlineQueued ?: 0) + phone.waiting

/**
 * What Settings shows of the phone's offline payments. [off]: why they are off (the library or the
 * store failed), or null; [starting] while the store is still being opened.
 */
data class OfflineView(
    val off: String?,
    val starting: Boolean,
    val paired: Boolean,
    val kit: KitStore.Kit?,
    val stats: CoreStats?,
    val refused: List<RefusedEntry>,
    val kitError: String?,
    val syncError: String?,
) {
    /** What re-pairing or unpairing would strand: waiting to sync, or held for their cards. */
    val pending: Int get() = stats?.let { it.waiting + it.signing } ?: 0
}

/**
 * Every offline decision of the Android POS (docs/POS-OFFLINE.md § Android): when a charge goes to
 * the phone's core instead of the till, the kit, the sync, and what the screens show of them. The
 * core, the kit and the till are given, so tests can fake each. [pairing] is the current pairing's
 * [pairingId], null when unpaired (then nothing here runs: no kit, no sync, no local charge).
 *
 * In memory only, for this activity: the sales a POST /v1/charge was sent for ([postedRefs]; never
 * taken on the phone after that) and the sales charged on the phone ([localRefs]; never sent to the
 * till while held or after a throw).
 */
class OfflineTill(
    private val core: () -> OfflineCore?,
    private val kits: KitStore,
    private val till: TillClient,
    private val pairing: () -> String?,
    private val offReason: () -> String? = { null },
    private val now: () -> Long = System::currentTimeMillis,
) {
    private val lock = Any()
    private val postedRefs = HashSet<String>()
    private val localRefs = HashSet<String>()
    private var awaiting = 0
    private var syncSeq = 0L
    private val refreshing = AtomicBoolean(false)
    private val syncing = AtomicBoolean(false)
    @Volatile private var kitStale = false
    @Volatile private var kitRetryAt = 0L
    @Volatile private var syncRetryAt = 0L
    @Volatile private var lastStats: CoreStats? = null

    /** True while the core drives the card: the USB relay then answers "no card" to anyone else. */
    @Volatile var localCharging = false
        private set

    /** Why the last kit fetch failed, cleared by one that works. */
    @Volatile var kitError: String? = null
        private set

    /** Why the last sync stopped short, cleared by one that sends everything. */
    @Volatile var syncError: String? = null
        private set

    /**
     * One attempt at the sale [ref]. A sale this phone holds signing, or whose local charge threw,
     * goes to the core only, with the same ref, never to the till (the A4 order does not apply to
     * it). Any other sale tries the till first: [online] answers null when the card link did not
     * open and nothing was sent, and calls `sent` before it posts `POST /v1/charge`. On a link that
     * did not open, a sale that was never posted goes to the core; one that was is
     * [TillError.Unreachable], since its last reply may be lost. [pin] is wiped on every path.
     * Blocks: runs on the charge's own thread.
     */
    fun runCharge(priceMsat: Long, ref: String, pin: Pin?, card: () -> CardLink, online: (sent: () -> Unit) -> ChargeResult?): ChargeResult = try {
        when {
            localOnly(ref) -> local(priceMsat, ref, pin, card)
            else -> online { synchronized(lock) { postedRefs += ref } }
                ?: if (synchronized(lock) { ref in postedRefs }) ChargeResult.Failed(TillError.Unreachable)
                else local(priceMsat, ref, pin, card)
        }
    } finally {
        pin?.wipe()
    }

    /** Charged on this phone and held or threw, or listed signing by the core (a list that cannot be read leaves [localRefs] to decide). */
    private fun localOnly(ref: String): Boolean {
        if (synchronized(lock) { ref in localRefs }) return true
        val c = core() ?: return false
        return try { ref in c.signingRefs() } catch (e: Throwable) { false }
    }

    /** Whether the core still holds [ref] signing; a list that cannot be read counts as held. */
    private fun holds(c: OfflineCore, ref: String): Boolean = try { ref in c.signingRefs() } catch (e: Throwable) { true }

    private fun local(priceMsat: Long, ref: String, pin: Pin?, card: () -> CardLink): ChargeResult {
        val c = core()
        val kit = pairing()?.let(kits::read)
        if (c == null || kit == null) {
            // Nothing was asked of the card; a sale held from before stays held.
            val held = synchronized(lock) { ref in localRefs } || (c != null && holds(c, ref))
            return ChargeResult.Declined(ChargeErrorCode.OFFLINE_NOT_READY, if (c == null) offReason() ?: "" else "", 0, held = held, local = true)
        }
        synchronized(lock) { localRefs += ref }
        val started = now()
        localCharging = true
        val outcome: CoreCharge? = try {
            // The core takes the PIN as a String (an accepted limit of this slice); the Pin is wiped below.
            c.charge(card(), kit.answer, priceMsat, ref, pin?.withChars { String(it) } ?: "")
        } catch (e: Throwable) {
            null // the outcome is unknown, as for undecided: held, retried on the phone only
        } finally {
            localCharging = false
            pin?.wipe()
        }
        return when (outcome) {
            is CoreCharge.Paid -> {
                synchronized(lock) { localRefs -= ref }
                ChargeResult.Success(receipt(outcome, now() - started))
            }
            is CoreCharge.Refused -> {
                if (outcome.code in KIT_CODES) kitStale = true
                // A held sale answers its card's checks first (expiry, attestation, PIN) and stays held.
                val held = outcome.code == UNDECIDED || holds(c, ref)
                if (!held) synchronized(lock) { localRefs -= ref }
                ChargeResult.Declined(ChargeErrorCode.fromWire(outcome.code), detail(outcome), 0, outcome.suggestedMsat, held = held, local = true)
            }
            null -> ChargeResult.Declined(ChargeErrorCode.UNDECIDED, "the phone's charge failed; its outcome is unknown", 0, held = true, local = true)
        }
    }

    /** `wrong_pin`'s detail always carries the tries left, which the PIN screen reads. */
    private fun detail(r: CoreCharge.Refused): String =
        if (r.code == "wrong_pin" && r.triesLeft != null && parseTriesLeft(r.detail) == null) "wrong PIN; ${r.triesLeft} tries left" else r.detail

    private fun receipt(p: CoreCharge.Paid, ms: Long) = ChargeReceipt(
        saleId = 0, txid = p.txid, priceMsat = p.priceMsat, receivedMsat = p.tillMsat, overpaidMsat = p.overpayMsat,
        feesMsat = p.feesMsat, inputs = p.inputs, change = p.change, collectionPending = true, warnings = emptyList(),
        durationMs = ms, offline = true,
    )

    /**
     * Fetches this phone's kit when one is due: none kept for this pairing, the kept one 10 minutes
     * old, [force] (after a sync that synced charges), or after a local kit, pool or cap refusal. A
     * failed fetch keeps the old kit and waits a minute (unless forced). One at a time. Answers
     * whether a new kit was kept. Blocks.
     */
    fun refreshKit(force: Boolean = false): Boolean {
        val c = core() ?: return false
        val p = pairing() ?: return false
        if (!refreshing.compareAndSet(false, true)) return false
        try {
            val t = now()
            val kept = kits.read(p)
            val due = force || kitStale || kept == null || t - kept.fetchedAtMs !in 0 until KIT_MAX_AGE_MS
            if (!due || (!force && t < kitRetryAt)) return false
            val phone = c.phoneId()
            val r = till.offlineKit(phone)
            if (r is TillResult.Ok && kits.save(p, phone, r.value, t) != null) {
                kitStale = false
                kitRetryAt = 0
                kitError = null
                return true
            }
            kitError = (r as? TillResult.Err)?.let { describe(it.error) } ?: "the till's answer is not a kit for this phone"
        } catch (e: Throwable) {
            kitError = e.message ?: e.javaClass.simpleName
        } finally {
            refreshing.set(false)
        }
        kitRetryAt = now() + RETRY_MS
        return false
    }

    /**
     * Sends what waits on this phone to the till: `drain(50)`, `POST /v1/offline/charges`, then
     * `mark_synced` with a 200's answer, until nothing waits. Stops on an HTTP error, a round the
     * till took none of, or charges it does not know ([keepForTheirTill]: they wait for the till
     * they were taken for), and then waits a minute. One at a time. What it synced counts as
     * waiting until a status poll that started after it ([statusShown]), and is followed by a new
     * kit (its pool entries are spent). Answers how many charges it synced. Blocks.
     */
    fun sync(): Int {
        val c = core() ?: return 0
        if (pairing() == null || now() < syncRetryAt) return 0
        if (!syncing.compareAndSet(false, true)) return 0
        var synced = 0
        try {
            for (round in 1..MAX_ROUNDS) {
                val d = c.drain(BATCH)
                if (d.txids.isEmpty()) {
                    syncError = null
                    break
                }
                val r = till.postOfflineCharges(d.body)
                if (r !is TillResult.Ok) {
                    stopSync(describe((r as TillResult.Err).error))
                    break
                }
                val (answer, notTheirs) = keepForTheirTill(r.value)
                val report = c.markSynced(answer)
                synced += report.synced
                if (notTheirs > 0) {
                    // The next drain starts with them again: this till's answer would not change.
                    stopSync(
                        if (notTheirs == 1) "1 payment waits for the till it was taken for"
                        else "$notTheirs payments wait for the till they were taken for",
                    )
                    break
                }
                if (report.synced + report.refused == 0) {
                    stopSync("the till took none of ${d.txids.size} (it asked for them again later)")
                    break
                }
            }
        } catch (e: Throwable) {
            stopSync(e.message ?: e.javaClass.simpleName)
        } finally {
            syncing.set(false)
        }
        if (synced > 0) {
            synchronized(lock) {
                awaiting += synced
                syncSeq++
            }
            refreshKit(force = true)
        }
        return synced
    }

    private fun stopSync(why: String) {
        syncError = why
        syncRetryAt = now() + RETRY_MS
    }

    /** Taken as a status poll starts; given back to [statusShown] when it answers. */
    fun pollMark(): Long = synchronized(lock) { syncSeq }

    /**
     * A status poll that started at [mark] answered. When no sync finished since it started, the
     * till's queue in it includes every charge synced before: they stop counting as the phone's.
     */
    fun statusShown(mark: Long) {
        synchronized(lock) { if (mark == syncSeq) awaiting = 0 }
    }

    /** The phone's counts for the home screen. A core call: off the main thread. */
    fun counts(): PhoneCounts {
        val s = core()?.let { c -> try { c.stats().also { lastStats = it } } catch (e: Throwable) { lastStats } }
        val a = synchronized(lock) { awaiting }
        return PhoneCounts((s?.waiting ?: 0).toLong() + a, s?.signing ?: 0)
    }

    /** What Settings shows. Core calls: off the main thread. */
    fun view(): OfflineView {
        val c = core()
        val p = pairing()
        val off = if (c == null) offReason() else null
        return OfflineView(
            off = off,
            starting = c == null && off == null,
            paired = p != null,
            kit = p?.let(kits::read),
            stats = c?.let { try { it.stats() } catch (e: Throwable) { null } },
            refused = c?.let { try { it.drain(0).refused } catch (e: Throwable) { null } } ?: emptyList(),
            kitError = kitError,
            syncError = syncError,
        )
    }

    /** Deletes a refused charge's record (the card paid; the till would not take it). Blocks. */
    fun dismiss(txid: String): Boolean = try { core()?.dismiss(txid) ?: false } catch (e: Throwable) { false }

    companion object {
        /** A kit older than this is fetched again at the next poll. */
        const val KIT_MAX_AGE_MS = 10 * 60_000L
        /** After a failed kit fetch or a sync that stopped short. */
        const val RETRY_MS = 60_000L
        /** Charges per `POST /v1/offline/charges`: the till's most. */
        const val BATCH = 50
        /** Rounds per sync; what is left goes at the next one. */
        const val MAX_ROUNDS = 20
        private const val UNDECIDED = "undecided"
        /** Local refusals a new kit may answer differently. */
        private val KIT_CODES = setOf("offline_kit_expired", "offline_pool_exhausted", "offline_cap_reached")
        /**
         * The sync's refusals of a till that does not know the charge (docs/TILL-API.md): pool
         * entries it never issued to this phone, or notes and a mint of another federation.
         */
        private val NOT_THEIRS = setOf("pool_entry_unknown", "bad_note", "bad_charge")

        /**
         * [answerJson], the till's 200 to `POST /v1/offline/charges`, with each refusal in
         * [NOT_THEIRS] made a retry, and how many it made one. A charge the core took checks
         * against the federation of the kit it was taken with, and its pool entries stay that
         * kit's till's until their redeem window ends (the kit's expiry and a grace, 30 days by
         * default), so those refusals come from another till, paired since. The core would keep
         * them refused for good; they wait instead, and sync once the phone is paired with their
         * till again. `pool_entry_reused` and `pool_entry_expired` stay final: only the till that
         * issued the entries says them. An answer that is not JSON goes to the core as it came,
         * which refuses it.
         */
        internal fun keepForTheirTill(answerJson: String): Pair<String, Int> {
            val a = try { JSONObject(answerJson) } catch (e: JSONException) { return answerJson to 0 }
            val results = a.optJSONArray("results") ?: return answerJson to 0
            var kept = 0
            for (i in 0 until results.length()) {
                val r = results.optJSONObject(i) ?: continue
                if (r.optString("status") == "refused" && r.optString("code") in NOT_THEIRS && !r.optBoolean("retry")) {
                    r.put("retry", true)
                    kept++
                }
            }
            return (if (kept > 0) a.toString() else answerJson) to kept
        }

        private fun describe(e: TillError): String = when (e) {
            TillError.Unreachable -> "the till did not answer"
            TillError.Timeout -> "the till took too long"
            TillError.Interrupted -> "the connection broke off"
            is TillError.BadResponse -> e.detail
        }
    }
}
