package com.fortis.wallet.data

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class SwapBookParseTest {
    @Test
    fun parses_both_sides_of_the_book() {
        val json = JSONObject(
            """
            {"xbt_to_btc":[{"id":"o1","direction":"xbt_to_btc","amount_sat":20000000,"price_sat_per_xbt":990000,
                            "btc_sat":198000,"xbt_sat":20000000,"expires_at":1791800000}],
             "btc_to_xbt":[{"id":"o2","direction":"btc_to_xbt","amount_sat":300000,"price_sat_per_xbt":950000,
                            "btc_sat":300000,"xbt_sat":31578947,"expires_at":1791800001}]}
            """,
        )
        val b = parseBook(json)
        assertEquals(listOf(BookEntry("o1", "xbt_to_btc", 20_000_000, 990_000, 198_000, 20_000_000, 1_791_800_000)), b.xbtToBtc)
        assertEquals(listOf(BookEntry("o2", "btc_to_xbt", 300_000, 950_000, 300_000, 31_578_947, 1_791_800_001)), b.btcToXbt)
    }

    @Test
    fun a_missing_side_is_empty() {
        val b = parseBook(JSONObject("""{"xbt_to_btc":[]}"""))
        assertTrue(b.xbtToBtc.isEmpty())
        assertTrue(b.btcToXbt.isEmpty())
    }
}
