package com.fortis.wallet.lightning

import com.fortis.wallet.data.WalletConfig
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import uniffi.wallet_ffi.LnBalances
import uniffi.wallet_ffi.LnChannelState
import uniffi.wallet_ffi.LnFfiException
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** Regressions for the review findings on close, balance refresh and the move-to-Lightning intent. */
class LightningReviewFixesTest {
    private val seed = ByteArray(64) { (it + 1).toByte() }
    private val addr = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq"
    private val wallet = WalletConfig(
        id = "w-btc", name = "Main", chain = "ln", network = "mainnet",
        sealed = "deadbeef", salt = "cafebabe", backendToken = "tok",
    )

    private val info = LnInfo(
        nodeId = "02" + "11".repeat(32), host = "lsp.test", port = 9735, lsps2Token = "t",
        feePpm = 10_000L, feeMinSat = 1_000L, minPaymentSat = 10_000L, maxPaymentSat = 500_000L,
        channelMinSat = 100_000L, channelMaxSat = 500_000L, enabled = true, message = null,
        network = "bitcoin",
    )

    private fun bal(transit: ULong) = LnBalances(
        lightningSat = 0uL, onchainTransitSat = transit, pendingCloseSat = 0uL,
        reservedSat = 0uL, pendingTransitSat = 0uL, inboundSat = 0uL,
    )

    private open class Node(
        var transit: ULong = 0uL,
        var channel: LnChannelState = LnChannelState.NONE,
    ) : LnNodeApi {
        @Volatile var opened: ULong? = null
        @Volatile var swept: String? = null
        @Volatile var forced = false
        @Volatile var balancesThread: Thread? = null
        var closeError: Throwable? = null
        override fun start() {}
        override fun stop() {}
        override fun balances(): LnBalances {
            balancesThread = Thread.currentThread()
            return LnBalances(0uL, transit, 0uL, 0uL, 0uL, 0uL)
        }
        override fun channelState(): LnChannelState = channel
        override fun closeChannel() { closeError?.let { throw it } ; channel = LnChannelState.CLOSING }
        override fun forceCloseChannel() { forced = true; channel = LnChannelState.CLOSING }
        override fun openChannel(amountSat: ULong) { opened = amountSat; transit = 0uL }
        override fun sweepTransit(toAddress: String): String? { swept = toAddress; transit = 0uL; return "txid" }
    }

    private fun session(
        scope: CoroutineScope,
        store: InMemoryLightningStore,
        node: LnNodeApi,
        sweepTo: String? = null,
    ) = LightningSession(
        scope = scope,
        store = store,
        nodeFactory = { node },
        info = LnInfoApi { info },
        sweepAddressProvider = sweepTo?.let { a -> { _: String -> a } },
    )

    private fun newStore() = InMemoryLightningStore(mapOf(wallet.id to wallet)).also {
        runBlocking { it.setLightningEnabled(wallet.id, true) }
    }

