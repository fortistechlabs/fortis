package com.fortis.wallet.swap

import com.fortis.wallet.data.SwapHttpException
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test

/** A swap index used before (the edge refuses its tag) is skipped: the next
 *  attempt draws the next index. Any other refusal is the caller's. */
class WithFreshIndexTest {
    private val used = SwapHttpException(409, "POST /swap/offers: $INDEX_USED")

    @Test
    fun createOffer_moves_past_a_used_index() = runBlocking {
        var calls = 0
        val got = withFreshIndex {
            calls++
            if (calls <= 2) throw used
            "posted on attempt $calls"
        }
        assertEquals("posted on attempt 3", got)
    }

    /** Each used index first catches the counter up (a scan of the seed's
     *  tags), so a restore whose counter lags by hundreds needs one retry. */
    @Test
    fun a_used_index_catches_the_counter_up_before_the_next_try() = runBlocking {
        var calls = 0
        var caught = 0
        val got = withFreshIndex(onUsed = { caught++ }) {
            calls++
            if (calls == 1) throw used
            "ok"
        }
        assertEquals("ok", got)
        assertEquals(1, caught)
    }

    @Test
    fun other_refusals_are_rethrown_at_once() {
        var calls = 0
        val e = assertThrows(SwapHttpException::class.java) {
            runBlocking { withFreshIndex<Unit> { calls++; throw SwapHttpException(409, "POST /swap/offers: price changed") } }
        }
        assertEquals(1, calls)
        assertEquals(409, e.code)
    }

    @Test
    fun it_gives_up_after_its_tries() {
        var calls = 0
        assertThrows(SwapHttpException::class.java) { runBlocking { withFreshIndex<Unit>(tries = 5) { calls++; throw used } } }
        assertEquals(5, calls)
    }
}
