package com.fortis.wallet.lightning

import com.fortis.wallet.data.WalletConfig
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.wallet_ffi.LnBalances
import uniffi.wallet_ffi.LnChain
import uniffi.wallet_ffi.LnChannelState
import uniffi.wallet_ffi.LnFfiException

/**
 * 2026-11-16: the XBT edge refused a channel's funding transaction (402). The phone's node
 * logged it and went on: the channel stayed pending for ever and the Move said "Moving…".
 * The session now reports a failed open, notices a pending funding the network never got, and
 * can cancel such a channel and return the funds.
 */
class StuckChannelSessionTest {
    private val seed = ByteArray(64) { (it + 1).toByte() }
    private val wallet = WalletConfig(
        id = "w-xbt", name = "LN", chain = "ln-xbt", network = "mainnet",
        sealed = "deadbeef", salt = "cafebabe", backendToken = "tok",
    )
    private val info = LnInfo(
        nodeId = "02" + "11".repeat(32), host = "lsp.test", port = 9835, lsps2Token = null,
        feePpm = 10_000L, feeMinSat = 1_000L, minPaymentSat = 51_000L, maxPaymentSat = 250_000L,
        channelMinSat = 100_000L, channelMaxSat = 500_000L, enabled = true, message = null, network = "bitcoin",
    )

    private class Node(
        var transitSat: ULong = 0uL,
        var channel: LnChannelState = LnChannelState.NONE,
        var pendingFunding: String? = null,
        var openError: Throwable? = null,
        var sweepError: Throwable? = null,
    ) : LnNodeApi {
        var forceClosed = false
        var sweptTo: String? = null
        var opened: ULong? = null
        override fun start() {}
        override fun stop() {}
        override fun balances() = LnBalances(
            lightningSat = if (channel == LnChannelState.OPENING) 143_607uL else 0uL,
            onchainTransitSat = transitSat, pendingCloseSat = 0uL, reservedSat = 0uL,
            pendingTransitSat = 0uL, inboundSat = 0uL,
        )
        override fun channelState() = channel
        override fun pendingFundingTxid() = pendingFunding
        override fun openChannel(amountSat: ULong) {
            openError?.let { throw it }
            opened = amountSat
        }
        override fun forceCloseChannel() { forceClosed = true; channel = LnChannelState.CLOSING; pendingFunding = null }
        override fun sweepTransit(toAddress: String): String? {
            sweepError?.let { throw it }
            sweptTo = toAddress; return "swept-tx"
        }
    }

    private class Env(val session: LightningSession, val store: InMemoryLightningStore, val node: Node, val scope: CoroutineScope, val clock: LongArray)

    private val cancelled = mutableListOf<String>()

    private fun env(
        node: Node,
        onNetwork: (String) -> Boolean?,
        reported: MutableList<Pair<String, Throwable>> = mutableListOf(),
        selfPolling: Boolean = false,
        autoCancel: Boolean = false,
    ): Env {
        val store = InMemoryLightningStore(mapOf(wallet.id to wallet))
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        val clock = longArrayOf(1_000_000L)
        val session = LightningSession(
            scope = scope, store = store, nodeFactory = { node }, info = LnInfoApi { info },
            chain = LnChain.XBT,
            pollIntervalMs = if (selfPolling) 20L else 0L,
            autoCancelStuck = autoCancel,
            onStuckCancelled = { cancelled += it },
            fundingOnNetwork = { onNetwork(it) },
            nowMs = { clock[0] },
            sweepAddressProvider = { "xbt-addr" },
        )
        session.errorReporter = { w, e -> reported += w to e }
        return Env(session, store, node, scope, clock)
    }

    private suspend fun Env.ready() {
        session.onUnlocked(wallet.id, seed).join()
        assertTrue(session.state.value is LnState.Ready)
    }

    @Test
    fun aFailedChannelOpenIsReportedNotSwallowed() = runBlocking {
        val reported = mutableListOf<Pair<String, Throwable>>()
        val e = env(Node(transitSat = 144_590uL, openError = LnFfiException.Other("boom")), { null }, reported)
        try {
            e.session.onUnlocked(wallet.id, seed).join()
            e.session.commitTransfer(2)
            e.session.refreshBalances()
            assertTrue("the transfer stays pending to retry", e.session.transferPending.value)
            val err = e.session.transferError.value
            assertNotNull("the failure must be visible", err)
            assertTrue(err!!, err.contains("boom"))
            assertEquals(listOf("channel_open"), reported.map { it.first })
            // The same failure on the next refresh is not reported again.
            e.session.refreshBalances()
            assertEquals(1, reported.size)
        } finally {
            e.scope.cancel()
        }
    }

    @Test
    fun aSuccessfulOpenClearsTheError() = runBlocking {
        val node = Node(transitSat = 144_590uL, openError = LnFfiException.Other("boom"))
        val e = env(node, { null })
        try {
            e.session.onUnlocked(wallet.id, seed).join()
            e.session.commitTransfer(2)
            e.session.refreshBalances()
            assertNotNull(e.session.transferError.value)
            node.openError = null
            e.session.refreshBalances()
            assertNull(e.session.transferError.value)
            assertNotNull(node.opened)
        } finally {
            e.scope.cancel()
        }
    }

