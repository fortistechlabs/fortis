package com.fortis.wallet.lightning

import com.fortis.wallet.data.WalletConfig
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.wallet_ffi.LnBalances
import uniffi.wallet_ffi.LnChannelState
import uniffi.wallet_ffi.LnFfiException
import uniffi.wallet_ffi.LnNodeConfig
import java.util.concurrent.atomic.AtomicInteger

class LightningSessionTest {
    private val sampleSeed = ByteArray(64) { (it + 1).toByte() }

    private val btcMainnetWallet = WalletConfig(
        id = "w-btc",
        name = "Main",
        chain = "btc",
        network = "mainnet",
        sealed = "deadbeef",
        salt = "cafebabe",
        backendToken = "tok-123",
    )

    private fun sampleInfo(
        nodeIdSuffix: String = "11",
        enabled: Boolean = true,
        message: String? = null,
    ) = LnInfo(
        nodeId = "02" + nodeIdSuffix.repeat(32),
        host = "lsp.fortis.test",
        port = 9735,
        lsps2Token = "lsps2-tok",
        feePpm = 10_000L,
        feeMinSat = 1_000L,
        minPaymentSat = 10_000L,
        maxPaymentSat = 500_000L,
        channelMinSat = 100_000L,
        channelMaxSat = 500_000L,
        enabled = enabled,
        message = message,
        network = "bitcoin",
    )

    private class FakeNode(
        var balancesValue: LnBalances = LnBalances(
            lightningSat = 50_000uL,
            onchainTransitSat = 0uL,
            pendingCloseSat = 0uL,
            reservedSat = 0uL,
            pendingTransitSat = 0uL,
            inboundSat = 50_000uL,
        ),
        var channelValue: LnChannelState = LnChannelState.READY,
        private val onStart: () -> Unit = {},
        private val onStop: () -> Unit = {},
        private val onSweep: (String) -> String? = { "txid-swept" },
        private val failCoopClose: Boolean = false,
    ) : LnNodeApi {
        var started = false
        var startMode: uniffi.wallet_ffi.LnStartMode? = null
        var stopped = false
        var sweptTo: String? = null
        var openedChannelSat: ULong? = null
        var coopClosed = false
        var forceClosed = false

        override fun start() {
            onStart()
            started = true
            startMode = uniffi.wallet_ffi.LnStartMode.NORMAL
        }

        override fun startWith(mode: uniffi.wallet_ffi.LnStartMode) {
            onStart()
            started = true
            startMode = mode
        }

        override fun stop() {
            onStop()
            stopped = true
        }

        override fun balances(): LnBalances = balancesValue

        override fun channelState(): LnChannelState = channelValue

        override fun openChannel(amountSat: ULong) {
            openedChannelSat = amountSat
        }

        override fun closeChannel() {
            if (failCoopClose) throw LnFfiException.LspUnreachable()
            coopClosed = true
            channelValue = LnChannelState.CLOSING
        }

        override fun forceCloseChannel() {
            forceClosed = true
            channelValue = LnChannelState.CLOSING
        }

        override fun sweepTransit(toAddress: String): String? {
            sweptTo = toAddress
            return onSweep(toAddress)
        }
    }

