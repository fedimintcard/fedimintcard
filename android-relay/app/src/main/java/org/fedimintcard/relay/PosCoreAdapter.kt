package org.fedimintcard.relay

import android.util.Log
import org.fedimintcard.poscore.ApduTransport
import org.fedimintcard.poscore.ChargeOutcome
import org.fedimintcard.poscore.PosCore
import org.fedimintcard.poscore.SlotMsat
import org.fedimintcard.poscore.TransportException
import java.io.File
import java.util.concurrent.Executors

/**
 * The one file that imports the generated bindings (`org.fedimintcard.poscore`, built from
 * host/pos-core by the Gradle step): [OfflineCore] over the core's [PosCore], with the core's
 * unsigned numbers as the app's Longs and Ints. Loading the generated classes loads the native
 * library through JNA, so unit tests never touch this file and fake [OfflineCore] instead.
 */
class PosCoreAdapter private constructor(private val core: PosCore) : OfflineCore {
    override fun phoneId(): String = core.phoneId()

    override fun charge(card: CardLink, kitJson: String, priceMsat: Long, clientRef: String, pin: String): CoreCharge =
        when (val o = core.charge(CardTransport(card), kitJson, priceMsat.toULong(), clientRef, pin)) {
            is ChargeOutcome.Paid -> CoreCharge.Paid(
                o.txid, o.priceMsat.toLong(), o.tillMsat.toLong(), o.overpayMsat.toLong(), o.feesMsat.toLong(),
                o.inputs.map(::note), o.change.map(::note), o.replayed,
            )
            is ChargeOutcome.Refused -> CoreCharge.Refused(o.code, o.detail, o.suggestedMsat?.toLong(), o.triesLeft?.toInt())
        }

    override fun stats(): CoreStats = core.stats().let {
        CoreStats(it.waiting.toInt(), it.waitingMsat.toLong(), it.signing.toInt(), it.refused.toInt(), it.synced.toInt())
    }

    override fun signingRefs(): List<String> = core.signingRefs()

    override fun drain(max: Int): CoreDrain = core.drain(max.toUInt()).let { d ->
        CoreDrain(d.body, d.txids, d.refused.map { RefusedEntry(it.txid, it.code, it.priceMsat.toLong(), it.tsUnixMs.toLong()) })
    }

    override fun markSynced(answerJson: String): CoreSync =
        core.markSynced(answerJson).let { CoreSync(it.synced.toInt(), it.refused.toInt(), it.retry.toInt()) }

    override fun dismiss(txid: String): Boolean = core.dismiss(txid)

    companion object {
        /** Loads the library and opens the store in [dir]; throws whatever fails (the reason offline is off). */
        fun open(dir: File): OfflineCore = PosCoreAdapter(PosCore.open(dir.path))

        private fun note(s: SlotMsat) = Note(s.slot.toInt(), s.msat.toLong())
    }
}

/** The card on the phone as the core's transport: any failure is the card leaving the field. */
private class CardTransport(private val card: CardLink) : ApduTransport {
    override fun transmit(apdu: ByteArray): ByteArray = try {
        card.transceive(apdu)
    } catch (e: Exception) {
        throw TransportException.Lost(e.javaClass.simpleName)
    }
}

/**
 * The phone's offline core for the whole process: opened once, on the core's own thread, in
 * `noBackupFilesDir/pos-core` (credential-encrypted storage, out of every backup), and never closed,
 * since a second open of the store fails on its lock. A failure to load the library or open the
 * store is kept as the reason offline payments are off; online charges are unchanged.
 */
object OfflineRuntime {
    private const val TAG = "PosCore"
    private val thread = Executors.newSingleThreadExecutor { r -> Thread(r, "pos-core").apply { isDaemon = true } }
    private var started = false

    @Volatile var core: OfflineCore? = null
        private set

    @Volatile var failure: String? = null
        private set

    /** Opens the store under [noBackupDir] once per process, then runs [then] on the core's thread. */
    @Synchronized
    fun start(noBackupDir: File, then: () -> Unit) {
        if (!started) {
            started = true
            post {
                try {
                    core = PosCoreAdapter.open(File(noBackupDir, "pos-core"))
                } catch (t: Throwable) {
                    failure = "${t.javaClass.simpleName}: ${t.message}"
                    Log.w(TAG, "offline payments are off: $failure")
                }
            }
        }
        post(then)
    }

    /** Runs [task] on the core's thread, after what is queued there. A failure is logged, never thrown. */
    fun post(task: () -> Unit) {
        thread.execute {
            try {
                task()
            } catch (t: Throwable) {
                Log.w(TAG, "core task failed: ${t.javaClass.simpleName}")
            }
        }
    }
}