    @Test
    fun aPendingFundingTheNetworkLacksBecomesStuckAfterTheGrace() = runBlocking {
        val e = env(Node(channel = LnChannelState.OPENING, pendingFunding = "be62e143"), { false })
        try {
            e.ready()
            e.session.refreshBalances()
            assertFalse("not at once: the index may be a moment behind", e.session.stuckChannel.value)
            e.clock[0] += STUCK_AFTER_MS - 1
            e.session.refreshBalances()
            assertFalse(e.session.stuckChannel.value)
            e.clock[0] += 1
            e.session.refreshBalances()
            assertTrue(e.session.stuckChannel.value)
        } finally {
            e.scope.cancel()
        }
    }

    @Test
    fun aFundingTheNetworkHasIsNeverStuck() = runBlocking {
        val e = env(Node(channel = LnChannelState.OPENING, pendingFunding = "be62e143"), { true })
        try {
            e.ready()
            e.session.refreshBalances()
            e.clock[0] += 10 * STUCK_AFTER_MS
            e.session.refreshBalances()
            assertFalse(e.session.stuckChannel.value)
        } finally {
            e.scope.cancel()
        }
    }

    @Test
    fun anUnknownAnswerIsNeverStuck() = runBlocking {
        val e = env(Node(channel = LnChannelState.OPENING, pendingFunding = "be62e143"), { null })
        try {
            e.ready()
            e.session.refreshBalances()
            e.clock[0] += 10 * STUCK_AFTER_MS
            e.session.refreshBalances()
            assertFalse("offline is not evidence", e.session.stuckChannel.value)
        } finally {
            e.scope.cancel()
        }
    }

    @Test
    fun cancellingAStuckChannelForceClosesDropsTheTransferAndSweepsTheCoinsBack() = runBlocking {
        val node = Node(transitSat = 144_590uL, channel = LnChannelState.OPENING, pendingFunding = "be62e143")
        val e = env(node, { false })
        try {
            e.ready()
            e.session.commitTransfer(2)
            e.session.refreshBalances()
            e.clock[0] += STUCK_AFTER_MS
            e.session.refreshBalances()
            assertTrue(e.session.stuckChannel.value)

            val txid = e.session.cancelStuckChannel("xbt-addr")

            assertTrue("force close, never a cooperative close of an unfunded channel", node.forceClosed)
            assertEquals("xbt-addr", node.sweptTo)
            assertEquals("swept-tx", txid)
            assertFalse(e.session.stuckChannel.value)
            assertFalse("no retry of the dead open", e.session.transferPending.value)
            assertNull(e.store.pendingTransfer(wallet.id))
            assertNull(e.session.transferError.value)
        } finally {
            e.scope.cancel()
        }
    }

    @Test
    fun cancelRefusesWhenTheNetworkHasTheFundingAfterAll() = runBlocking {
        var answer: Boolean? = false
        val node = Node(transitSat = 144_590uL, channel = LnChannelState.OPENING, pendingFunding = "be62e143")
        val e = env(node, { answer })
        try {
            e.ready()
            e.session.refreshBalances()
            e.clock[0] += STUCK_AFTER_MS
            e.session.refreshBalances()
            assertTrue(e.session.stuckChannel.value)
            answer = true // the index caught up before the user tapped
            val failed = runCatching { e.session.cancelStuckChannel("xbt-addr") }.exceptionOrNull()
            assertNotNull("must refuse", failed)
            assertFalse("a live channel is never force closed", node.forceClosed)
            assertFalse(e.session.stuckChannel.value)
        } finally {
            e.scope.cancel()
        }
    }

    @Test
    fun cancelRefusesWhenTheNetworkCannotBeAsked() = runBlocking {
        var answer: Boolean? = false
        val node = Node(channel = LnChannelState.OPENING, pendingFunding = "be62e143")
        val e = env(node, { answer })
        try {
            e.ready()
            e.session.refreshBalances()
            e.clock[0] += STUCK_AFTER_MS
            e.session.refreshBalances()
            answer = null // offline now
            assertNotNull(runCatching { e.session.cancelStuckChannel("xbt-addr") }.exceptionOrNull())
            assertFalse(node.forceClosed)
        } finally {
            e.scope.cancel()
        }
    }

    /** 2026-11-16: nothing called refreshBalances while the Lightning wallet was on screen, so the
     *  check never ran. The session now polls by itself while its node runs. */
    @Test
    fun theSessionNoticesAStuckChannelOnItsOwnWithoutAnyoneRefreshing() = runBlocking {
        val node = Node(channel = LnChannelState.OPENING, pendingFunding = "be62e143")
        val e = env(node, { false }, selfPolling = true)
        try {
            e.ready()
            kotlinx.coroutines.withTimeout(5_000) {
                while (!e.session.stuckChannel.value) {
                    e.clock[0] += STUCK_AFTER_MS // time passes between the session's own polls
                    kotlinx.coroutines.delay(20)
                }
            }
            assertTrue(e.session.stuckChannel.value)
        } finally {
            e.scope.cancel()
        }
    }

