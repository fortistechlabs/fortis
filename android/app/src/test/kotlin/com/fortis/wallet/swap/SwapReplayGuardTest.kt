package com.fortis.wallet.swap

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Test

class SwapReplayGuardTest {
    @Test
    fun aBtcFundingCarriesAHundredFreshBytes() {
        val a = swapFundingGuard("btc")!!
        val b = swapFundingGuard("btc")!!
        assertEquals(100, a.size)
        assertFalse("fresh each time", a.contentEquals(b))
    }

    /** XBT spends are bound to XBT by their SIGHASH_UNIFIED signatures. */
    @Test
    fun anXbtFundingNeedsNone() {
        assertNull(swapFundingGuard("xbt"))
    }
}
