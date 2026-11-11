package com.fortis.wallet.lightning

import com.fortis.wallet.data.WalletConfig
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.wallet_ffi.LnBalances
import uniffi.wallet_ffi.LnChannelState
import uniffi.wallet_ffi.LnEventRecord
import uniffi.wallet_ffi.LnDirection
import uniffi.wallet_ffi.LnFfiException
import uniffi.wallet_ffi.LnPayment
import uniffi.wallet_ffi.LnPaymentStatus
import uniffi.wallet_ffi.LnTarget
import java.util.concurrent.atomic.AtomicInteger

/** Seed hygiene, the idle event poll, sweep bookkeeping and the bounds on the
 *  `/ln/info` document. */
class LightningHardeningTest {
    private val seed = ByteArray(64) { (it + 1).toByte() }

    private val wallet = WalletConfig(
        id = "w-btc",
        name = "Main",
        chain = "ln",
        network = "mainnet",
        sealed = "deadbeef",
        salt = "cafebabe",
        backendToken = "tok",
    )

    private fun info() = LnInfo(
        nodeId = "02" + "11".repeat(32),
        host = "lsp.fortis.test",
        port = 9735,
        lsps2Token = "t",
        feePpm = 10_000L,
        feeMinSat = 1_000L,
        minPaymentSat = 10_000L,
        maxPaymentSat = 500_000L,
        channelMinSat = 100_000L,
        channelMaxSat = 500_000L,
    )

    private fun balances(transitSat: ULong) = LnBalances(
        lightningSat = 0uL,
        onchainTransitSat = transitSat,
        pendingCloseSat = 0uL,
        reservedSat = 0uL,
        pendingTransitSat = 0uL,
        inboundSat = 0uL,
    )

    private open class Node(
        private val bal: LnBalances,
        private val channel: LnChannelState = LnChannelState.NONE,
        private val swept: String? = "txid",
    ) : LnNodeApi {
        override fun start() {}
        override fun stop() {}
        override fun balances() = bal
        override fun channelState() = channel
        override fun sweepTransit(toAddress: String): String? = swept
    }

    private fun store() = InMemoryLightningStore(mapOf(wallet.id to wallet))

