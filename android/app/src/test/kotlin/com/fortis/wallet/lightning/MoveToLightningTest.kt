package com.fortis.wallet.lightning

import com.fortis.wallet.data.WalletConfig
import com.fortis.wallet.ui.screens.MovePlan
import com.fortis.wallet.ui.screens.planMove
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.wallet_ffi.LnBalances
import uniffi.wallet_ffi.LnChannelState

class MoveToLightningTest {
    private val sampleSeed = ByteArray(64) { (it + 1).toByte() }
    private val btcMainnetWallet = WalletConfig(
        id = "w-btc",
        name = "Main BTC",
        chain = "btc",
        network = "mainnet",
        watchOnly = false,
        hardware = false,
        sealed = "deadbeef",
        salt = "cafebabe",
    )

    @Test
    fun below_channel_min_rejected() {
        val plan = planMove(
            amountSat = 50_000L,
            onchainSat = 500_000L,
            channelMinSat = 100_000L,
            feeSat = 500L,
        )
        assertEquals(MovePlan.BelowMin(100_000L), plan)
    }

    @Test
    fun insufficient_after_fee_rejected() {
        val insufficient = planMove(
            amountSat = 100_000L,
            onchainSat = 100_200L,
            channelMinSat = 100_000L,
            feeSat = 500L,
        )
        assertEquals(MovePlan.Insufficient, insufficient)

        val exact = planMove(
            amountSat = 100_000L,
            onchainSat = 100_500L,
            channelMinSat = 100_000L,
            feeSat = 500L,
        )
        assertEquals(MovePlan.Ok, exact)
    }

    @Test
    fun second_move_opens_second_channel_in_phase1() = runBlocking {
        val openedAmounts = mutableListOf<ULong>()
        var transitSat = 0uL
        val fakeNode = object : LnNodeApi {
            override fun start() {}
            override fun stop() {}
            override fun balances(): LnBalances = LnBalances(
                lightningSat = 200_000uL,
                onchainTransitSat = transitSat,
                pendingCloseSat = 0uL,
                reservedSat = 2_000uL,
                pendingTransitSat = 0uL,
                inboundSat = 300_000uL,
            )
            override fun channelState(): LnChannelState = LnChannelState.READY
            override fun transitAddress(): String = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq"
            override fun openChannel(amountSat: ULong) {
                openedAmounts += amountSat
                transitSat = transitSat.saturatingSub(amountSat)
            }
        }

        val store = InMemoryLightningStore(wallets = mapOf(btcMainnetWallet.id to btcMainnetWallet))
        store.setLightningEnabled(btcMainnetWallet.id, true)
        val session = LightningSession(
            scope = CoroutineScope(SupervisorJob() + Dispatchers.Unconfined),
            store = store,
            nodeFactory = { fakeNode },
            info = LnInfoApi {
                LnInfo(
                    nodeId = "02" + "11".repeat(32),
                    host = "lsp.fortis.test",
                    port = 9735,
                    lsps2Token = "tok",
                    feePpm = 10_000L,
                    feeMinSat = 1_000L,
                    minPaymentSat = 10_000L,
                    maxPaymentSat = 500_000L,
                    channelMinSat = 100_000L,
                    channelMaxSat = 500_000L,
                )
            },
        )

        session.onUnlocked(btcMainnetWallet.id, sampleSeed).join()
        assertTrue(session.state.value is LnState.Ready)

        // Request a second move while a channel is already READY.
        session.requestMoveToChannel(150_000uL)
        assertTrue(openedAmounts.isEmpty())

        // Once the on-chain send to transit_address() confirms, refreshBalances() opens the second channel.
        transitSat = 150_000uL
        session.refreshBalances()
        assertEquals(listOf(150_000uL), openedAmounts)
    }
}

private fun ULong.saturatingSub(other: ULong): ULong = if (this > other) this - other else 0uL
