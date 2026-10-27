package org.fedimintcard.relay

import android.content.Context
import java.net.URI

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

class Settings(ctx: Context) {
    private val prefs = ctx.getSharedPreferences("pos", Context.MODE_PRIVATE)
    var baseUrl: String
        get() = prefs.getString("till_url", null)?.let { normalizeBaseUrl(it) } ?: DEFAULT_TILL_URL
        set(v) { prefs.edit().putString("till_url", v).apply() }
}
