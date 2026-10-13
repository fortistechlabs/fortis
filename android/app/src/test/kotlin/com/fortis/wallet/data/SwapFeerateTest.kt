package com.fortis.wallet.data

import org.junit.Assert.assertEquals
import org.junit.Test

class SwapFeerateTest {
    @Test
    fun edge_fee_estimates_are_clamped_to_the_swap_ceiling() {
        assertEquals(listOf(12L, 30L), clampFeerates(listOf(12L, 30L), max = 500))
        // A hostile or broken edge's absurd estimate can't reach a signed spend.
        assertEquals(listOf(500L), clampFeerates(listOf(100_000L, 1_000_000L), max = 500))
        assertEquals(listOf(40L, 500L), clampFeerates(listOf(40L, 9_999L), max = 500))
        // Zero or negative becomes the 1 sat/vB floor.
        assertEquals(listOf(1L, 5L), clampFeerates(listOf(0L, 5L), max = 500))
    }
}
