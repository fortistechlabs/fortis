package com.fortis.wallet.data

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Test

/** An address dusted past the index's per-address cap comes back with its
 *  largest coins listed and the rest summed under `unlisted`: the balance
 *  counts those too, so dust can't make a wallet look emptier than it is. */
class ScanUnlistedTest {
    @Test
    fun unlisted_coins_add_to_the_balance() {
        val r = JSONObject("""{"unlisted":[{"address":"bc1qa","count":12000,"value":3600000},{"address":"bc1qb","count":1,"value":5}]}""")
        assertEquals(3_600_005L, unlistedSat(r, setOf("bc1qa", "bc1qb")))
    }

    @Test
    fun no_unlisted_field_is_nothing_unlisted() {
        assertEquals(0L, unlistedSat(JSONObject("""{"utxos":[]}"""), setOf("bc1qa")))
    }

    @Test(expected = java.io.IOException::class)
    fun an_address_that_was_not_asked_about_is_refused() {
        unlistedSat(JSONObject("""{"unlisted":[{"address":"bc1qz","count":1,"value":1}]}"""), setOf("bc1qa"))
    }
}
