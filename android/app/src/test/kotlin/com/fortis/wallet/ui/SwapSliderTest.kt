package com.fortis.wallet.ui

import com.fortis.wallet.ui.screens.amountAtFraction
import com.fortis.wallet.ui.screens.commission
import com.fortis.wallet.ui.screens.fractionOf
import com.fortis.wallet.ui.screens.maxSwapSat
import com.fortis.wallet.ui.screens.swapFeeReserve
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class SwapSliderTest {
    @Test
    fun maxLeavesRoomForTheMinimumCommissionAndTheFee() {
        // 30 000 in the wallet, 1 000 reserved for the network fee: the 400 sat
        // floor applies, so 28 600 is the most that can be swapped.
        assertEquals(28_600L, maxSwapSat(30_000, 1_000))
    }

    @Test
    fun maxLeavesRoomForThePercentageCommission() {
        val max = maxSwapSat(5_000_000, 2_000)
        assert(max + commission(max) + 2_000 <= 5_000_000)
        assert(max + 1 + commission(max + 1) + 2_000 > 5_000_000)
    }

    @Test
    fun nothingToSwapWhenTheBalanceCantCoverTheCosts() {
        assertEquals(0L, maxSwapSat(1_200, 1_000))
        assertEquals(0L, maxSwapSat(0, 0))
    }

    @Test
    fun theFeeReserveCoversEveryInputAndThreeOutputs() {
        // 2 inputs at 3 sat/vB: (11 + 2×68 + 43 + 31 + 31) × 3
        assertEquals(756L, swapFeeReserve(utxoCount = 2, feerateSatVb = 3, chain = "xbt"))
    }

    @Test
    fun aBtcFundingAlsoPaysForItsReplayGuard() {
        // + the 100-byte OP_RETURN output: 8 + 1 + 103 = 112 vB.
        assertEquals(756L + 112 * 3, swapFeeReserve(utxoCount = 2, feerateSatVb = 3, chain = "btc"))
    }

    @Test
    fun sliderAndAmountTrackEachOther() {
        assertEquals(0.5f, fractionOf(50_000, 100_000), 0.0001f)
        assertEquals(1f, fractionOf(150_000, 100_000), 0.0001f) // typed more than the max: pinned at 100%
        assertEquals(0f, fractionOf(null, 100_000), 0.0001f)
        assertEquals(0f, fractionOf(10, 0), 0.0001f)
        assertEquals(25_000L, amountAtFraction(0.25f, 100_000))
        assertEquals(100_000L, amountAtFraction(1f, 100_000))
    }

    /** "Min" fills in the smallest amount the swap minimum allows on the
     *  sending chain: the BTC minimum itself, or the XBT that buys it at the price. */
    @Test
    fun min_is_the_smallest_send_that_meets_the_swap_minimum() {
        assertEquals(20_000L, com.fortis.wallet.ui.screens.minSendSat(20_000, "btc", 1_120_000))
        for (price in listOf(1_120_000L, 999_999L, 3L)) {
            val xbt = com.fortis.wallet.ui.screens.minSendSat(20_000, "xbt", price)!!
            assertTrue("$price", com.fortis.wallet.ui.screens.btcLeg(xbt, "xbt", price) >= 20_000)
            assertTrue("$price", com.fortis.wallet.ui.screens.btcLeg(xbt - 1, "xbt", price) < 20_000)
        }
        assertEquals(null, com.fortis.wallet.ui.screens.minSendSat(20_000, "xbt", 0))
    }
}
