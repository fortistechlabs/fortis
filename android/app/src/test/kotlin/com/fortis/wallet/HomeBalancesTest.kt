package com.fortis.wallet

import com.fortis.wallet.data.Balances
import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Test

class HomeBalancesTest {
    private val start = OverviewBalances(mapOf("a" to 1_000L), mapOf("a" to 0L))

    @Test
    fun pendingIsRecordedAlongsideConfirmed() {
        val next = start.with("b", Balances(confirmedSat = 5_000, pendingSat = 35_000))
        assertEquals(5_000L, next.confirmed["b"])
        assertEquals(35_000L, next.pending["b"])
        assertEquals(1_000L, next.confirmed["a"])
    }

    @Test
    fun aDegradedScanChangesNothing() {
        // A flagged, incomplete scan reports no UTXOs: recording it would show 0.
        val next = start.with("a", Balances(confirmedSat = 0, pendingSat = 0, serverDegraded = true))
        assertSame(start, next)
    }

    @Test
    fun removingAWalletDropsBothAmounts() {
        val next = start.with("b", Balances(5_000, 35_000)).without("b")
        assertEquals(setOf("a"), next.confirmed.keys)
        assertEquals(setOf("a"), next.pending.keys)
    }
}
