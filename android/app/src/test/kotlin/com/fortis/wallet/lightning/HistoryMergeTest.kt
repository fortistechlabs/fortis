package com.fortis.wallet.lightning

import com.fortis.wallet.data.HistoryEntry
import com.fortis.wallet.lightning.deleteWalletLightningWarningSat
import com.fortis.wallet.ui.screens.mergeHistory
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.wallet_ffi.LnBalances
import uniffi.wallet_ffi.LnChannelState
import uniffi.wallet_ffi.LnDirection
import uniffi.wallet_ffi.LnPayment
import uniffi.wallet_ffi.LnPaymentStatus

class HistoryMergeTest {
    @Test
    fun merge_sorts_by_time_and_flags_lightning() {
        val onchain = listOf(
            HistoryEntry(
                txid = "tx-old",
                send = false,
                amountSat = 100_000L,
                confirmations = 12L,
                time = 1_000L,
                feeSat = 0L,
            ),
            HistoryEntry(
                txid = "tx-new",
                send = true,
                amountSat = -40_000L,
                confirmations = 2L,
                time = 3_000L,
                feeSat = 350L,
            ),
        )
        val ln = listOf(
            LnPayment(
                id = "ln-mid",
                direction = LnDirection.IN,
                amountMsat = 25_000_000uL,
                feeMsat = 500_000uL,
                status = LnPaymentStatus.SUCCEEDED,
                description = "Espresso",
                timestamp = 2_000uL,
            ),
            LnPayment(
                id = "ln-newest",
                direction = LnDirection.OUT,
                amountMsat = 12_000_000uL,
                feeMsat = 45_000uL,
                status = LnPaymentStatus.SUCCEEDED,
                description = "Bakery",
                timestamp = 4_000uL,
            ),
        )

        val merged = mergeHistory(onchain, ln)
        assertEquals(listOf("ln-newest", "tx-new", "ln-mid", "tx-old"), merged.map { it.id })

        val newest = merged[0]
        assertTrue(newest.lightning)
        assertTrue(newest.send)
        assertEquals(-12_000L, newest.amountSat)
        assertEquals(45L, newest.feeSat)
        assertEquals("Bakery", newest.description)
        assertFalse(newest.failed)

        val txNew = merged[1]
        assertFalse(txNew.lightning)
        assertTrue(txNew.send)
        assertEquals(-40_000L, txNew.amountSat)
        assertEquals(350L, txNew.feeSat)

        val lnMid = merged[2]
        assertTrue(lnMid.lightning)
        assertFalse(lnMid.send)
        assertEquals(25_000L, lnMid.amountSat)
        assertEquals(500L, lnMid.feeSat)
        assertEquals("Espresso", lnMid.description)
    }

    @Test
    fun failed_lightning_send_marked_failed() {
        val ln = listOf(
            LnPayment(
                id = "ln-failed",
                direction = LnDirection.OUT,
                amountMsat = 15_000_000uL,
                feeMsat = 0uL,
                status = LnPaymentStatus.FAILED,
                description = "Unreachable node",
                timestamp = 5_000uL,
            ),
        )
        val merged = mergeHistory(emptyList(), ln)
        assertEquals(1, merged.size)
        val row = merged.single()
        assertTrue(row.lightning)
        assertTrue(row.send)
        assertTrue(row.failed)
        assertFalse(row.pending)
        assertEquals(-15_000L, row.amountSat)
        assertEquals("Unreachable node", row.description)
    }

    @Test
    fun delete_warning_only_when_lightning_funds() {
        assertNull(deleteWalletLightningWarningSat(LnState.Off))
        assertNull(
            deleteWalletLightningWarningSat(
                LnState.Ready(
                    balances = LnBalances(
                        lightningSat = 0uL,
                        onchainTransitSat = 0uL,
                        pendingCloseSat = 0uL,
                        reservedSat = 0uL,
                        pendingTransitSat = 0uL,
                        inboundSat = 0uL,
                    ),
                    channel = LnChannelState.NONE,
                ),
            ),
        )

        val withFunds = LnState.Ready(
            balances = LnBalances(
                lightningSat = 85_000uL,
                onchainTransitSat = 0uL,
                pendingCloseSat = 0uL,
                reservedSat = 1_000uL,
                pendingTransitSat = 0uL,
                inboundSat = 100_000uL,
            ),
            channel = LnChannelState.READY,
        )
        assertEquals(85_000L, deleteWalletLightningWarningSat(withFunds))
    }
}
