package org.fedimintcard.relay

import org.junit.Assert.*
import org.junit.Test

class SettingsTest {
    @Test fun acceptsAndTrims() {
        assertEquals("http://127.0.0.1:9877", normalizeBaseUrl("  http://127.0.0.1:9877/ "))
        assertEquals("https://till.example.com", normalizeBaseUrl("https://till.example.com//"))
    }
    @Test fun addsSchemeWhenMissing() = assertEquals("http://192.168.1.5:9877", normalizeBaseUrl("192.168.1.5:9877"))
    @Test fun rejectsGarbage() {
        for (s in listOf("", "   ", "ftp://x", "http://", "http://a b", "http://h/?q=1", "http://h/#f", "http://u:p@h")) assertNull(s, normalizeBaseUrl(s))
    }
}
