package org.fedimintcard.relay

import org.junit.Assert.*
import org.junit.Test
import org.w3c.dom.Element
import java.io.File
import javax.xml.parsers.DocumentBuilderFactory

/**
 * The phone's offline store (noBackupFilesDir) never leaves the phone: a store restored from a backup
 * or moved to a new phone pays the till's pool entries again (`pool_entry_reused`), and the payments
 * in it are lost (docs/POS-OFFLINE.md § The phone's store).
 */
class BackupRulesTest {
    private val android = "http://schemas.android.com/apk/res/android"

    /** A file of the app module, from the module (Gradle's test directory) or the project root. */
    private fun source(path: String): File = listOf(File(path), File("app/$path")).first { it.exists() }

    private fun xml(path: String) =
        DocumentBuilderFactory.newInstance().apply { isNamespaceAware = true }.newDocumentBuilder().parse(source(path)).documentElement

    @Test fun theStoreIsLeftOutOfCloudBackupAndDeviceTransfer() {
        val rules = xml("src/main/res/xml/data_extraction_rules.xml")
        assertEquals("data-extraction-rules", rules.tagName)
        for (section in listOf("cloud-backup", "device-transfer")) {
            val s = rules.getElementsByTagName(section)
            assertEquals(section, 1, s.length)
            val e = s.item(0) as Element
            val excludes = (0 until e.getElementsByTagName("exclude").length).map { e.getElementsByTagName("exclude").item(it) as Element }
            assertTrue(section, excludes.any { it.getAttribute("domain") == "root" && it.getAttribute("path") == "no_backup/" })
            assertEquals("nothing is let back in: $section", 0, e.getElementsByTagName("include").length)
        }
    }

    @Test fun olderAndroidsLeaveItOutToo() {
        val rules = xml("src/main/res/xml/backup_rules.xml")
        assertEquals("full-backup-content", rules.tagName)
        val excludes = (0 until rules.getElementsByTagName("exclude").length).map { rules.getElementsByTagName("exclude").item(it) as Element }
        assertTrue(excludes.any { it.getAttribute("domain") == "root" && it.getAttribute("path") == "no_backup/" })
        assertEquals(0, rules.getElementsByTagName("include").length)
    }

    @Test fun theManifestUsesTheRulesAndNoBackup() {
        val app = xml("src/main/AndroidManifest.xml").getElementsByTagName("application").item(0) as Element
        assertEquals("@xml/data_extraction_rules", app.getAttributeNS(android, "dataExtractionRules"))
        assertEquals("@xml/backup_rules", app.getAttributeNS(android, "fullBackupContent"))
        assertEquals("false", app.getAttributeNS(android, "allowBackup"))
    }
}