    @Test
    fun the_seed_copy_handed_to_the_node_factory_is_wiped_afterwards() = runBlocking {
        var cfgSeen: uniffi.wallet_ffi.LnNodeConfig? = null
        var seedAtCreation: List<Byte>? = null
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val session = LightningSession(
                scope = scope,
                store = store(),
                nodeFactory = { cfg ->
                    cfgSeen = cfg
                    seedAtCreation = cfg.bip39Seed.toList()
                    Node(balances(0uL))
                },
                info = LnInfoApi { info() },
                chain = uniffi.wallet_ffi.LnChain.BTC,
            )
            session.onUnlocked(wallet.id, seed).join()
            assertEquals("the factory saw the real seed", seed.toList(), seedAtCreation)
            assertTrue("then it was wiped", cfgSeen!!.bip39Seed.all { it == 0.toByte() })
            assertEquals("the caller's own array is the caller's to wipe", seed.toList(), ByteArray(64) { (it + 1).toByte() }.toList())
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun the_seed_copy_is_wiped_when_the_factory_fails_too() = runBlocking {
        var cfgSeen: uniffi.wallet_ffi.LnNodeConfig? = null
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val session = LightningSession(
                scope = scope,
                store = store(),
                nodeFactory = { cfg ->
                    cfgSeen = cfg
                    throw LnFfiException.BackupUnavailable()
                },
                info = LnInfoApi { info() },
                chain = uniffi.wallet_ffi.LnChain.BTC,
            )
            session.onUnlocked(wallet.id, seed).join()
            assertTrue(session.state.value is LnState.Error)
            assertTrue(cfgSeen!!.bip39Seed.all { it == 0.toByte() })
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun an_idle_event_poll_that_returns_at_once_does_not_spin() = runBlocking {
        val polls = AtomicInteger()
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val session = LightningSession(
                scope = scope,
                store = store(),
                nodeFactory = {
                    object : Node(balances(0uL)) {
                        override val blocksOnNextEvent = true
                        override fun nextEvent(timeoutMs: ULong): LnEventRecord? {
                            polls.incrementAndGet()
                            return null // returns at once: the node is not waiting
                        }
                    }
                },
                info = LnInfoApi { info() },
                chain = uniffi.wallet_ffi.LnChain.BTC,
            )
            session.onUnlocked(wallet.id, seed).join()
            delay(900)
            val n = polls.get()
            // With a 250 ms back-off that is a handful of calls; a busy loop
            // is tens of thousands.
            assertTrue("polled $n times in 0.9 s", n in 1..12)
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun a_sweep_that_was_broadcast_reports_it_and_one_that_was_not_does_not() = runBlocking {
        for ((txid, expected) in listOf("txid" to 1, null to 0)) {
            val reported = mutableListOf<String>()
            val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
            try {
                val session = LightningSession(
                    scope = scope,
                    store = store(),
                    nodeFactory = { Node(balances(70_000uL), swept = txid) },
                    info = LnInfoApi { info() },
                    sweepAddressProvider = { "bc1qexample" },
                    onSweepBroadcast = { reported += it },
                    chain = uniffi.wallet_ffi.LnChain.BTC,
                )
                session.onUnlocked(wallet.id, seed).join()
                assertEquals("txid=$txid", expected, reported.size)
                if (expected == 1) assertEquals(wallet.id, reported.single())
            } finally {
                scope.cancel()
            }
        }
    }

    // ---- double-payment guard for targets that yield a fresh invoice each time ----

    private fun outbound(status: LnPaymentStatus) = LnPayment(
        id = "p1", direction = LnDirection.OUT, amountMsat = 10_000uL, feeMsat = 0uL,
        status = status, description = "", timestamp = 0uL,
    )

    private class PayingNode(
        bal: LnBalances,
        private val payments: List<LnPayment>,
    ) : Node(bal, LnChannelState.READY) {
        var paid = 0
        override fun payments() = payments
        // An LNURL / address resolves to a BTC invoice (the session checks its chain, then pays it).
        override fun lnurlInvoice(target: LnTarget, amountMsat: ULong): String = BTC_INVOICE
        override fun pay(target: LnTarget, amountMsat: ULong?): String {
            paid++
            return "p2"
        }
    }

    private suspend fun payWith(payments: List<LnPayment>, target: LnTarget): Pair<Result<String>, Int> {
        val node = PayingNode(balances(0uL), payments)
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val session = LightningSession(scope = scope, store = store(), nodeFactory = { node }, info = LnInfoApi { info() }, chain = uniffi.wallet_ffi.LnChain.BTC)
            session.onUnlocked(wallet.id, seed).join()
            return runCatching { session.pay(target, 10_000uL) } to node.paid
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun a_fresh_invoice_target_waits_while_an_outbound_payment_is_pending() = runBlocking {
        val targets = listOf(
            LnTarget.LnurlPay(url = "https://example.com/p"),
            LnTarget.LightningAddress(user = "bob", domain = "example.com"),
            LnTarget.Bolt12Offer(offer = "lno1x", amountMsat = null, description = ""),
        )
        for (t in targets) {
            val (r, paid) = payWith(listOf(outbound(LnPaymentStatus.PENDING)), t)
            assertTrue("$t: $r", r.exceptionOrNull() is LnFfiException.PaymentStateUnknown)
            assertEquals("nothing was sent for $t", 0, paid)
        }
    }

    @Test
    fun the_wait_ends_when_nothing_is_pending_and_bolt11_is_left_to_rust() = runBlocking {
        val lnurl = LnTarget.LnurlPay(url = "https://example.com/p")
        for (done in listOf(LnPaymentStatus.SUCCEEDED, LnPaymentStatus.FAILED)) {
            val (r, paid) = payWith(listOf(outbound(done)), lnurl)
            assertTrue("$done: $r", r.isSuccess)
            assertEquals(1, paid)
        }
        // BOLT11 has its own guard (the payment hash, in Rust): not blocked here.
        val b11 = LnTarget.Bolt11(invoice = BTC_INVOICE, amountMsat = 10_000uL, description = "", expirySecs = 3600uL)
        val (r, paid) = payWith(listOf(outbound(LnPaymentStatus.PENDING)), b11)
        assertTrue("$r", r.isSuccess)
        assertEquals(1, paid)
    }

    private companion object {
        /** A BTC (no bit 512) mainnet invoice: the only kind a BTC session pays. */
        const val BTC_INVOICE =
            "lnbc10u1pj48ugqdqvve5hsar4wfjspp5wtxkappzcsrlkmgfs6g0zyct0hkhashh7hsaxz7e65slq9fkx7fssp5qurswpc8qurswpc8qurswpc8qurswpc8qurswpc8qurswpc8qurs9qyysgqcqzysug9w4fmxjxs5d7ns8mhwf4yee4ku2r0yw4kx52hlu5mj0mtxys6rauzqc3fkpku2utdmc7y4hsg6cc9lafr5s0xzd9zcn5twv3qddyqq777d99"
    }

    // ---- /ln/info bounds ----------------------------------------------------

    private fun infoJson(
        maxPaymentSat: Long = 500_000,
        channelMaxSat: Long = 500_000,
        feeMinSat: Long = 1_000,
    ) = """
        {"node_id":"02${"11".repeat(32)}","host":"lsp.fortis.test","port":9735,
         "fee":{"ppm":10000,"min_sat":$feeMinSat},
         "min_payment_sat":10000,"max_payment_sat":$maxPaymentSat,
         "channel_min_sat":100000,"channel_max_sat":$channelMaxSat,
         "enabled":true,"network":"bitcoin"}
    """.trimIndent()

    @Test
    fun info_amounts_are_held_to_the_total_supply() {
        parseLnInfo(infoJson()) // sane values pass
        val supply = 21_000_000L * 100_000_000L
        parseLnInfo(infoJson(maxPaymentSat = supply, channelMaxSat = supply)) // the edge
        for (bad in listOf(
            infoJson(maxPaymentSat = supply + 1),
            infoJson(maxPaymentSat = Long.MAX_VALUE),
            infoJson(channelMaxSat = supply + 1),
            infoJson(channelMaxSat = Long.MAX_VALUE),
            infoJson(feeMinSat = supply + 1),
            infoJson(feeMinSat = -1),
        )) {
            assertTrue("accepted: $bad", runCatching { parseLnInfo(bad) }.isFailure)
        }
        // And the msat conversion of an accepted value stays inside a ULong.
        val limits = parseLnInfo(infoJson(maxPaymentSat = supply, channelMaxSat = supply)).toLspLimits()
        assertEquals((supply.toULong()) * 1_000uL, limits.maxPaymentMsat)
    }
}