    @Test
    fun coop_close_failure_is_thrown_and_never_forces() = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            for (err in listOf<LnFfiException>(
                LnFfiException.LspUnreachable(),
                LnFfiException.BackupUnavailable(),
                LnFfiException.Other("close_failed"),
            )) {
                val node = Node(channel = LnChannelState.READY).also { it.closeError = err }
                val s = session(scope, newStore(), node)
                s.onUnlocked(wallet.id, seed).join()
                try {
                    s.closeAndSweep(addr)
                    fail("expected $err")
                } catch (e: LnFfiException) {
                    assertEquals(err::class, e::class)
                }
                assertFalse("no force close on $err", node.forced)
            }
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun a_close_already_in_progress_is_not_an_error_and_not_forced() = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val node = Node(transit = 5_000uL, channel = LnChannelState.READY)
                .also { it.closeError = LnFfiException.Other("close_in_progress") }
            val s = session(scope, newStore(), node)
            s.onUnlocked(wallet.id, seed).join()
            assertEquals("txid", s.closeAndSweep(addr))
            assertFalse(node.forced)
            assertEquals(addr, node.swept)
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun a_closing_channel_is_not_asked_to_close_again() = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val node = object : Node(transit = 5_000uL, channel = LnChannelState.CLOSING) {
                var closeCalls = 0
                override fun closeChannel() { closeCalls++ }
            }
            val s = session(scope, newStore(), node)
            s.onUnlocked(wallet.id, seed).join()
            s.closeAndSweep(addr)
            assertEquals(0, node.closeCalls)
            assertFalse(node.forced)
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun the_explicit_force_choice_force_closes() = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val node = Node(channel = LnChannelState.READY).also { it.closeError = LnFfiException.LspUnreachable() }
            val s = session(scope, newStore(), node)
            s.onUnlocked(wallet.id, seed).join()
            s.closeAndSweep(addr, force = true)
            assertTrue(node.forced)
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun refresh_blocking_calls_run_off_the_calling_thread() = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val node = Node()
            val s = session(scope, newStore(), node)
            s.onUnlocked(wallet.id, seed).join()
            val caller = Thread.currentThread()
            s.refreshBalances()
            assertNotEquals(caller, node.balancesThread)
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun refresh_does_not_overlap_a_close_in_progress() = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val entered = CountDownLatch(1)
            val release = CountDownLatch(1)
            val node = object : Node(transit = 200_000uL, channel = LnChannelState.READY) {
                override fun closeChannel() {
                    entered.countDown()
                    release.await(10, TimeUnit.SECONDS)
                    channel = LnChannelState.CLOSING
                }
            }
            val s = session(scope, newStore(), node)
            s.onUnlocked(wallet.id, seed).join()
            s.requestMoveToChannel(100_000uL)
            val closing = async(Dispatchers.Default) { s.closeAndSweep(addr) }
            assertTrue(entered.await(5, TimeUnit.SECONDS))
            // The close holds the node: a refresh now must return at once and touch nothing.
            withTimeout(2_000) { s.refreshBalances() }
            assertNull("no channel opened while a close is running", node.opened)
            release.countDown()
            closing.await()
            Unit
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun a_broadcast_move_survives_a_restart_and_is_not_swept_back() = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val store = newStore()
            val first = session(scope, store, Node())
            first.onUnlocked(wallet.id, seed).join()
            first.commitMove(100_000uL)

            // The process dies; the deposit arrives while the app is closed.
            val node = Node(transit = 125_000uL, channel = LnChannelState.NONE) // + 25 000 sat anchor reserve
            val second = session(scope, store, node, sweepTo = addr)
            second.onUnlocked(wallet.id, seed).join()
            assertEquals(100_000uL, node.opened)
            assertNull("the deposit must not be swept back", node.swept)
            assertNull("the intent is spent", store.pendingMove(wallet.id))
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun a_move_that_was_only_requested_is_not_remembered() = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val store = newStore()
            val first = session(scope, store, Node())
            first.onUnlocked(wallet.id, seed).join()
            first.requestMoveToChannel(100_000uL) // review sheet opened, never sent
            assertNull(store.pendingMove(wallet.id))

