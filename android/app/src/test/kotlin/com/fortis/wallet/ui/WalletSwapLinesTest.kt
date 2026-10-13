package com.fortis.wallet.ui

import com.fortis.wallet.data.Step
import com.fortis.wallet.data.SwapRecord
import com.fortis.wallet.ui.screens.WalletSwapKind
import com.fortis.wallet.ui.screens.WalletSwapLine
import com.fortis.wallet.ui.screens.walletSwapLines
import org.junit.Assert.assertEquals
import org.junit.Test

class WalletSwapLinesTest {
    private val terms = """{"xbt_sat":719066,"btc_sat":6402}"""

    /** An XBT → BTC swap from wallet "x" into wallet "b". */
    private fun swap(step: Step, matched: Boolean = true, funded: Boolean = false, id: String = "s1") = SwapRecord(
        id = id, fromWalletId = "x", toWalletId = "b", sendChain = "xbt", amountSat = 800_000,
        limitPrice = 890_000, ownerSecret = "", swapIndex = 1, wrappedXbt = "", wrappedBtc = "",
        redeemAddress = "r", refundAddress = "f",
        step = step, termsJson = if (matched) terms else null, fundingTxid = if (funded) "f" else null,
    )

    @Test
    fun an_offer_waiting_for_a_match_will_send_the_offer_amount_and_nothing_is_on_its_way_yet() {
        val s = listOf(swap(Step.WAITING_MATCH, matched = false))
        assertEquals(listOf(WalletSwapLine("s1", WalletSwapKind.SENDING, 800_000, "xbt")), walletSwapLines("x", s))
        assertEquals(emptyList<WalletSwapLine>(), walletSwapLines("b", s))
    }

    @Test
    fun once_matched_the_amounts_come_from_the_terms() {
        val s = listOf(swap(Step.MATCHED))
        assertEquals(listOf(WalletSwapLine("s1", WalletSwapKind.SENDING, 719_066, "xbt")), walletSwapLines("x", s))
        assertEquals(listOf(WalletSwapLine("s1", WalletSwapKind.INCOMING, 6_402, "btc")), walletSwapLines("b", s))
    }

    @Test
    fun once_funded_the_paying_wallets_coins_are_locked() {
        for (step in listOf(Step.FUNDED, Step.THEY_FUNDED, Step.REDEEMED)) {
            val s = listOf(swap(step, funded = true))
            assertEquals(step.name, WalletSwapKind.LOCKED, walletSwapLines("x", s).single().kind)
            assertEquals(step.name, WalletSwapKind.INCOMING, walletSwapLines("b", s).single().kind)
        }
    }

    /** A funding broadcast but not yet seen counts as locked too. */
    @Test
    fun a_pending_funding_counts_as_locked() {
        val s = listOf(swap(Step.NEED_FUNDING).copy(pendingFundingTxid = "p"))
        assertEquals(WalletSwapKind.LOCKED, walletSwapLines("x", s).single().kind)
    }

    @Test
    fun a_refund_is_coming_back_and_nothing_else_is_coming() {
        val s = listOf(swap(Step.REFUNDING, funded = true))
        assertEquals(listOf(WalletSwapLine("s1", WalletSwapKind.RETURNING, 719_066, "xbt")), walletSwapLines("x", s))
        assertEquals(emptyList<WalletSwapLine>(), walletSwapLines("b", s))
    }

    @Test
    fun finished_swaps_show_nothing() {
        for (step in listOf(Step.DONE, Step.REFUNDED, Step.CANCELLED, Step.FAILED)) {
            val s = listOf(swap(step, funded = true))
            assertEquals(step.name, emptyList<WalletSwapLine>(), walletSwapLines("x", s) + walletSwapLines("b", s))
        }
    }

    @Test
    fun other_wallets_see_nothing_and_each_swap_has_its_own_line() {
        val s = listOf(swap(Step.FUNDED, funded = true, id = "a"), swap(Step.MATCHED, id = "b2"))
        assertEquals(emptyList<WalletSwapLine>(), walletSwapLines("other", s))
        assertEquals(listOf("a", "b2"), walletSwapLines("x", s).map { it.swapId })
    }
}
