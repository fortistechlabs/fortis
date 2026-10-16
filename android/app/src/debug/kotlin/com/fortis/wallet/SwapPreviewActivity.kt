package com.fortis.wallet

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.fortis.wallet.data.NeoxEx
import com.fortis.wallet.data.Quote
import com.fortis.wallet.data.Step
import com.fortis.wallet.data.SwapRecord
import com.fortis.wallet.data.Taken
import com.fortis.wallet.data.WalletConfig
import com.fortis.wallet.ui.AmbientBackground
import com.fortis.wallet.ui.screens.CurrencyPicker
import com.fortis.wallet.ui.screens.NewSwapContent
import com.fortis.wallet.ui.screens.SwapDetailContent
import com.fortis.wallet.ui.screens.SwapListContent
import com.fortis.wallet.ui.screens.TakeCodeContent
import com.fortis.wallet.ui.theme.FortisTheme
import com.fortis.wallet.ui.theme.Fx
import org.json.JSONObject

/**
 * Debug builds only (src/debug): renders the swap screens with fake data, to
 * check their layout on a phone without an edge that has swaps on.
 * `adb shell am start -n com.fortistechlabs.wallet.dev/com.fortis.wallet.SwapPreviewActivity --es screen new`
 * (screen: list | new | new_low | new_small | new_noref | take | take_noref | detail_code | detail_funded
 *  | detail_turn | detail_need_funding | detail_done | detail_released | currency).
 */
class SwapPreviewActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        val screen = intent?.getStringExtra("screen") ?: "list"
        val now = System.currentTimeMillis() / 1000
        val wallets = listOf(
            WalletConfig("w1", "Savings", "btc", "mainnet", sealed = "x", salt = "y"),
            WalletConfig("w2", "Savings", "xbt", "mainnet", sealed = "x", salt = "y"),
            WalletConfig("w3", "Spending", "xbt", "mainnet", sealed = "x", salt = "y"),
        )
        val balances = mapOf("w1" to 1_234_567L, "w2" to 250_000_000L)
        val quote = Quote(1_120_000, true, 1_120_000, 1_110_000, 50, 2_000, null)
        val terms = JSONObject()
            .put("match_id", "m1").put("network", "bitcoin")
            .put("xbt_sat", 17_857_142L).put("btc_sat", 200_000L).put("price_sat_per_xbt", 1_120_000L)
            .put("fee_bps", 50).put("fee_address", "bc1qfee")
            .put("matched_at", now - 30).put("xbt_locktime", now + 144 * 3600).put("btc_locktime", now + 24 * 3600)
            .put("xbt_confs", 6).put("btc_confs", 2).put("accept_by", now + 90).put("initiator_fund_by", now + 1890)
            .put("contract_version", 2)
            .toString()
        fun rec(id: String, step: Step, send: String = "btc") = SwapRecord(
            id = id, fromWalletId = "w1", toWalletId = "w2", sendChain = send, amountSat = 200_000,
            limitPrice = 1_120_000, ownerSecret = "s", swapIndex = 1, wrappedXbt = "", wrappedBtc = "",
            redeemAddress = "r", refundAddress = "f", step = step,
        )
        val funded = rec("o2", Step.FUNDED).copy(
            matchId = "m1", role = "participant", termsJson = terms,
            theirTxid = "c0ffee".repeat(10) + "abcd", fundingTxid = "beef".repeat(16),
        )
        setContent {
            FortisTheme {
                Box(Modifier.fillMaxSize()) {
                    AmbientBackground()
                    Column(
                        Modifier.fillMaxSize().widthIn(max = 460.dp).align(Alignment.TopCenter)
                            .systemBarsPadding().imePadding().padding(horizontal = Fx.s4).padding(top = Fx.s3),
                    ) {
                        when (screen) {
                            "currency" -> CurrencyPicker(chosen = intent?.getStringExtra("chosen"), onPick = {}, onDismiss = {})
                            "new", "new_low", "new_small", "new_noref" -> NewSwapContent(
                                wallets, balances, if (screen == "new_noref") quote.copy(referencePrice = null, bestBtcToXbt = null) else quote,
                                if (screen == "new_noref") null else NeoxEx.Reference(1_100_000, true), busy = false, error = null,
                                onSubmit = { _, _, _, _, _ -> }, onBack = {},
                                initialAmount = when (screen) { "new_low" -> "0.0005"; "new_small" -> "0.0003"; else -> "0.002" },
                            )
                            "take", "take_noref" -> TakeCodeContent(
                                TakePreview(
                                    Taken("m1", "s", "o9"), terms, "participant",
                                    if (screen == "take") NeoxEx.Reference(1_118_000, true) else null, null,
                                ),
                                wallets, balances, busy = false, error = null,
                                onLookUp = { _ -> }, onAccept = { _, _ -> }, onDecline = {}, onBack = {},
                            )
                            "detail_code" -> SwapDetailContent(
                                rec("o1", Step.WAITING_MATCH).copy(shareCode = "K7QM2XWP"), now, null,
                                "BTC · Savings", "XBT · Savings", "mainnet", false, null, {}, {},
                            )
                            // the participant, while the initiator's XBT gathers its confirmations
                            "detail_turn" -> SwapDetailContent(
                                rec("o5", Step.MATCHED).copy(matchId = "m1", role = "participant", termsJson = terms, matchStatus = "initiator_funded"),
                                now, 2, "BTC · Savings", "XBT · Savings", "mainnet", false, null, {}, {},
                            )
                            "detail_need_funding" -> SwapDetailContent(
                                rec("o6", Step.NEED_FUNDING).copy(
                                    matchId = "m1", role = "participant", termsJson = terms, matchStatus = "initiator_funded",
                                    theirTxid = "c0ffee".repeat(10) + "abcd", deadline = now + 7000,
                                ),
                                now, 6, "BTC · Savings", "XBT · Savings", "mainnet", false, null, {}, {},
                            )
                            "detail_done" -> SwapDetailContent(
                                funded.copy(step = Step.DONE, redeemTxid = "feed".repeat(16), matchStatus = "closed"),
                                now, 6, "BTC · Savings", "XBT · Savings", "mainnet", false, null, {}, {},
                            )
                            // the initiator: the other side never funded and released its XBT
                            "detail_released" -> SwapDetailContent(
                                rec("o7", Step.REFUNDING, "xbt").copy(
                                    matchId = "m1", role = "initiator", termsJson = terms, matchStatus = "cancelled",
                                    fundingTxid = "beef".repeat(16), refundTxid = "abba".repeat(16), marks = setOf("released"),
                                    fromWalletId = "w2", toWalletId = "w1", amountSat = 17_857_142,
                                ),
                                now, null, "XBT · Savings", "BTC · Savings", "mainnet", false, null, {}, {},
                            )
                            "detail_funded" -> SwapDetailContent(
                                funded, now, null, "BTC · Savings", "XBT · Savings", "mainnet", false, null, {}, {},
                            )
                            // only watch-only / hardware wallets: the page explains what's needed
                            "list_need_phrase" -> SwapListContent(
                                emptyList(), canStart = false, error = null, onNew = {}, onTake = {}, onOpen = {}, onBack = {},
                                needsPhrase = true,
                            )
                            else -> SwapListContent(
                                listOf(
                                    rec("o1", Step.WAITING_MATCH),
                                    funded,
                                    rec("o3", Step.DONE, "xbt").copy(amountSat = 17_857_142),
                                    rec("o4", Step.CANCELLED),
                                ),
                                canStart = true, error = null, onNew = {}, onTake = {}, onOpen = {}, onBack = {},
                            )
                        }
                    }
                }
            }
        }
    }
}
