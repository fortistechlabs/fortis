package com.fortis.wallet.lightning

import com.fortis.wallet.ui.screens.mergeLightningHistory
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.wallet_ffi.LnDirection
import uniffi.wallet_ffi.LnPayment
import uniffi.wallet_ffi.LnPaymentStatus

class LightningHistoryMergeTest {
    private fun rx(id: String, sat: Long, at: Long) = LnPayment(
        id = id, direction = LnDirection.IN, amountMsat = (sat * 1_000).toULong(), feeMsat = 0uL,
        status = LnPaymentStatus.SUCCEEDED, description = "", timestamp = at.toULong(),
    )

    private fun entry(kind: String, sat: Long, at: Long, hash: String? = null, feeSat: Long = 0) =
        AccountEntry(atSecs = at, kind = kind, amountMsat = sat * 1_000, feeMsat = feeSat * 1_000, paymentHash = hash)

    @Test
    fun account_receipts_show_and_the_move_into_the_channel_does_not() {
        val rows = mergeLightningHistory(
            ln = listOf(rx("aa", 1_000, 100), rx("move", 164, 300)),
            account = listOf(
                entry("received", 160, 200, "h1"),
                entry("received", 4, 250, "h2"),
                entry("converted", -164, 301, "move"),
            ),
        )
        assertEquals(listOf(4L, 160L, 1_000L), rows.map { it.amountSat })
        assertEquals(listOf(true, true, false), rows.map { it.viaFortis })
    }

    @Test
    fun an_old_move_without_its_payment_is_matched_by_amount_and_time() {
        val rows = mergeLightningHistory(
            ln = listOf(rx("move", 164, 300), rx("other", 164, 5_000)),
            account = listOf(entry("received", 164, 200, "h1"), entry("converted", -164, 330)),
        )
        // The receipt on the balance and the unrelated later payment of the same amount stay.
        assertEquals(listOf("other", "h1"), rows.map { it.id })
    }

    @Test
    fun a_move_that_opened_a_channel_shows_only_its_fee() {
        val rows = mergeLightningHistory(
            ln = emptyList(),
            account = listOf(entry("received", 52_267, 10), entry("converted", -52_267, 20, feeSat = 1_000)),
        )
        assertEquals(listOf(-1_000L, 52_267L), rows.map { it.amountSat })
        assertTrue(rows.first().send)
    }

    @Test
    fun the_ledger_reconciles_to_zero_and_shows_what_it_cannot_explain() {
        val rows = mergeLightningHistory(
            ln = listOf(rx("a", 10_000, 100), rx("fee", 600, 300).copy(direction = LnDirection.OUT)),
            account = listOf(entry("received", 2_000, 150)),
            moves = listOf(LnMove(301, 5_783, 5_000, 783, "BTC", "fee", "splice")),
        )
        // 10,000 in, 2,000 in, 5,783 out (the 600 fee folded into the move): 6,217 left.
        val (after, unaccounted) = com.fortis.wallet.ui.screens.ledgerBalances(rows, 6_217)
        assertEquals(0L, unaccounted)
        assertEquals(6_217L, after["move-301"])
        // A balance the entries don't explain shows as unaccounted, not hidden.
        assertEquals(500L, com.fortis.wallet.ui.screens.ledgerBalances(rows, 6_717).second)
    }

    @Test
    fun a_move_is_matched_to_the_transaction_that_delivered_it_with_its_exact_fee() {
        val moves = listOf(
            LnMove(1_000, 57_107, 56_324, 783, "BTC", "fee", "splice"),
            LnMove(2_000, 924, 924, 0, "BTC", null, "close"),
        )
        val history = listOf(
            com.fortis.wallet.data.HistoryEntry("big", send = false, amountSat = 56_324, confirmations = 3, time = 1_100),
            com.fortis.wallet.data.HistoryEntry("small", send = false, amountSat = 801, confirmations = 1, time = 2_400),
            com.fortis.wallet.data.HistoryEntry("other", send = false, amountSat = 900, confirmations = 1, time = 90_000),
        )
        val settled = settleMoves(moves, history)!!
        assertEquals(listOf("big", "small"), settled.map { it.txid })
        assertEquals(listOf(783L, 123L), settled.map { it.feeSat })
        assertEquals(listOf(56_324L, 801L), settled.map { it.payoutSat })
        // Nothing new the second time.
        assertEquals(null, settleMoves(settled, history))
    }
}
