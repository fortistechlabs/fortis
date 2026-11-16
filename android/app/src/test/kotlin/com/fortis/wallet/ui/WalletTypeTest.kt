package com.fortis.wallet.ui

import com.fortis.wallet.R
import com.fortis.wallet.data.LN_CHAIN
import com.fortis.wallet.data.LN_XBT_CHAIN
import com.fortis.wallet.ui.screens.walletTypeHintRes
import com.fortis.wallet.ui.screens.walletTypeSummaryRes
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

/**
 * The create / restore screens must say plainly which wallet the selected tab makes: on-chain or
 * Lightning, BTC or XBT. (A tester created the wrong kind because "BTC | XBT" alone did not say it.)
 */
class WalletTypeTest {
    @Test
    fun everyWalletTypeHasItsOwnSummary() {
        val ids = listOf("xbt", "btc", LN_CHAIN, LN_XBT_CHAIN).map(::walletTypeSummaryRes)
        assertEquals("one summary per wallet type: $ids", 4, ids.toSet().size)
        assertEquals(R.string.create_type_ln_xbt, walletTypeSummaryRes(LN_XBT_CHAIN))
        assertEquals(R.string.create_type_ln, walletTypeSummaryRes(LN_CHAIN))
        assertEquals(R.string.create_type_xbt, walletTypeSummaryRes("xbt"))
        assertEquals(R.string.create_type_btc, walletTypeSummaryRes("btc"))
    }

    @Test
    fun theLightningFlowGetsItsOwnHint() {
        assertNotEquals(walletTypeHintRes(lightningOnly = true), walletTypeHintRes(lightningOnly = false))
    }

    @Test
    fun theEnglishWordingNamesTheKindAndTheCoin() {
        val s = strings()
        fun text(name: String) = s.getValue(name)
        assertTrue(text("create_type_ln_xbt"), "Lightning" in text("create_type_ln_xbt") && "XBT" in text("create_type_ln_xbt"))
        assertTrue(text("create_type_ln"), "Lightning" in text("create_type_ln") && "BTC" in text("create_type_ln"))
        assertTrue(text("create_type_xbt"), "on-chain" in text("create_type_xbt") && "XBT" in text("create_type_xbt"))
        assertTrue(text("create_type_btc"), "on-chain" in text("create_type_btc") && "BTC" in text("create_type_btc"))
        // Both hints say the choice is permanent, so it is picked on purpose.
        for (h in listOf("wallet_type_hint_onchain", "wallet_type_hint_lightning")) {
            assertTrue(h, "can't be changed" in text(h) || "cannot be changed" in text(h))
        }
    }

    /** The default (English) strings.xml: unit tests run with the module directory as the cwd. */
    private fun strings(): Map<String, String> {
        val xml = File("src/main/res/values/strings.xml").readText()
        return Regex("""<string name="([^"]+)"[^>]*>(.*?)</string>""", RegexOption.DOT_MATCHES_ALL)
            .findAll(xml).associate { it.groupValues[1] to it.groupValues[2].replace("\\'", "'") }
    }
}
