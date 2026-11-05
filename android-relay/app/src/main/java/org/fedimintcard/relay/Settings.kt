package org.fedimintcard.relay

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.net.URI
import java.security.KeyStore
import java.util.Base64
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

const val DEFAULT_TILL_URL = "http://127.0.0.1:9877"

/** Returns a clean base URL (no trailing slash) or null if the input is not a usable http(s) URL. */
fun normalizeBaseUrl(input: String): String? {
    var s = input.trim()
    if (s.isEmpty()) return null
    if (!s.contains("://")) s = "http://$s"
    val u = try { URI(s) } catch (e: Exception) { return null }
    if (u.scheme != "http" && u.scheme != "https") return null
    if (u.host.isNullOrEmpty() || u.query != null || u.fragment != null || u.userInfo != null) return null
    return s.trimEnd('/')
}

const val PAIR_SCHEME = "fedimintcard-till:"

/** A till this phone is paired with: where it is, and the POS token it wants as a bearer. */
data class Pairing(val url: String, val token: String) {
    override fun toString() = "Pairing(url=$url, token=<hidden>)" // never log the token
}

/**
 * Parses the text `till pair` prints (and puts in its QR code):
 * `fedimintcard-till:http://<host>:9877?t=<64 hex>`. Null if it is anything else.
 */
fun parsePairing(input: String): Pairing? {
    val s = input.trim()
    if (!s.startsWith(PAIR_SCHEME)) return null
    val rest = s.removePrefix(PAIR_SCHEME)
    val q = rest.lastIndexOf("?t=")
    if (q < 0) return null
    val token = rest.substring(q + 3)
    if (token.length != 64 || !token.all { it in '0'..'9' || it in 'a'..'f' || it in 'A'..'F' }) return null
    val url = normalizeBaseUrl(rest.substring(0, q)) ?: return null
    if (!rest.substring(0, q).contains("://")) return null
    return Pairing(url, token.lowercase())
}

class Settings(ctx: Context) {
    private val prefs = ctx.getSharedPreferences("pos", Context.MODE_PRIVATE)
    var baseUrl: String
        get() = prefs.getString("till_url", null)?.let { normalizeBaseUrl(it) } ?: DEFAULT_TILL_URL
        set(v) { prefs.edit().putString("till_url", v).apply() }

    /**
     * The paired till, kept encrypted under a Keystore key that never leaves the phone. Null when
     * unpaired: then charges use the USB relay path (no token, no link).
     */
    var pairing: Pairing?
        get() {
            val blob = prefs.getString("pairing", null) ?: return null
            val plain = try { KeystoreBox.open(blob) } catch (e: Exception) { return null } ?: return null
            val i = plain.indexOf('\n')
            if (i < 0) return null
            return Pairing(plain.substring(0, i), plain.substring(i + 1))
        }
        set(v) {
            if (v == null) prefs.edit().remove("pairing").apply()
            else prefs.edit().putString("pairing", KeystoreBox.seal("${v.url}\n${v.token}")).putString("till_url", v.url).apply()
        }
}

/** AES-GCM with a key in the Android Keystore. Blob = base64(iv || ciphertext). */
private object KeystoreBox {
    private const val ALIAS = "till-pairing"

    private fun key(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getKey(ALIAS, null) as? SecretKey)?.let { return it }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build()
        )
        return gen.generateKey()
    }

    fun seal(plain: String): String {
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, key())
        return Base64.getEncoder().encodeToString(c.iv + c.doFinal(plain.toByteArray(Charsets.UTF_8)))
    }

    fun open(blob: String): String? {
        val b = Base64.getDecoder().decode(blob)
        if (b.size < 13) return null
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, b, 0, 12))
        return String(c.doFinal(b, 12, b.size - 12), Charsets.UTF_8)
    }
}
