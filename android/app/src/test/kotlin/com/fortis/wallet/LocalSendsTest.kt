package com.fortis.wallet

import com.fortis.wallet.data.Balances
import com.fortis.wallet.data.HistoryEntry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class LocalSendsTest {
    // 100k in, 30k to someone else, 200 network fee, 69,800 change back.
    private val send = LocalSend(
        walletId = "w", txid = "t1", spent = setOf("a:0"),
        inSat = 100_000, backSat = 69_800, feeSat = 200, at = 1_000_000,
    )
    private val server = Balances(confirmedSat = 150_000, pendingSat = 0)
    private val old = HistoryEntry("t0", send = false, amountSat = 150_000, confirmations = 9, time = 500)

    @Test
    fun anUnlistedSendShowsAtOnce() {
        val (b, h) = withLocalSends(server, listOf(old), listOf(send))
        assertEquals(50_000L, b.confirmedSat)
        assertEquals(69_800L, b.pendingSat)
        assertEquals(listOf("t1", "t0"), h.map { it.txid })
        // the same row the server will list: what left the wallet, less the fee shown apart
        assertEquals(-30_000L, h[0].amountSat)
        assertEquals(200L, h[0].feeSat)
        assertEquals(0L, h[0].confirmations)
        assertTrue(h[0].send)
    }

    @Test
    fun onceTheServerListsItNothingIsAppliedTwice() {
        val listed = HistoryEntry("t1", send = true, amountSat = -30_000, confirmations = 0, time = 1_000, feeSat = 200)
        val after = Balances(confirmedSat = 50_000, pendingSat = 69_800)
        val (b, h) = withLocalSends(after, listOf(listed, old), listOf(send))
        assertSame(after, b)
        assertEquals(listOf("t1", "t0"), h.map { it.txid })
    }

    @Test
    fun aPaymentToItselfOnlyCostsTheFees() {
        val self = send.copy(backSat = 99_800)
        val (b, h) = withLocalSends(server, emptyList(), listOf(self))
        assertEquals(50_000L, b.confirmedSat)
        assertEquals(99_800L, b.pendingSat)
        assertTrue(h[0].internal)
    }

    @Test
    fun theBalanceNeverGoesNegative() {
        val (b, _) = withLocalSends(Balances(10_000, 0), emptyList(), listOf(send))
        assertEquals(0L, b.confirmedSat)
    }

    @Test
    fun aDegradedFlagIsKept() {
        val (b, _) = withLocalSends(server.copy(serverDegraded = true), emptyList(), listOf(send))
        assertTrue(b.serverDegraded)
    }

    @Test
    fun pruningDropsWhatTheServerListsAndTheExpired() {
        val other = send.copy(walletId = "x", txid = "t1")
        val second = send.copy(txid = "t2")
        val now = send.at + 1_000
        assertEquals(listOf(other, second), pruneLocalSends(listOf(send, other, second), "w", setOf("t1"), now))
        // no history to go by: only expiry
        assertEquals(listOf(send, other, second), pruneLocalSends(listOf(send, other, second), "w", null, now))
        assertEquals(emptyList<LocalSend>(), pruneLocalSends(listOf(send), "w", null, send.at + LOCAL_SEND_MS))
    }
}
