package com.fortis.wallet.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** Plain Esplora (electrs) has no mempool `/v1/fees/recommended` — only `/fee-estimates`. */
class FeeEstimatesTest {
    private val body = """{"1":87.8,"2":60.1,"3":41.0,"6":20.4,"25":9.9,"144":3.2,"1008":1.1}"""

    @Test
    fun an_exact_target_is_used() = assertEquals(20uL, parseFeeEstimates(body, 6))

    @Test
    fun the_nearest_lower_target_stands_in() = assertEquals(10uL, parseFeeEstimates(body, 100))

    @Test
    fun the_next_block_is_target_one() = assertEquals(88uL, parseFeeEstimates(body, 1))

    @Test
    fun never_below_one() = assertEquals(1uL, parseFeeEstimates("""{"1":0.2}""", 1))

    @Test
    fun garbage_is_null() = assertNull(parseFeeEstimates("<html>", 6))

    @Test
    fun empty_is_null() = assertNull(parseFeeEstimates("{}", 6))
}
