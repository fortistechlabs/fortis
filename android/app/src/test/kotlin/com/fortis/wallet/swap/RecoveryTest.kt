package com.fortis.wallet.swap

import com.fortis.wallet.data.Outpoint
import com.fortis.wallet.data.Recovered
import com.fortis.wallet.data.RecoveredMatch
import com.fortis.wallet.data.Step
import com.fortis.wallet.data.WalletConfig
import kotlinx.coroutines.runBlocking
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class RecoveryTest {
    private fun tag(i: Int) = "tag-$i"

    /** An edge that knows the tags at [known] (offer `o-<index>`, no match). */
    private class Edge(val known: Set<Int>) {
        val calls = mutableListOf<List<String>>()
        suspend fun recover(tags: List<String>): List<Recovered> {
            calls += tags
            return tags.filter { it.removePrefix("tag-").toInt() in known }
                .map { Recovered(it, "o-${it.removePrefix("tag-")}", "xbt_to_btc", null, null) }
        }
    }

    @Test
    fun scan_stops_after_20_unknown_in_a_row() = runBlocking {
        val edge = Edge(setOf(0, 25, 60))
        val (found, highest) = scanTags(::tag, edge::recover, gap = 20, batch = 10)
        assertEquals(listOf(0, 25), found.map { it.first })
        assertEquals(25, highest)
        // 26..45 unknown ends it: batches 0-9 … 40-49, never 60
        assertEquals(5, edge.calls.size)
        assertTrue(edge.calls.all { it.size == 10 })
    }

    @Test
    fun scan_asks_in_batches_of_100_and_reports_none() = runBlocking {
        val edge = Edge(emptySet())
        val (found, highest) = scanTags(::tag, edge::recover)
        assertTrue(found.isEmpty())
        assertEquals(-1, highest)
        assertEquals(listOf(100), edge.calls.map { it.size })
    }

    @Test
    fun scan_has_an_end_even_if_the_edge_knows_every_tag() = runBlocking {
        val edge = Edge((0 until 1_000_000).toSet())
        val (_, highest) = scanTags(::tag, edge::recover, limit = 1_000)
        assertTrue(highest < 1_000)
    }

    private val terms = JSONObject()
        .put("match_id", "m1").put("xbt_sat", 10_000_000L).put("btc_sat", 112_000L).put("price_sat_per_xbt", 1_120_000L)
    private val xbtOp = Outpoint("aa".repeat(32), 1)
    private val btcOp = Outpoint("bb".repeat(32), 0)
    private val xbtWallet = WalletConfig("wx", "Main", "xbt", "mainnet", sealed = "s", salt = "t")
    private val btcWallet = WalletConfig("wb", "Main", "btc", "mainnet", sealed = "s", salt = "t")

    private fun rec(
        role: String, xbt: Outpoint?, btc: Outpoint?, secret: String? = "new-secret",
        direction: String = if (role == "initiator") "xbt_to_btc" else "btc_to_xbt", matched: Boolean = true,
    ) = Recovered(
        "tag-7", "offer-7", direction,
        if (matched) RecoveredMatch("m1", role, "accepted", terms, xbt, btc) else null, secret,
    )

    private fun build(r: Recovered, from: WalletConfig, to: WalletConfig) =
        recoveredRecord(7, r, "f00dbabe", from, to, "wx-wrapped", "wb-wrapped", "redeem-addr", "refund-addr", now = 42)

    @Test
    fun recovered_record_only_for_a_funded_own_leg() {
        // no match, no secret, or only their leg funded: nothing to rebuild
        assertNull(build(rec("initiator", null, null, matched = false), xbtWallet, btcWallet))
        assertNull(build(rec("initiator", xbtOp, null, secret = null), xbtWallet, btcWallet))
        assertNull(build(rec("initiator", null, btcOp), xbtWallet, btcWallet))
        assertNull(build(rec("participant", xbtOp, null), btcWallet, xbtWallet))
        // a role the offer's direction contradicts
        assertNull(build(rec("initiator", xbtOp, null, direction = "btc_to_xbt"), xbtWallet, btcWallet))
        // wallets on the wrong chains
        assertNull(build(rec("initiator", xbtOp, null), btcWallet, xbtWallet))

        val i = build(rec("initiator", xbtOp, btcOp), xbtWallet, btcWallet)!!
        assertEquals(Step.FUNDED, i.step)
        assertEquals("offer-7", i.id); assertEquals("m1", i.matchId); assertEquals("initiator", i.role)
        assertEquals("xbt", i.sendChain); assertEquals(10_000_000L, i.amountSat); assertEquals(1_120_000L, i.limitPrice)
        assertEquals(xbtOp.txid, i.fundingTxid); assertEquals(1, i.fundingVout)
        assertEquals("new-secret", i.ownerSecret); assertEquals(7, i.swapIndex)
        assertEquals("f00dbabe", i.seed); assertEquals("tag-7", i.recoveryTag)
        assertEquals("wx", i.fromWalletId); assertEquals("wb", i.toWalletId)
        assertEquals(terms.toString(), i.termsJson)
        assertTrue(i.marks.containsAll(listOf("accepted", "remainder_cancelled")))
        assertNull(i.sessionJson); assertNull(i.theirTxid)

        val p = build(rec("participant", xbtOp, btcOp), btcWallet, xbtWallet)!!
        assertEquals(Step.THEY_FUNDED, p.step)
        assertEquals("btc", p.sendChain); assertEquals(112_000L, p.amountSat)
        assertEquals(btcOp.txid, p.fundingTxid); assertEquals(0, p.fundingVout)
    }

    /** The swap's keys come from the seed that pays; the coins can land in any
     *  of the user's wallets on the other chain — a phone whose XBT and BTC
     *  wallets have different phrases still recovers its swaps. */
    @Test
    fun recovery_pays_into_another_seeds_wallet_when_its_own_seed_has_none() {
        val otherBtc = WalletConfig("ob", "Other", "btc", "mainnet", sealed = "s", salt = "t")
        assertEquals(xbtWallet to otherBtc, recoveryWallets("xbt", listOf(xbtWallet), listOf(xbtWallet, otherBtc)))
        // the seed's own wallet is preferred
        assertEquals(xbtWallet to btcWallet, recoveryWallets("xbt", listOf(xbtWallet, btcWallet), listOf(otherBtc, xbtWallet, btcWallet)))
        // the paying side must be the seed's own (its keys and refund)
        assertNull(recoveryWallets("btc", listOf(xbtWallet), listOf(xbtWallet, otherBtc)))
    }

    @Test
    fun recovery_skips_a_swap_without_both_wallets() {
        assertNull(recoveryWallets("xbt", listOf(xbtWallet)))
        assertNull(recoveryWallets("btc", listOf(xbtWallet)))
        assertEquals(xbtWallet to btcWallet, recoveryWallets("xbt", listOf(btcWallet, xbtWallet)))
        assertEquals(btcWallet to xbtWallet, recoveryWallets("btc", listOf(xbtWallet, btcWallet)))
    }
}
