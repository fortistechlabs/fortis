package com.fortis.wallet.ui

import com.fortis.wallet.data.LN_CHAIN
import com.fortis.wallet.data.LN_XBT_CHAIN
import com.fortis.wallet.ui.screens.chainRowOptions
import com.fortis.wallet.ui.screens.createLightningTabs
import org.junit.Assert.assertEquals
import org.junit.Test

class ChainRowOptionsTest {
    private val both = listOf(LN_CHAIN, LN_XBT_CHAIN)

    @Test
    fun createLightningOffersOnlyBtcAndXbt() {
        // "Create a Lightning wallet": two choices, not four.
        assertEquals(
            listOf(LN_CHAIN to "BTC", LN_XBT_CHAIN to "XBT"),
            chainRowOptions(both, lightningOnly = true, lnLabel = "Lightning", lnXbtLabel = "Lightning (XBT)"),
        )
    }

    @Test
    fun createLightningOnlyOffersWhatIsAvailable() {
        assertEquals(
            listOf(LN_CHAIN to "BTC"),
            chainRowOptions(listOf(LN_CHAIN), lightningOnly = true, lnLabel = "Lightning", lnXbtLabel = "Lightning (XBT)"),
        )
        assertEquals(
            listOf(LN_XBT_CHAIN to "XBT"),
            chainRowOptions(listOf(LN_XBT_CHAIN), lightningOnly = true, lnLabel = "Lightning", lnXbtLabel = "Lightning (XBT)"),
        )
    }

    @Test
    fun mixedFlowKeepsTheOnChainTabsAndNamedLightningTabs() {
        assertEquals(
            listOf("xbt" to "XBT", "btc" to "BTC", LN_CHAIN to "Lightning", LN_XBT_CHAIN to "Lightning (XBT)"),
            chainRowOptions(both, lightningOnly = false, lnLabel = "Lightning", lnXbtLabel = "Lightning (XBT)"),
        )
    }

    @Test
    fun onChainOnlyWhenNoLightningIsOffered() {
        assertEquals(
            listOf("xbt" to "XBT", "btc" to "BTC"),
            chainRowOptions(emptyList(), lightningOnly = false, lnLabel = "Lightning", lnXbtLabel = "Lightning (XBT)"),
        )
    }

    @Test
    fun plainCreateHasNoLightningTabsButTheLightningFlowDoes() {
        assertEquals(emptyList<String>(), createLightningTabs(lightningFlow = false, choices = both))
        assertEquals(both, createLightningTabs(lightningFlow = true, choices = both))
        // ...so a plain create shows exactly XBT and BTC.
        assertEquals(
            listOf("xbt" to "XBT", "btc" to "BTC"),
            chainRowOptions(createLightningTabs(false, both), lightningOnly = false, lnLabel = "L", lnXbtLabel = "LX"),
        )
    }
}
