package com.fortis.wallet

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.w3c.dom.Element
import java.io.File
import javax.xml.parsers.DocumentBuilderFactory

/**
 * Nothing of the app's data leaves the phone through Android's backup or
 * device-to-device transfer. `allowBackup="false"` covers cloud backup only on
 * Android 12+; transfer to a new phone needs `dataExtractionRules`. The data
 * holds sealed seeds (password mode can be attacked offline), swap owner
 * secrets and install tokens.
 */
class BackupRulesTest {
    private val android = "http://schemas.android.com/apk/res/android"

    private fun parse(path: String): Element = DocumentBuilderFactory.newInstance().apply { isNamespaceAware = true }
        .newDocumentBuilder().parse(File(path)).documentElement

    private fun Element.children(tag: String): List<Element> =
        getElementsByTagName(tag).let { n -> (0 until n.length).map { n.item(it) as Element } }

    @Test
    fun the_manifest_turns_backup_off_and_names_the_extraction_rules() {
        val app = parse("src/main/AndroidManifest.xml").children("application").single()
        assertEquals("false", app.getAttributeNS(android, "allowBackup"))
        assertEquals("@xml/data_extraction_rules", app.getAttributeNS(android, "dataExtractionRules"))
    }

    @Test
    fun every_domain_is_excluded_from_cloud_backup_and_device_transfer() {
        val rules = parse("src/main/res/xml/data_extraction_rules.xml")
        val domains = setOf("root", "file", "database", "sharedpref", "external")
        for (section in listOf("cloud-backup", "device-transfer")) {
            val s = rules.children(section).single()
            assertTrue("$section includes nothing", s.children("include").isEmpty())
            val excluded = s.children("exclude").filter { it.getAttribute("path") == "." }.map { it.getAttribute("domain") }.toSet()
            assertEquals(section, domains, excluded)
        }
    }
}
