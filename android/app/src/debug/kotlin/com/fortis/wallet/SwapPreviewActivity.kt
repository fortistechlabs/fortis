package com.fortis.wallet

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.fortis.wallet.data.Quote
import com.fortis.wallet.data.Step
import com.fortis.wallet.data.SwapRecord
import com.fortis.wallet.data.Taken
import com.fortis.wallet.data.WalletConfig
import com.fortis.wallet.ui.AmbientBackground
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
 * (screen: list | new | new_low | take | detail_code | detail_funded).
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
        val quote = Quote(1_120_000, 50, 80_000, 1_000_000)
        val terms = JSONObject()
            .put("match_id", "m1").put("network", "bitcoin")
            .put("xbt_sat", 17_857_142L).put("btc_sat", 200_000L).put("price_sat_per_xbt", 1_120_000L)
            .put("fee_bps", 50).put("fee_address", "bc1qfee")
            .put("matched_at", now - 30).put("xbt_locktime", now + 144 * 3600).put("btc_locktime", now + 24 * 3600)
            .put("xbt_confs", 6).put("btc_confs", 2).put("accept_by", now + 90).put("initiator_fund_by", now + 1890)
            .toString()
        fun rec(id: String, step: Step, send: String = "btc") = SwapRecord(
            id = id, fromWalletId = "w1", toWalletId = "w2", sendChain = send, amountSat = 200_000,
            slippageBps = 100, ownerSecret = "s", swapIndex = 1, wrappedXbt = "", wrappedBtc = "",
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
                            "new", "new_low" -> NewSwapContent(
                                wallets, balances, quote, busy = false, error = null,
                                onSubmit = { _, _, _, _, _ -> }, onBack = {},
                                initialAmount = if (screen == "new_low") "0.0005" else "0.002",
                            )
                            "take" -> TakeCodeContent(
                                TakePreview(Taken("m1", "s", "o9"), terms, "participant", 100, 1_118_000, null),
                                wallets, balances, busy = false, error = null,
                                onLookUp = { _, _ -> }, onAccept = { _, _ -> }, onDecline = {}, onBack = {},
                            )
                            "detail_code" -> SwapDetailContent(
                                rec("o1", Step.WAITING_MATCH).copy(shareCode = "K7QM2XWP"), now, null,
                                "BTC · Savings", "XBT · Savings", "mainnet", false, null, {}, {},
                            )
                            "detail_funded" -> SwapDetailContent(
                                funded, now, null, "BTC · Savings", "XBT · Savings", "mainnet", false, null, {}, {},
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
