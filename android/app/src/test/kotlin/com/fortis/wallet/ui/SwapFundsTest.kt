package com.fortis.wallet.ui

import com.fortis.wallet.data.BookEntry
import com.fortis.wallet.data.WalletConfig
import com.fortis.wallet.ui.screens.Funds
import com.fortis.wallet.ui.screens.fundsFor
import com.fortis.wallet.ui.screens.maxSwapSat
import com.fortis.wallet.ui.screens.takerNeedSat
import com.fortis.wallet.ui.screens.takerSendChain
import org.junit.Assert.assertEquals
import org.junit.Test

class SwapFundsTest {
    /** Someone selling 0.2 XBT for 198 000 sat BTC: a taker sends BTC. */
    private val ask = BookEntry("o1", "xbt_to_btc", 20_000_000, 990_000, 198_000, 20_000_000, 9_999_999_999)
    private val min = 2_000L

    private fun w(id: String, chain: String, watchOnly: Boolean = false, hardware: Boolean = false) =
        WalletConfig(id = id, name = id, chain = chain, network = "mainnet", watchOnly = watchOnly, hardware = hardware)

    private val xbtWallet = w("x", "xbt")

    @Test
    fun the_taker_sends_the_coin_the_offer_wants() {
        assertEquals("btc", takerSendChain(ask))
        assertEquals(198_000L, takerNeedSat(ask))
        val bid = BookEntry("o2", "btc_to_xbt", 300_000, 950_000, 300_000, 31_578_947, 9_999_999_999)
        assertEquals("xbt", takerSendChain(bid))
        assertEquals(31_578_947L, takerNeedSat(bid))
    }

    @Test
    fun all_of_it_when_the_best_wallet_covers_it() {
        val f = fundsFor(ask, listOf(w("b", "btc"), xbtWallet), mapOf("b" to 1_000_000L), mapOf("b" to 1_000L), min)
        assertEquals(Funds.All("b"), f)
    }

    @Test
    fun up_to_x_when_only_part_fits() {
        val f = fundsFor(ask, listOf(w("b", "btc"), xbtWallet), mapOf("b" to 100_000L), mapOf("b" to 1_000L), min)
        assertEquals(Funds.UpTo("b", maxSwapSat(100_000, 1_000)), f)
    }

    @Test
    fun not_enough_below_the_minimum() {
        val f = fundsFor(ask, listOf(w("b", "btc"), xbtWallet), mapOf("b" to 2_500L), mapOf("b" to 1_000L), min)
        assertEquals(Funds.NotEnough, f)
    }

    @Test
    fun needs_a_receive_wallet_on_the_other_chain_first() {
        val f = fundsFor(ask, listOf(w("b", "btc")), mapOf("b" to 1_000_000L), mapOf("b" to 1_000L), min)
        assertEquals(Funds.NeedReceiveWallet("xbt"), f)
    }

    @Test
    fun watch_only_and_hardware_wallets_are_ignored() {
        val wallets = listOf(w("watch", "btc", watchOnly = true), w("hw", "btc", watchOnly = true, hardware = true), xbtWallet)
        val f = fundsFor(ask, wallets, mapOf("watch" to 9_000_000L, "hw" to 9_000_000L), mapOf("watch" to 1_000L, "hw" to 1_000L), min)
        assertEquals(Funds.NotEnough, f)
        // …and a watch-only wallet on the other chain can't receive a swap either.
        val onlyWatchXbt = listOf(w("b", "btc"), w("wx", "xbt", watchOnly = true))
        assertEquals(Funds.NeedReceiveWallet("xbt"), fundsFor(ask, onlyWatchXbt, mapOf("b" to 1_000_000L), mapOf("b" to 1_000L), min))
    }

    @Test
    fun picks_the_wallet_that_can_swap_the_most() {
        val wallets = listOf(w("small", "btc"), w("big", "btc"), xbtWallet)
        val f = fundsFor(ask, wallets, mapOf("small" to 50_000L, "big" to 120_000L), mapOf("small" to 500L, "big" to 2_000L), min)
        assertEquals(Funds.UpTo("big", maxSwapSat(120_000, 2_000)), f)
    }

    @Test
    fun unknown_balance_or_fee_reserve_counts_as_nothing_yet() {
        assertEquals(Funds.NotEnough, fundsFor(ask, listOf(w("b", "btc"), xbtWallet), emptyMap(), mapOf("b" to 1_000L), min))
        assertEquals(Funds.NotEnough, fundsFor(ask, listOf(w("b", "btc"), xbtWallet), mapOf("b" to 1_000_000L), emptyMap(), min))
    }
}
