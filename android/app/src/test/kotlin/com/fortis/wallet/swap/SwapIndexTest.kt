package com.fortis.wallet.swap

import com.fortis.wallet.data.SwapRecord
import com.fortis.wallet.data.SwapStore
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Test

/** Swap indices count up per seed (so a restored phone can find its swaps by
 *  their tags) and are never handed out twice. */
class SwapIndexTest {
    private fun record(id: String, index: Int, seed: String?) = SwapRecord(
        id = id, fromWalletId = "a", toWalletId = "b", sendChain = "xbt", amountSat = 5,
        limitPrice = 1, ownerSecret = "s", swapIndex = index, wrappedXbt = "x", wrappedBtc = "y",
        redeemAddress = "ra", refundAddress = "rf", seed = seed,
    )

    @Test
    fun nextSwapIndex_never_reuses() = runBlocking {
        val s = SwapStore(MemoryBlob())
        assertEquals(listOf(0, 1, 2), List(3) { s.nextSwapIndex("seedA") })
        assertEquals(0, s.nextSwapIndex("seedB"))
        s.bumpSwapIndex("seedA", 7)
        assertEquals(8, s.nextSwapIndex("seedA"))
        s.bumpSwapIndex("seedA", 2) // never back
        assertEquals(9, s.nextSwapIndex("seedA"))
        // An index a record holds is skipped — older records (no seed) too.
        s.put(record("o1", 10, "seedA"))
        s.put(record("o2", 11, null))
        assertEquals(12, s.nextSwapIndex("seedA"))
    }

    @Test
    fun the_counter_survives_a_restart() = runBlocking {
        val blob = MemoryBlob()
        SwapStore(blob).nextSwapIndex("seedA")
        assertEquals(1, SwapStore(blob).nextSwapIndex("seedA"))
    }

    /** A draw the edge refused before storing its tag (a mistyped share code,
     *  a price change) gives its index back: no gap for the restore scan. */
    @Test
    fun a_refused_draw_gives_its_index_back() = runBlocking {
        val store = SwapStore(MemoryBlob())
        assertEquals(0, store.nextSwapIndex("s"))
        store.releaseSwapIndex("s", 0)
        assertEquals(0, store.nextSwapIndex("s"))
        assertEquals(1, store.nextSwapIndex("s"))
        // only the latest draw comes back
        store.releaseSwapIndex("s", 0)
        assertEquals(2, store.nextSwapIndex("s"))
    }
}