    @Test
    fun theSessionRetriesAFailedOpenOnItsOwn() = runBlocking {
        val node = Node(transitSat = 144_590uL, openError = LnFfiException.Other("boom"))
        val e = env(node, { null }, selfPolling = true)
        try {
            e.session.onUnlocked(wallet.id, seed).join()
            e.session.commitTransfer(2)
            kotlinx.coroutines.withTimeout(5_000) {
                while (e.session.transferError.value == null) kotlinx.coroutines.delay(20)
            }
            node.openError = null
            kotlinx.coroutines.withTimeout(5_000) {
                while (node.opened == null) kotlinx.coroutines.delay(20)
            }
            assertNotNull(node.opened)
        } finally {
            e.scope.cancel()
        }
    }

    private suspend fun waitFor(what: String, cond: () -> Boolean) {
        kotlinx.coroutines.withTimeout(5_000) { while (!cond()) kotlinx.coroutines.delay(20) }
    }

    /** The user's rule: a stuck channel cancels itself and returns the funds, without asking. */
    @Test
    fun aStuckChannelCancelsItselfAndReturnsTheFunds() = runBlocking {
        val node = Node(transitSat = 144_590uL, channel = LnChannelState.OPENING, pendingFunding = "be62e143")
        val e = env(node, { false }, selfPolling = true, autoCancel = true)
        try {
            e.session.onUnlocked(wallet.id, seed).join()
            e.session.commitTransfer(2)
            waitFor("auto cancel") { e.clock[0] += STUCK_AFTER_MS; node.forceClosed && node.sweptTo != null }
            assertEquals("xbt-addr", node.sweptTo)
            assertNull(e.store.pendingTransfer(wallet.id))
            waitFor("cleanup callback") { cancelled.isNotEmpty() }
            assertEquals(listOf(wallet.id), cancelled)
            assertFalse(e.session.stuckChannel.value)
        } finally {
            e.scope.cancel()
        }
    }

    @Test
    fun aChannelTheNetworkHasIsNeverAutoCancelled() = runBlocking {
        val node = Node(channel = LnChannelState.OPENING, pendingFunding = "be62e143")
        val e = env(node, { true }, selfPolling = true, autoCancel = true)
        try {
            e.ready()
            repeat(10) { e.clock[0] += STUCK_AFTER_MS; kotlinx.coroutines.delay(30) }
            assertFalse(node.forceClosed)
        } finally {
            e.scope.cancel()
        }
    }

    @Test
    fun withNoWalletToReturnFundsToTheNoticeStaysForTheUser() = runBlocking {
        val node = Node(transitSat = 144_590uL, channel = LnChannelState.OPENING, pendingFunding = "be62e143")
        val store = InMemoryLightningStore(mapOf(wallet.id to wallet))
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        val clock = longArrayOf(1_000_000L)
        val session = LightningSession(
            scope = scope, store = store, nodeFactory = { node }, info = LnInfoApi { info }, chain = LnChain.XBT,
            fundingOnNetwork = { false }, nowMs = { clock[0] }, pollIntervalMs = 20L, autoCancelStuck = true,
            sweepAddressProvider = { null },
        )
        try {
            session.onUnlocked(wallet.id, seed).join()
            waitFor("stuck") { clock[0] += STUCK_AFTER_MS; session.stuckChannel.value }
            kotlinx.coroutines.delay(200)
            assertFalse("no destination: never closed blindly", node.forceClosed)
            assertTrue(session.stuckChannel.value)
        } finally {
            scope.cancel()
        }
    }

    /** 2026-11-16: right after a force close the node refuses the sweep ("closing") until it has
     *  proved the dead channel's funding void. That is expected: the cancel must not fail, must
     *  say the funds are on their way, and the node's own orphan sweep finishes the job. */
    @Test
    fun aSweepRefusedAsStillClosingIsNotAFailureAndIsFinishedLater() = runBlocking {
        val node = Node(transitSat = 144_590uL, channel = LnChannelState.OPENING, pendingFunding = "be62e143",
            sweepError = LnFfiException.Other("closing"))
        val e = env(node, { false })
        try {
            e.ready()
            e.session.commitTransfer(2)
            e.session.refreshBalances()
            e.clock[0] += STUCK_AFTER_MS
            e.session.refreshBalances()
            val txid = e.session.cancelStuckChannel("xbt-addr")
            assertNull("nothing swept yet", txid)
            assertTrue(node.forceClosed)
            assertTrue("the UI says the funds are returning", e.session.cancelling.value)
            assertFalse(e.session.stuckChannel.value)

            // The node proves the funding void: the channel is gone, the coins are sweepable.
            node.channel = LnChannelState.NONE
            node.sweepError = null
            e.session.refreshBalances()
            assertEquals("xbt-addr", node.sweptTo)
            node.transitSat = 0uL
            e.session.refreshBalances()
            assertFalse("done: the notice goes", e.session.cancelling.value)
        } finally {
            e.scope.cancel()
        }
    }
}
