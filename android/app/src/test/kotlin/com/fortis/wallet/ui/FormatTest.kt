package com.fortis.wallet.ui

import com.fortis.wallet.data.LN_CHAIN
import com.fortis.wallet.data.LN_XBT_CHAIN
import com.fortis.wallet.data.isLightningChain
import com.fortis.wallet.data.lnChainOf
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.wallet_ffi.LnChain

class FormatTest {
    @Test
    fun coin_of_ln_xbt_is_xbt() {
        assertEquals("ln-xbt", LN_XBT_CHAIN)
        assertEquals("XBT", chainUnit(LN_XBT_CHAIN))
        assertEquals("BTC", chainUnit(LN_CHAIN))
        assertEquals("BTC", chainUnit("btc"))
        assertEquals("XBT", chainUnit("xbt"))
    }

    @Test
    fun lightning_chains() {
        assertTrue(isLightningChain("ln"))
        assertTrue(isLightningChain("ln-xbt"))
        assertFalse(isLightningChain("btc"))
        assertFalse(isLightningChain("xbt"))
        assertEquals(LnChain.BTC, lnChainOf("ln"))
        assertEquals(LnChain.XBT, lnChainOf("ln-xbt"))
        assertNull(lnChainOf("btc"))
        assertNull(lnChainOf("xbt"))
    }
}
