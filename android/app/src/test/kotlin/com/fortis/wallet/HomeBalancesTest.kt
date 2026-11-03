package com.fortis.wallet

import com.fortis.wallet.data.Balances
import com.fortis.wallet.lightning.LnState
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.wallet_ffi.LnBalances
import uniffi.wallet_ffi.LnChannelState
import uniffi.wallet_ffi.LnFfiException

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

    @Test
    fun total_is_sum() {
        val ln = LnState.Ready(
            balances = LnBalances(
                lightningSat = 45_000uL,
                onchainTransitSat = 0uL,
                pendingCloseSat = 0uL,
                reservedSat = 0uL,
                pendingTransitSat = 0uL,
                inboundSat = 55_000uL,
            ),
            channel = LnChannelState.READY,
        )
        val hb = homeBalance(onchainSat = 100_000L, ln = ln)
        assertEquals(145_000L, hb.totalSat)
        assertEquals(100_000L, hb.onchainSat)
        assertEquals(45_000L, hb.lightningSat)
        assertFalse(hb.moving)
    }

    @Test
    fun lightning_off_equals_onchain_only() {
        val hb = homeBalance(onchainSat = 100_000L, ln = LnState.Off)
        assertEquals(100_000L, hb.totalSat)
        assertEquals(100_000L, hb.onchainSat)
        assertEquals(0L, hb.lightningSat)
        assertFalse(hb.moving)
    }

    @Test
    fun transit_counts_as_lightning_and_sets_moving() {
        val ln = LnState.Ready(
            balances = LnBalances(
                lightningSat = 20_000uL,
                onchainTransitSat = 80_000uL,
                pendingCloseSat = 0uL,
                reservedSat = 0uL,
                pendingTransitSat = 0uL,
                inboundSat = 0uL,
            ),
            channel = LnChannelState.OPENING,
        )
        val hb = homeBalance(onchainSat = 50_000L, ln = ln)
        assertEquals(150_000L, hb.totalSat)
        assertEquals(50_000L, hb.onchainSat)
        assertEquals(100_000L, hb.lightningSat)
        assertTrue(hb.moving)
    }

    @Test
    fun error_state_keeps_last_known_lightning_balance() {
        val lastBal = LnBalances(
            lightningSat = 60_000uL,
            onchainTransitSat = 0uL,
            pendingCloseSat = 0uL,
            reservedSat = 0uL,
            pendingTransitSat = 0uL,
            inboundSat = 40_000uL,
        )
        val ln = LnState.Error(LnFfiException.LspUnreachable(), lastBalances = lastBal)
        val hb = homeBalance(onchainSat = 100_000L, ln = ln)
        assertEquals(160_000L, hb.totalSat)
        assertEquals(100_000L, hb.onchainSat)
        assertEquals(60_000L, hb.lightningSat)
        assertFalse(hb.moving)
    }

    @Test
    fun lightningWalletBalanceIsLightningPlusMoving() {
        val ln = LnState.Ready(
            balances = LnBalances(
                lightningSat = 80_000uL,
                onchainTransitSat = 20_000uL,
                pendingCloseSat = 0uL,
                reservedSat = 0uL,
                pendingTransitSat = 0uL,
                inboundSat = 0uL,
            ),
            channel = LnChannelState.READY,
        )
        val hb = homeBalance(0L, ln)
        assertEquals(100_000L, hb.totalSat)
        assertTrue(hb.moving)
    }
}
