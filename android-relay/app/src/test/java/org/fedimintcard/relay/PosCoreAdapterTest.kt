package org.fedimintcard.relay

import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

/**
 * [PosCoreAdapter] against the real core: the host build of host/pos-core that the Gradle step
 * makes with the bindings (so this also proves they load together, UniFFI's checksums included),
 * called through JNA as on the phone. No card: a charge stops at the kit, before any APDU.
 */
class PosCoreAdapterTest {
    @get:Rule val tmp = TemporaryFolder()
    private val phone = "00112233445566778899aabbccddeeff"

    /** A store whose phone id is known: the core keeps the one it finds. */
    private fun store(): File = tmp.newFolder().also { File(it, "phone-id").writeText(phone) }

    @Test fun theStoreOpensOnceAndAnswersEmpty() {
        val dir = store()
        val core = PosCoreAdapter.open(dir)
        assertEquals(phone, core.phoneId())
        assertEquals(CoreStats(), core.stats())
        assertEquals(emptyList<String>(), core.signingRefs())
        val d = core.drain(50)
        assertEquals(emptyList<String>(), d.txids); assertEquals(emptyList<RefusedEntry>(), d.refused)
        val body = JSONObject(d.body)
        assertEquals(phone, body.getString("phone_id")); assertEquals(0, body.getJSONArray("charges").length())
        assertEquals(CoreSync(0, 0, 0), core.markSynced("""{"ok":true,"results":[]}"""))
        assertFalse(core.dismiss("ab".repeat(32)))
        // A second open of the store fails on its lock: hence one core per process.
        try {
            PosCoreAdapter.open(dir)
            fail("opened twice")
        } catch (e: Exception) {
            assertTrue(e.javaClass.name, e.javaClass.name.contains("PosCoreException"))
        }
    }

    @Test fun theTillsRefusalOfASyncIsAnError() {
        val core = PosCoreAdapter.open(store())
        try {
            core.markSynced("""{"ok":false,"error":"offline_not_ready"}""")
            fail("taken")
        } catch (e: Exception) {
            assertTrue(e.message, e.message!!.contains("offline_not_ready"))
        }
    }

    @Test fun aKitThePhoneCannotUseIsRefusedBeforeTheCardHearsAnything() {
        val core = PosCoreAdapter.open(store())
        var asked = 0
        val card = CardLink { asked++; byteArrayOf(0x90.toByte(), 0) }
        for (kit in listOf("{}", "not json", """{"ok":true,"kit":{"version":1,"phone_id":"ffffffffffffffffffffffffffffffff"}}""")) {
            val r = core.charge(card, kit, 5_000, "sale-1", "1234") as CoreCharge.Refused
            assertEquals(kit, "offline_not_ready", r.code)
            assertNull(r.suggestedMsat); assertNull(r.triesLeft)
        }
        assertEquals(0, asked)
        assertEquals(CoreStats(), core.stats())
    }
}
