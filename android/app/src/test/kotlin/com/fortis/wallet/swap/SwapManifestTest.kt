package com.fortis.wallet.swap

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.w3c.dom.Element
import java.io.File
import javax.xml.parsers.DocumentBuilderFactory

/** The swap service, boot receiver and permissions are declared as Android needs them. */
class SwapManifestTest {
    private val android = "http://schemas.android.com/apk/res/android"

    private val manifest: Element by lazy {
        DocumentBuilderFactory.newInstance().apply { isNamespaceAware = true }
            .newDocumentBuilder().parse(File("src/main/AndroidManifest.xml")).documentElement
    }

    private fun elements(tag: String): List<Element> =
        manifest.getElementsByTagName(tag).let { n -> (0 until n.length).map { n.item(it) as Element } }

    private fun Element.attr(name: String): String = getAttributeNS(android, name)

    @Test
    fun declares_the_swap_permissions() {
        val perms = elements("uses-permission").map { it.attr("name") }.toSet()
        for (p in listOf(
            "android.permission.FOREGROUND_SERVICE",
            "android.permission.FOREGROUND_SERVICE_DATA_SYNC",
            "android.permission.POST_NOTIFICATIONS",
            "android.permission.RECEIVE_BOOT_COMPLETED",
        )) assertTrue("missing $p", p in perms)
    }

    @Test
    fun declares_the_data_sync_service() {
        val s = elements("service").singleOrNull { it.attr("name") == ".swap.SwapService" }
            ?: error("no .swap.SwapService")
        assertEquals("dataSync", s.attr("foregroundServiceType"))
        assertEquals("false", s.attr("exported"))
    }

    @Test
    fun declares_the_boot_receiver() {
        val r = elements("receiver").singleOrNull { it.attr("name") == ".swap.BootReceiver" }
            ?: error("no .swap.BootReceiver")
        val actions = r.getElementsByTagName("action").let { n -> (0 until n.length).map { (n.item(it) as Element).attr("name") } }
        assertTrue(actions.toString(), "android.intent.action.BOOT_COMPLETED" in actions)
    }
}