    @Test
    fun off_by_default_and_never_starts() = runBlocking {
        val store = InMemoryLightningStore(mapOf(btcMainnetWallet.id to btcMainnetWallet))
        assertFalse(store.lightningEnabled(btcMainnetWallet.id))
        var factoryCalls = 0
        var infoCalls = 0
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val session = LightningSession(
                scope = scope,
                store = store,
                nodeFactory = { factoryCalls++; FakeNode() },
                info = LnInfoApi { infoCalls++; sampleInfo() },
            )
            assertEquals(LnState.Off, session.state.value)
            session.onUnlocked(btcMainnetWallet.id, sampleSeed).join()
            assertEquals(LnState.Off, session.state.value)
            assertEquals(0, factoryCalls)
            assertEquals(0, infoCalls)
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun unlock_with_enabled_starts_and_reaches_ready() = runBlocking {
        val store = InMemoryLightningStore(mapOf(btcMainnetWallet.id to btcMainnetWallet))
        store.setLightningEnabled(btcMainnetWallet.id, true)
        var capturedCfg: LnNodeConfig? = null
        // The session wipes its copy of the seed once the node exists, so what
        // the factory was given is read inside the factory.
        var capturedSeed: List<Byte>? = null
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val session = LightningSession(
                scope = scope,
                store = store,
                nodeFactory = { cfg ->
                    capturedCfg = cfg
                    capturedSeed = cfg.bip39Seed.toList()
                    FakeNode()
                },
                info = LnInfoApi { sampleInfo() },
            )
            session.onUnlocked(btcMainnetWallet.id, sampleSeed).join()
            val st = session.state.value
            assertTrue("expected Ready, got $st", st is LnState.Ready)
            val ready = st as LnState.Ready
            assertEquals(50_000uL, ready.balances.lightningSat)
            assertEquals(LnChannelState.READY, ready.channel)
            assertNotNull(capturedCfg)
            assertEquals(sampleSeed.toList(), capturedSeed)
            assertEquals("02" + "11".repeat(32), capturedCfg!!.lsp.nodeId)
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun relock_stops_node_and_waits_for_inflight() = runBlocking {
        val store = InMemoryLightningStore(mapOf(btcMainnetWallet.id to btcMainnetWallet))
        store.setLightningEnabled(btcMainnetWallet.id, true)
        val stopEntered = CompletableDeferred<Unit>()
        val inflightSettled = CompletableDeferred<Unit>()
        var stopFinishedBeforeOff = false
        lateinit var node: FakeNode
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val session = LightningSession(
                scope = scope,
                store = store,
                nodeFactory = {
                    FakeNode(
                        onStop = {
                            stopEntered.complete(Unit)
                            runBlocking { withTimeout(5_000) { inflightSettled.await() } }
                            stopFinishedBeforeOff = true
                        },
                    ).also { node = it }
                },
                info = LnInfoApi { sampleInfo() },
            )
            session.onUnlocked(btcMainnetWallet.id, sampleSeed).join()
            assertTrue(session.state.value is LnState.Ready)

            val relockJob = session.onRelocked()
            withTimeout(5_000) { stopEntered.await() }
            // While in-flight HTLC is still settling inside stop(), relock has not finished yet.
            assertFalse(relockJob.isCompleted)
            inflightSettled.complete(Unit)
            withTimeout(5_000) { relockJob.join() }

            assertTrue(stopFinishedBeforeOff)
            assertTrue(node.stopped)
            assertEquals(LnState.Off, session.state.value)
            assertNull(session.currentNode)
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun start_failure_with_no_network_is_error_not_blocking() = runBlocking {
        val store = InMemoryLightningStore(mapOf(btcMainnetWallet.id to btcMainnetWallet))
        store.setLightningEnabled(btcMainnetWallet.id, true)
        val allowFinish = CompletableDeferred<Unit>()
        val startEntered = CompletableDeferred<Unit>()
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val session = LightningSession(
                scope = scope,
                store = store,
                nodeFactory = {
                    FakeNode(
                        onStart = {
                            startEntered.complete(Unit)
                            runBlocking { withTimeout(5_000) { allowFinish.await() } }
                            throw LnFfiException.LspUnreachable()
                        },
                    )
                },
                info = LnInfoApi { sampleInfo() },
            )
            // onUnlocked returns a Job immediately without blocking the caller thread.
            val job = session.onUnlocked(btcMainnetWallet.id, sampleSeed)
            withTimeout(5_000) { startEntered.await() }
            assertEquals(LnState.Starting, session.state.value)

            allowFinish.complete(Unit)
            withTimeout(5_000) { job.join() }
            val st = session.state.value
            assertTrue("expected Error(LspUnreachable), got $st", st is LnState.Error && st.error is LnFfiException.LspUnreachable)
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun retries_on_connectivity() = runBlocking {
        val store = InMemoryLightningStore(mapOf(btcMainnetWallet.id to btcMainnetWallet))
        store.setLightningEnabled(btcMainnetWallet.id, true)
        val attempts = AtomicInteger(0)
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val session = LightningSession(
                scope = scope,
                store = store,
                nodeFactory = {
                    FakeNode(
                        onStart = {
                            if (attempts.incrementAndGet() == 1) {
                                throw LnFfiException.LspUnreachable()
                            }
                        },
                    )
                },
                info = LnInfoApi { sampleInfo() },
            )
            session.onUnlocked(btcMainnetWallet.id, sampleSeed).join()
            assertTrue(session.state.value is LnState.Error)

            session.onConnectivityRestored().join()
            assertTrue("expected Ready after connectivity retry, got ${session.state.value}", session.state.value is LnState.Ready)
            assertEquals(2, attempts.get())
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun stale_ln_info_is_refetched_on_each_start() = runBlocking {
        val store = InMemoryLightningStore(mapOf(btcMainnetWallet.id to btcMainnetWallet))
        store.setLightningEnabled(btcMainnetWallet.id, true)
        var fetchCount = 0
        val seenNodeIds = mutableListOf<String>()
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val session = LightningSession(
                scope = scope,
                store = store,
                nodeFactory = { cfg ->
                    seenNodeIds += cfg.lsp.nodeId
                    FakeNode()
                },
                info = LnInfoApi {
                    fetchCount++
                    sampleInfo(nodeIdSuffix = if (fetchCount == 1) "11" else "22")
                },
            )
            session.onUnlocked(btcMainnetWallet.id, sampleSeed).join()
            session.onRelocked().join()
            session.onUnlocked(btcMainnetWallet.id, sampleSeed).join()

            assertEquals(2, fetchCount)
            assertEquals(listOf("02" + "11".repeat(32), "02" + "22".repeat(32)), seenNodeIds)
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun channel_closed_by_lsp_while_stopped_shows_transit_balance() = runBlocking {
        val store = InMemoryLightningStore(mapOf(btcMainnetWallet.id to btcMainnetWallet))
        store.setLightningEnabled(btcMainnetWallet.id, true)
        lateinit var node: FakeNode
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val session = LightningSession(
                scope = scope,
                store = store,
                nodeFactory = {
                    FakeNode(
                        balancesValue = LnBalances(
                            lightningSat = 0uL,
                            onchainTransitSat = 120_000uL,
                            pendingCloseSat = 0uL,
                            reservedSat = 0uL,
                            pendingTransitSat = 0uL,
                            inboundSat = 0uL,
                        ),
                        channelValue = LnChannelState.NONE,
                    ).also { node = it }
                },
                info = LnInfoApi { sampleInfo() },
                sweepAddressProvider = { "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq" },
            )
            session.onUnlocked(btcMainnetWallet.id, sampleSeed).join()
            val st = session.state.value
            assertTrue("expected Ready, got $st", st is LnState.Ready)
            val ready = st as LnState.Ready
            assertEquals(120_000uL, ready.balances.onchainTransitSat)
            assertEquals(LnChannelState.NONE, ready.channel)
            // Also verifies auto-sweep of settled transit funds was triggered to the wallet's next receive address
            assertEquals("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq", node.sweptTo)
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun only_enabled_for_software_mainnet_btc_wallet() = runBlocking {
        val watchOnly = btcMainnetWallet.copy(id = "w-watch", watchOnly = true, sealed = null, xpub = "xpub...")
        val bitbox = btcMainnetWallet.copy(id = "w-hw", watchOnly = true, hardware = true, sealed = null, xpub = "xpub...")
        val xbt = btcMainnetWallet.copy(id = "w-xbt", chain = "xbt")
        val testnet = btcMainnetWallet.copy(id = "w-test", network = "testnet")

        val ineligible = listOf(watchOnly, bitbox, xbt, testnet)
        val store = InMemoryLightningStore((ineligible + btcMainnetWallet).associateBy { it.id })
        var factoryCalls = 0
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val session = LightningSession(
                scope = scope,
                store = store,
                nodeFactory = { factoryCalls++; FakeNode() },
                info = LnInfoApi { sampleInfo() },
            )
            for (w in ineligible) {
                assertFalse("toggle must be hidden for ${w.id}", showLightningToggle(w))
                session.enable(w.id).join()
                assertFalse("must not enable in store for ${w.id}", store.lightningEnabled(w.id))
                session.onUnlocked(w.id, sampleSeed).join()
                assertEquals(LnState.Off, session.state.value)
            }
            assertEquals(0, factoryCalls)
            assertTrue(showLightningToggle(btcMainnetWallet))
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun remote_kill_switch_starts_in_exit_mode_blocks_move_and_allows_force_close_and_sweep() = runBlocking {
        val store = InMemoryLightningStore(mapOf(btcMainnetWallet.id to btcMainnetWallet))
        store.setLightningEnabled(btcMainnetWallet.id, true)
        lateinit var node: FakeNode
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val session = LightningSession(
                scope = scope,
                store = store,
                nodeFactory = {
                    FakeNode(
                        balancesValue = LnBalances(
                            lightningSat = 50_000uL,
                            onchainTransitSat = 150_000uL,
                            pendingCloseSat = 0uL,
                            reservedSat = 0uL,
                            pendingTransitSat = 0uL,
                            inboundSat = 50_000uL,
                        ),
                        channelValue = LnChannelState.READY,
                        failCoopClose = true,
                    ).also { node = it }
                },
                info = LnInfoApi {
                    sampleInfo(enabled = false, message = "Lightning maintenance in progress.")
                },
            )
            session.onUnlocked(btcMainnetWallet.id, sampleSeed).join()
            assertEquals(uniffi.wallet_ffi.LnStartMode.EXIT, node.startMode)
            assertFalse(session.lastInfo!!.enabled)
            assertEquals("Lightning maintenance in progress.", session.lastInfo!!.message)

            // Move-to-channel auto-open is suppressed while remote kill switch is active.
            session.requestMoveToChannel(100_000uL)
            session.refreshBalances()
            assertNull(node.openedChannelSat)

            // A cooperative close that cannot reach the LSP is an error, never a silent force close.
            val addr = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq"
            try {
                session.closeAndSweep(addr)
                org.junit.Assert.fail("expected the failed cooperative close to be thrown")
            } catch (e: LnFfiException.LspUnreachable) {
                // expected
            }
            assertFalse(node.forceClosed)
            assertNull(node.sweptTo)

            // The user's explicit choice does force-close, then sweeps.
            val txid = session.closeAndSweep(addr, force = true)
            assertTrue(node.forceClosed)
            assertEquals("txid-swept", txid)
            assertEquals(addr, node.sweptTo)
        } finally {
            scope.cancel()
        }
    }
}
