package com.fortis.wallet.ui

import com.fortis.wallet.R
import com.fortis.wallet.data.LN_CHAIN
import com.fortis.wallet.data.LN_XBT_CHAIN
import com.fortis.wallet.ui.screens.offerHelpRes
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

/** Phoenix is a BTC wallet: the reusable-address help must not point XBT users at it. */
class OfferHelpTest {
    @Test
    fun xbtGetsItsOwnHelpTextAndBtcKeepsTheOriginal() {
        assertEquals(R.string.ln_offer_help_xbt, offerHelpRes(LN_XBT_CHAIN))
        assertEquals(R.string.ln_offer_help, offerHelpRes(LN_CHAIN))
    }

    @Test
    fun theXbtHelpDoesNotMentionPhoenixButStillExplainsTheAddress() {
        val xml = File("src/main/res/values/strings.xml").readText()
        fun text(name: String) = Regex("""<string name="$name"[^>]*>(.*?)</string>""", RegexOption.DOT_MATCHES_ALL)
            .find(xml)!!.groupValues[1]
        val xbt = text("ln_offer_help_xbt")
        assertFalse(xbt, xbt.contains("Phoenix", ignoreCase = true))
        assertTrue(xbt, "BOLT12" in xbt && "any amount" in xbt)
        // BTC's text is unchanged.
        assertTrue(text("ln_offer_help").contains("Phoenix"))
    }
}
