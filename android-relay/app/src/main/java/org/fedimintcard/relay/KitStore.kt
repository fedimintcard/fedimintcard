package org.fedimintcard.relay

import org.json.JSONException
import org.json.JSONObject
import java.io.File
import java.io.FileOutputStream
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.security.MessageDigest

/**
 * The till's offline kit for this phone (`GET /v1/offline/kit`, docs/POS-OFFLINE.md § Android), kept
 * in [dir] (the app's no-backup storage) as `pos-kit.json`: `{"pairing_sha256", "fetched_at_ms",
 * "answer"}`, the answer as the till gave it, which the core takes whole. Written to a temporary
 * file, synced and renamed over the old one. Plain JVM: no Android.
 */
class KitStore(private val dir: File) {
    /** A kit the till answered, with what Settings shows of it. */
    data class Kit(
        val answer: String,
        val fetchedAtMs: Long,
        val kitId: String,
        val issuedAtMs: Long,
        val expiresAtMs: Long,
        val capMsat: Long,
        val capCount: Long,
    )

    private val file get() = File(dir, FILE)

    /** The kit kept for [pairing] ([pairingId]), or null: none, another pairing's, or a file that does not read. */
    @Synchronized
    fun read(pairing: String): Kit? = try {
        val o = JSONObject(file.readText())
        if (o.optString("pairing_sha256") != pairing) null else parse(o.getString("answer"), null, o.getLong("fetched_at_ms"))
    } catch (e: Exception) {
        null
    }

    /**
     * Keeps [answer] for [pairing] when it is a kit (`ok:true`) for [phoneId] with an expiry, and
     * answers it; else answers null and the kit kept before stays.
     */
    @Synchronized
    fun save(pairing: String, phoneId: String, answer: String, nowMs: Long): Kit? {
        val kit = parse(answer, phoneId, nowMs) ?: return null
        val o = JSONObject().put("pairing_sha256", pairing).put("fetched_at_ms", nowMs).put("answer", answer)
        dir.mkdirs()
        val tmp = File(dir, "$FILE.tmp")
        FileOutputStream(tmp).use { it.write(o.toString().toByteArray(Charsets.UTF_8)); it.fd.sync() }
        Files.move(tmp.toPath(), file.toPath(), StandardCopyOption.ATOMIC_MOVE, StandardCopyOption.REPLACE_EXISTING)
        return kit
    }

    companion object {
        const val FILE = "pos-kit.json"

        /** [answer] as a kit, or null when it is not `ok:true`, is for another phone, or has no expiry. */
        internal fun parse(answer: String, phoneId: String?, fetchedAtMs: Long): Kit? = try {
            val o = JSONObject(answer)
            val k = if (o.optBoolean("ok", false)) o.optJSONObject("kit") else null
            when {
                k == null -> null
                phoneId != null && k.optString("phone_id") != phoneId -> null
                !k.has("expires_at_ms") || k.isNull("expires_at_ms") -> null
                else -> Kit(
                    answer, fetchedAtMs, k.optString("kit_id"), k.optLong("issued_at_ms"), k.getLong("expires_at_ms"),
                    k.optLong("cap_msat"), k.optLong("cap_count"),
                )
            }
        } catch (e: JSONException) {
            null
        }
    }
}

/** The pairing a kit belongs to: SHA-256 of the till's URL and token, in hex (the token is never written). */
fun pairingId(p: Pairing): String =
    MessageDigest.getInstance("SHA-256").digest("${p.url}\n${p.token}".toByteArray(Charsets.UTF_8)).let(::bytesToHex)
