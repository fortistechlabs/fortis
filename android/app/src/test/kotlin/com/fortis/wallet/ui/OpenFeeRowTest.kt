package com.fortis.wallet.ui

import com.fortis.wallet.R
import com.fortis.wallet.lightning.LnMove
import com.fortis.wallet.lightning.adjustLatestMoveIn
import com.fortis.wallet.ui.screens.HistoryRow
import com.fortis.wallet.ui.screens.ledgerBalances
import com.fortis.wallet.ui.screens.withPendingOpenFee
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * 2026-11-16: after "move to Lightning" the history said "not accounted for 400 sat". The move's
 * entry is net of the ESTIMATED channel opening fee, but the fee is only taken when the channel
 * opens, so until then the wallet holds those 400 sat: the history shows them as an entry, and
 * after the open the entry is settled against the real fee.
 */
class OpenFeeRowTest {
    private fun moveIn(time: Long = 100, net: Long = 147_344) = HistoryRow(
        id = "move-$time", lightning = true, send = false, amountSat = net, feeSat = 1_488,
        description = "XBT · Wallet", time = time, netSat = net, noteRes = R.string.history_moved_in,
    )

    @Test
    fun whileTheMoveIsPendingTheHeldFeeIsAnEntryAndTheLedgerAddsUp() {
        val rows = listOf(moveIn())
        val shown = withPendingOpenFee(rows, totalSat = 147_744, transferPending = true)
        assertEquals(2, shown.size)
        val held = shown.first { it.noteRes == R.string.history_open_fee_held }
        assertEquals(400L, held.netSat)
        assertTrue(held.pending)
        assertTrue("just after the move it belongs to", held.time > rows[0].time)
        assertEquals("nothing left unaccounted", 0L, ledgerBalances(shown, 147_744).second)
    }

    @Test
    fun noEntryOnceTheMoveIsNotPending() {
        val rows = listOf(moveIn())
        assertSame(rows, withPendingOpenFee(rows, 147_744, transferPending = false))
    }

    @Test
    fun noEntryWhenTheLedgerAlreadyAddsUpOrIsFarOff() {
        val rows = listOf(moveIn())
        assertSame(rows, withPendingOpenFee(rows, 147_344, true))
        // A big gap is not a fee: it stays visible as "not accounted for".
        assertSame(rows, withPendingOpenFee(rows, 160_000, true))
        assertSame(rows, withPendingOpenFee(rows, 100_000, true))
    }

    private val into = LnMove(100, 147_344, 147_344, 1_488, "w", null, "in")

    @Test
    fun afterTheOpenTheMoveInIsSettledAgainstTheRealFee() {
        // The channel opened for 283 sat, not the estimated 400: the wallet holds 117 more.
        val fixed = adjustLatestMoveIn(listOf(into), deltaSat = 117)!!
        assertEquals(147_461L, fixed[0].amountSat)
        assertEquals(1_371L, fixed[0].feeSat)
        assertEquals(147_461L, fixed[0].payoutSat)
    }

    @Test
    fun aDearerOpenThanEstimatedRaisesTheFee() {
        val fixed = adjustLatestMoveIn(listOf(into), deltaSat = -150)!!
        assertEquals(147_194L, fixed[0].amountSat)
        assertEquals(1_638L, fixed[0].feeSat)
    }

    @Test
    fun onlyAFeeSizedGapOnTheNewestMoveInIsSettled() {
        assertNull(adjustLatestMoveIn(listOf(into), 0))
        assertNull(adjustLatestMoveIn(listOf(into), 2_001))
        assertNull(adjustLatestMoveIn(listOf(into), -2_001))
        // The newest move is not a move in: nothing to settle against.
        val close = LnMove(200, 100, 100, 0, "w", null, "close")
        assertNull(adjustLatestMoveIn(listOf(into, close), 100))
        assertNull(adjustLatestMoveIn(emptyList(), 100))
    }
}