            val node = Node(transit = 100_000uL, channel = LnChannelState.NONE)
            val second = session(scope, store, node, sweepTo = addr)
            second.onUnlocked(wallet.id, seed).join()
            assertNull("no channel the user did not confirm", node.opened)
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun a_failed_channel_open_keeps_the_move_for_a_retry() = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val store = newStore()
            var fail = true
            val node = object : Node(transit = 0uL, channel = LnChannelState.READY) {
                override fun openChannel(amountSat: ULong) {
                    if (fail) throw LnFfiException.LspUnreachable()
                    super.openChannel(amountSat)
                }
            }
            val s = session(scope, store, node, sweepTo = addr)
            s.onUnlocked(wallet.id, seed).join()
            s.commitMove(100_000uL)
            node.transit = 125_000uL
            s.refreshBalances()
            assertNull(node.opened)
            assertEquals(100_000L, store.pendingMove(wallet.id)?.amountSat)
            fail = false
            s.refreshBalances()
            assertEquals(100_000uL, node.opened)
            assertNull(store.pendingMove(wallet.id))
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun a_stale_move_intent_is_dropped() = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val store = newStore()
            val old = System.currentTimeMillis() - PENDING_MOVE_MAX_AGE_MS - 1
            store.setPendingMove(wallet.id, PendingMove(100_000L, old))
            val node = Node(transit = 100_000uL, channel = LnChannelState.NONE)
            val s = session(scope, store, node, sweepTo = addr)
            s.onUnlocked(wallet.id, seed).join()
            assertNull(node.opened)
            assertEquals(addr, node.swept)
            assertNull(store.pendingMove(wallet.id))
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun isUnlockedFor_tracks_the_held_seed() = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val s = session(scope, newStore(), Node())
            assertFalse(s.isUnlockedFor(wallet.id))
            s.onUnlocked(wallet.id, seed).join()
            assertTrue(s.isUnlockedFor(wallet.id))
            assertFalse(s.isUnlockedFor("other"))
            s.onRelocked().join()
            assertFalse(s.isUnlockedFor(wallet.id))
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun a_channel_closed_event_does_not_sweep_a_pending_move_deposit() = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val store = newStore()
            val events = java.util.concurrent.LinkedBlockingQueue<uniffi.wallet_ffi.LnEventRecord>()
            val node = object : Node(transit = 0uL, channel = LnChannelState.NONE) {
                override val blocksOnNextEvent = true
                override fun nextEvent(timeoutMs: ULong): uniffi.wallet_ffi.LnEventRecord? =
                    events.poll(timeoutMs.toLong().coerceAtMost(50), TimeUnit.MILLISECONDS)
            }
            val s = session(scope, store, node, sweepTo = addr)
            s.onUnlocked(wallet.id, seed).join()
            s.commitMove(100_000uL)
            node.transit = 50_000uL // part of the deposit has landed; not enough to open yet
            events.add(uniffi.wallet_ffi.LnEventRecord.ChannelClosed("done"))
            val deadline = System.currentTimeMillis() + 3_000
            while (events.isNotEmpty() && System.currentTimeMillis() < deadline) Thread.sleep(20)
            Thread.sleep(300)
            assertNull("the pending deposit must not be swept back", node.swept)

            // Without a pending move, the same event does sweep the orphan balance.
            store.setPendingMove(wallet.id, null)
            s.onRelocked().join()
            val other = Node(transit = 50_000uL, channel = LnChannelState.NONE)
            val s2 = session(scope, store, other, sweepTo = addr)
            s2.onUnlocked(wallet.id, seed).join()
            assertEquals(addr, other.swept)
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun the_hosted_edge_gets_the_token_and_a_servers_of_the_users_own_does_not() = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val seen = mutableListOf<Pair<String, String>>()
            fun run(esplora: String) {
                val store = InMemoryLightningStore(mapOf(wallet.id to wallet), esplora = esplora)
                runBlocking { store.setLightningEnabled(wallet.id, true) }
                val s = LightningSession(
                    scope = scope,
                    store = store,
                    nodeFactory = { cfg -> seen += cfg.esploraUrl to cfg.esploraAuthHeader; Node() },
                    info = LnInfoApi { info },
                )
                runBlocking { s.onUnlocked(wallet.id, seed).join() }
            }
            run("${com.fortis.wallet.HOSTED_EDGE}/btc")
            run("https://my-own-esplora.example/api")
            assertEquals("Bearer tok", seen[0].second)
            assertEquals("", seen[1].second)
        } finally {
            scope.cancel()
        }
    }
}
