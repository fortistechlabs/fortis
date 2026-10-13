package com.fortis.wallet.ui

import com.fortis.wallet.data.NeoxEx
import com.fortis.wallet.data.Quote
import com.fortis.wallet.ui.screens.amountError
import com.fortis.wallet.ui.screens.btcLeg
import com.fortis.wallet.ui.screens.commission
import com.fortis.wallet.ui.screens.commissionIsMinimum
import com.fortis.wallet.ui.screens.defaultPrice
import com.fortis.wallet.ui.screens.fmtPrice
import com.fortis.wallet.ui.screens.needsConfirm
import com.fortis.wallet.ui.screens.parsePrice
import com.fortis.wallet.ui.screens.worseByPct
import com.fortis.wallet.ui.screens.youReceive
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
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
    fun commissionCrossover() {
        assertEquals(400L, commission(20_000))
        assertEquals(400L, commission(79_999))
        assertEquals(400L, commission(80_000))
        assertEquals(401L, commission(80_200))
        assertEquals(5_000L, commission(1_000_000))
    }

    @Test
    fun commissionIsMinimumBelowCrossover() {
        assertTrue(commissionIsMinimum(79_999))
        assertTrue(commissionIsMinimum(80_000))
        assertFalse(commissionIsMinimum(80_200))
    }

    @Test
    fun btc_leg_is_the_btc_side_either_way() {
        assertEquals(200_000L, btcLeg(200_000, "btc", 1_120_000))
        assertEquals(1_120_000L, btcLeg(100_000_000, "xbt", 1_120_000))
    }

    @Test
    fun amount_error_below_minimum_and_above_cap() {
        assertNotNull(amountError(1_999, 2_000, 1_000_000))
        assertNull(amountError(2_000, 2_000, 1_000_000))
        assertNull(amountError(1_000_000, 2_000, 1_000_000))
        assertNotNull(amountError(1_000_001, 2_000, 1_000_000))
    }

    @Test
    fun noCapMeansNoMaximum() {
        // The edge reports max_btc_sat: null when it runs without a cap.
        assertNull(amountError(5_000_000_000, 2_000, null))
        assertNotNull(amountError(1_999, 2_000, null))
    }

    @Test
    fun parsePriceReadsDecimals() {
        assertEquals(412_000L, parsePrice("0.00412"))
        assertEquals(412_000L, parsePrice("0,00412"))
        assertEquals(100_000_000L, parsePrice("1"))
        assertEquals(1L, parsePrice("0.00000001"))
        assertEquals(2_100_000_000_000_000L, parsePrice("21000000"))
    }

    @Test
    fun parsePriceRejectsJunk() {
        for (junk in listOf("", "0", "0.000000001", "abc", "1.2.3", "-1", "99999999", "0.0", " ", ".", "1e3", "21000000.00000001",
            "184467440738.09551616", "184467440737.09551617", "123456789012345678901234567890")) {
            assertNull("'$junk' must not parse", parsePrice(junk))
        }
    }

    @Test
    fun fmtPriceTrims() {
        assertEquals("0.00412", fmtPrice(412_000))
        assertEquals("1.00000", fmtPrice(100_000_000))
        assertEquals("0.00000001", fmtPrice(1))
    }

    @Test
    fun fmtPriceIsAsciiInAnyLocale() {
        val old = java.util.Locale.getDefault()
        try {
            java.util.Locale.setDefault(java.util.Locale("ar"))
            assertEquals(412_000L, parsePrice(fmtPrice(412_000)))
        } finally {
            java.util.Locale.setDefault(old)
        }
    }

    @Test
    fun worseByPctIsSidedForSeller() {
        assertEquals(7.2, worseByPct(928, 1000, "xbt"), 0.001)
    }

    @Test
    fun worseByPctIsSidedForBuyer() {
        assertEquals(1.7, worseByPct(1017, 1000, "btc"), 0.001)
    }

    @Test
    fun needsConfirmAboveFivePercent() {
        assertFalse(needsConfirm(950, 1000, "xbt")) // worse by exactly 5.0
        assertTrue(needsConfirm(949, 1000, "xbt")) // worse by 5.1
        assertFalse(needsConfirm(1050, 1000, "btc"))
        assertTrue(needsConfirm(1051, 1000, "btc"))
        assertFalse(needsConfirm(500, 1000, "btc")) // better than the reference
    }

    @Test
    fun needsConfirmWithoutReference() {
        assertTrue(needsConfirm(1000, null, "xbt"))
        assertTrue(needsConfirm(1000, null, "btc"))
    }

    @Test
    fun defaultPricePrefersBestOpposite() {
        val q = Quote(referencePrice = 1_000, referenceOk = true, bestXbtToBtc = 1_100, bestBtcToXbt = 900, feeBps = 50, minBtcSat = 1, maxBtcSat = 2)
        val own = NeoxEx.Reference(1_050, true)
        // selling XBT meets the best bid; selling BTC meets the best ask
        assertEquals(900L, defaultPrice(q, own, "xbt"))
        assertEquals(1_100L, defaultPrice(q, own, "btc"))
        // no open offers: our own reference, then the edge's
        val bare = q.copy(bestXbtToBtc = null, bestBtcToXbt = null)
        assertEquals(1_050L, defaultPrice(bare, own, "xbt"))
        assertEquals(1_000L, defaultPrice(bare, null, "btc"))
        assertNull(defaultPrice(bare.copy(referencePrice = null), null, "xbt"))
        assertNull(defaultPrice(null, null, "xbt"))
        // no reference at all: the best open price, unclamped
        val noRef = q.copy(referencePrice = null)
        assertEquals(900L, defaultPrice(noRef, null, "xbt"))
        assertEquals(1_100L, defaultPrice(noRef, null, "btc"))
    }

    @Test
    fun defaultPriceClampsBaitToReference() {
        // A bait bid 10x the reference / a bait ask at 0.1x is never prefilled past it.
        val bait = Quote(referencePrice = 1_000, referenceOk = true, bestXbtToBtc = 100, bestBtcToXbt = 10_000, feeBps = 50, minBtcSat = 1, maxBtcSat = 2)
        assertEquals(1_000L, defaultPrice(bait, null, "xbt"))
        assertEquals(1_000L, defaultPrice(bait, null, "btc"))
        // This phone's own reference wins over the edge's for the clamp too.
        val own = NeoxEx.Reference(1_050, true)
        assertEquals(1_050L, defaultPrice(bait, own, "xbt"))
        assertEquals(1_050L, defaultPrice(bait, own, "btc"))
        // A genuine best that is worse for the user than the reference is kept:
        // a bid below it for a seller, an ask above it for a buyer.
        val genuine = bait.copy(bestXbtToBtc = 1_020, bestBtcToXbt = 980)
        assertEquals(980L, defaultPrice(genuine, null, "xbt"))
        assertEquals(1_020L, defaultPrice(genuine, null, "btc"))
        assertEquals(980L, defaultPrice(genuine, own, "xbt"))
        assertEquals(1_050L, defaultPrice(genuine, own, "btc"))
    }
}
