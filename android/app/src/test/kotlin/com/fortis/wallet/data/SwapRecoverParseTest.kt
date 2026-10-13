package com.fortis.wallet.data

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class SwapRecoverParseTest {
    @Test
    fun a_full_and_a_minimal_entry() {
        val r = parseRecovered(
            JSONObject(
                """{"found":[
                {"tag":"aa","offer_id":"o1","offer_status":"matched","direction":"xbt_to_btc",
                 "match":{"id":"m1","role":"initiator","status":"initiator_funded","terms":{"xbt_sat":5},
                          "xbt_funding":{"txid":"t1","vout":1},"btc_funding":null},
                 "owner_secret":"s1"},
                {"tag":"bb","offer_id":"o2","offer_status":"open","direction":"btc_to_xbt","match":null,"owner_secret":null}
                ]}""",
            ),
        )
        assertEquals(2, r.size)
        val a = r[0]
        assertEquals("aa", a.tag); assertEquals("o1", a.offerId); assertEquals("xbt_to_btc", a.direction)
        assertEquals("s1", a.ownerSecret)
        val m = a.match!!
        assertEquals("m1", m.id); assertEquals("initiator", m.role); assertEquals("initiator_funded", m.status)
        assertEquals(5, m.terms.getInt("xbt_sat"))
        assertEquals(Outpoint("t1", 1), m.xbtFunding)
        assertNull(m.btcFunding)
        assertNull(r[1].match); assertNull(r[1].ownerSecret)
    }

    /** A tag whose offer the edge has purged: known (so counted used), nothing else. */
    @Test
    fun a_purged_entry() {
        val r = parseRecovered(JSONObject("""{"found":[{"tag":"aa","offer_id":null,"offer_status":"purged","direction":null,"match":null,"owner_secret":null}]}"""))
        assertEquals(listOf(Recovered("aa", null, null, null, null)), r)
    }
}
