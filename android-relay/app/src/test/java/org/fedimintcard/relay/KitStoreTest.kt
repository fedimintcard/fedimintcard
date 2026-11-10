package org.fedimintcard.relay

import org.junit.Assert.*
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

class KitStoreTest {
    @get:Rule val tmp = TemporaryFolder()
    private val phone = "0123456789abcdef0123456789abcdef"
    private lateinit var kits: KitStore

    private fun answer(phone: String = this.phone, expires: String = "9000000000", ok: Boolean = true) =
        """{"ok":$ok,"kit":{"version":1,"kit_id":"k1","phone_id":"$phone","issued_at_ms":5,"expires_at_ms":$expires,"cap_msat":100000000,"cap_count":200,"pool":[]}}"""

    @Before fun up() { kits = KitStore(tmp.root) }

    @Test fun aKitRoundTripsAsTheTillAnsweredIt() {
        val saved = kits.save("p1", phone, answer(), 1_234)!!
        assertEquals(KitStore.Kit(answer(), 1_234, "k1", 5, 9_000_000_000, 100_000_000, 200), saved)
        assertEquals(saved, kits.read("p1"))
        assertEquals(saved, KitStore(tmp.root).read("p1")) // from the file, as after a restart
        assertFalse(File(tmp.root, "${KitStore.FILE}.tmp").exists())
    }

    @Test fun anotherPairingsKitReadsAsNone() {
        kits.save("p1", phone, answer(), 1)
        assertNull(kits.read("p2"))
    }

    @Test fun aMissingOrCorruptFileReadsAsNone() {
        assertNull(kits.read("p1"))
        for (junk in listOf("", "{", "[]", """{"pairing_sha256":"p1"}""", """{"pairing_sha256":"p1","fetched_at_ms":1,"answer":"nope"}""",
            """{"pairing_sha256":"p1","fetched_at_ms":"x","answer":${org.json.JSONObject.quote(answer())}}""")) {
            File(tmp.root, KitStore.FILE).writeText(junk)
            assertNull(junk, kits.read("p1"))
        }
    }

    @Test fun anAnswerThatIsNotAKitForThisPhoneIsNotSavedAndTheOldOneStays() {
        val kept = kits.save("p1", phone, answer(), 1)!!
        for (bad in listOf(
            answer(ok = false), answer(phone = "f".repeat(32)), answer(expires = "null"),
            """{"ok":true,"kit":{"phone_id":"$phone"}}""", """{"ok":true}""", """{"ok":false,"error":"offline_not_ready"}""", "<html>",
        )) {
            assertNull(bad, kits.save("p1", phone, bad, 2))
            assertEquals(bad, kept, kits.read("p1"))
        }
    }

    @Test fun thePairingIdIsAHashOfTheTillAndItsToken() {
        val t = "ab".repeat(32)
        val id = pairingId(Pairing("http://10.0.0.2:9877", t))
        assertTrue(id.matches(Regex("[0-9a-f]{64}")))
        assertFalse(id.contains(t))
        assertEquals(id, pairingId(Pairing("http://10.0.0.2:9877", t)))
        assertNotEquals(id, pairingId(Pairing("http://10.0.0.3:9877", t)))
        assertNotEquals(id, pairingId(Pairing("http://10.0.0.2:9877", "cd".repeat(32))))
    }
}
