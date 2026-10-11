package com.fortis.wallet.ui

import com.fortis.wallet.ui.screens.amountError
import com.fortis.wallet.ui.screens.btcLeg
import com.fortis.wallet.ui.screens.commission
import com.fortis.wallet.ui.screens.youReceive
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test

class SwapMathTest {
    @Test
    fun you_receive_converts_at_the_price() {
        assertEquals(1_120_000L, youReceive(100_000_000, "xbt", 1_120_000))
        assertEquals(100_000_000L, youReceive(1_120_000, "btc", 1_120_000))
    }

    @Test
    fun you_receive_rounds_down_like_the_edge() {
        // 7 M XBT sat at 1 008 980 → 70 628.6 BTC sat → 70 628
        assertEquals(70_628L, youReceive(7_000_000, "xbt", 1_008_980))
        // 1 BTC sat at 1 120 000 → 89.28… XBT sat → 89
        assertEquals(89L, youReceive(1, "btc", 1_120_000))
    }

    @Test
    fun you_receive_is_zero_without_a_price() {
        assertEquals(0L, youReceive(100_000, "btc", 0))
    }

    @Test
    fun commission_is_half_a_percent_rounded_down() {
        assertEquals(5_000L, commission(1_000_000))
        assertEquals(0L, commission(199))
        assertEquals(1L, commission(200))
    }

    @Test
    fun btc_leg_is_the_btc_side_either_way() {
        assertEquals(200_000L, btcLeg(200_000, "btc", 1_120_000))
        assertEquals(1_120_000L, btcLeg(100_000_000, "xbt", 1_120_000))
    }

    @Test
    fun amount_error_below_minimum_and_above_cap() {
        assertNotNull(amountError(79_999, 80_000, 1_000_000))
        assertNull(amountError(80_000, 80_000, 1_000_000))
        assertNull(amountError(1_000_000, 80_000, 1_000_000))
        assertNotNull(amountError(1_000_001, 80_000, 1_000_000))
    }
}
