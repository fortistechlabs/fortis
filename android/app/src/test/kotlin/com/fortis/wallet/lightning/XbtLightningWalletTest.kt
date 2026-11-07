package com.fortis.wallet.lightning

import com.fortis.wallet.R
import com.fortis.wallet.data.LN_CHAIN
import com.fortis.wallet.data.LN_XBT_CHAIN
import com.fortis.wallet.data.WalletConfig
import com.fortis.wallet.ui.LightningLinkRoute
import com.fortis.wallet.ui.lightningLinkRoute
import com.fortis.wallet.wallet.LightningWalletExists
import com.fortis.wallet.wallet.checkCanAddLightning
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import org.w3c.dom.Element
import uniffi.wallet_ffi.LnBalances
import uniffi.wallet_ffi.LnChain
import uniffi.wallet_ffi.LnChannelState
import uniffi.wallet_ffi.LnFfiException
import uniffi.wallet_ffi.LnNodeConfig
import uniffi.wallet_ffi.LnTarget
import uniffi.wallet_ffi.LnTargetChain
import java.io.File
import java.lang.reflect.Modifier
import javax.xml.parsers.DocumentBuilderFactory

/** The Lightning (XBT) wallet next to the BTC one (Plan C Task 6). */
class XbtLightningWalletTest {
    /** 1,000 sat mainnet invoices signed with a fixed key at 1_700_000_000 (crates/wallet-ffi
     *  tests' `fixture_invoice`); the XBT one has feature bit 512 (option_blake2b). */
    private val xbtInvoice =
        "lnbc10u1pj48ugqdqvve5hsar4wfjspp5wtxkappzcsrlkmgfs6g0zyct0hkhashh7hsaxz7e65slq9fkx7fssp5qurswpc8qurswpc8qurswpc8qurswpc8qurswpc8qurswpc8qurs9r8yqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqysgqcqzyslr2vvjskhlv2zfstdgzqh632n6k8tgk3ruar2g3v73l83ss2fdspelafxgusqvcge7rcuvjmnzp4yp5nxucjg5rxnscafha8lh82fzqpl5akyw"
    private val btcInvoice =
        "lnbc10u1pj48ugqdqvve5hsar4wfjspp5wtxkappzcsrlkmgfs6g0zyct0hkhashh7hsaxz7e65slq9fkx7fssp5qurswpc8qurswpc8qurswpc8qurswpc8qurswpc8qurswpc8qurs9qyysgqcqzysug9w4fmxjxs5d7ns8mhwf4yee4ku2r0yw4kx52hlu5mj0mtxys6rauzqc3fkpku2utdmc7y4hsg6cc9lafr5s0xzd9zcn5twv3qddyqq777d99"
    private val fixtureNow = 1_700_000_010uL
    private val seed = ByteArray(64) { 1 }

    private val btc = WalletConfig("b", "Main", "btc", "mainnet", sealed = "aa", salt = "bb")
    private val xbt = WalletConfig("x", "Main", "xbt", "mainnet", sealed = "aa", salt = "bb")
    private val ln = WalletConfig("l", "Spend", LN_CHAIN, "bitcoin", sealed = "cc", salt = "dd")
    private val lnXbt = WalletConfig("lx", "Spend", LN_XBT_CHAIN, "bitcoin", sealed = "ee", salt = "ff")

    private fun target(invoice: String) =
        LnTarget.Bolt11(invoice = invoice, amountMsat = 1_000_000uL, description = "fixture", expirySecs = 3600uL)

    // ---- adding the wallet ---------------------------------------------------

    @Test
    fun can_add_xbt_only_when_enabled_or_debug() {
        assertFalse(canAddLightningWallet(listOf(btc), LN_XBT_CHAIN, xbtEnabled = false, debug = false))
        assertTrue(canAddLightningWallet(listOf(btc), LN_XBT_CHAIN, xbtEnabled = true, debug = false))
        assertTrue(canAddLightningWallet(listOf(btc), LN_XBT_CHAIN, xbtEnabled = false, debug = true))
        // The BTC Lightning wallet never depends on the XBT switch.
        assertTrue(canAddLightningWallet(listOf(btc), LN_CHAIN, xbtEnabled = false, debug = false))
        // On-chain chains are not Lightning wallets.
        assertFalse(canAddLightningWallet(listOf(btc), "xbt", xbtEnabled = true, debug = true))
    }

    @Test
    fun one_xbt_lightning_wallet_per_app() {
        assertFalse(canAddLightningWallet(listOf(btc, lnXbt), LN_XBT_CHAIN, xbtEnabled = true, debug = true))
        // Each chain has its own one: either can be added while only the other exists.
        assertTrue(canAddLightningWallet(listOf(btc, lnXbt), LN_CHAIN, xbtEnabled = false, debug = false))
        assertTrue(canAddLightningWallet(listOf(btc, ln), LN_XBT_CHAIN, xbtEnabled = true, debug = false))
        assertFalse(canAddLightningWallet(listOf(btc, ln), LN_CHAIN, xbtEnabled = true, debug = true))
        try {
            checkCanAddLightning(listOf(lnXbt), LN_XBT_CHAIN, xbtEnabled = true, debug = true)
            fail("a second Lightning (XBT) wallet was allowed")
        } catch (_: LightningWalletExists) {}
        checkCanAddLightning(listOf(ln), LN_XBT_CHAIN, xbtEnabled = true, debug = false)
        assertEquals(lnXbt, lightningWalletOf(listOf(btc, ln, lnXbt), LN_XBT_CHAIN))
        assertEquals(ln, lightningWalletOf(listOf(btc, lnXbt, ln), LN_CHAIN))
        assertNull(lightningWalletOf(listOf(btc, ln), LN_XBT_CHAIN))
        // Both are Lightning wallets (no on-chain session, no xpub), each labelled by its chain.
        assertTrue(lnXbt.isLightning)
        assertTrue(isLightningWallet(lnXbt))
        assertEquals("Lightning (XBT) · Spend", lnXbt.display)
        assertEquals("Lightning · Spend", ln.display)
        assertNull(lnXbt.otherChain)
    }

    // ---- wrong chain ------------------------------------------------------------

    private val english: Map<String, String> by lazy {
        val doc = DocumentBuilderFactory.newInstance().newDocumentBuilder()
            .parse(File("src/main/res/values/strings.xml")).documentElement
        val list = doc.getElementsByTagName("string")
        (0 until list.length).associate { i -> (list.item(i) as Element).let { it.getAttribute("name") to it.textContent } }
    }

    private fun str(res: Int): String {
        val name = R.string::class.java.declaredFields.first { f ->
            Modifier.isStatic(f.modifiers) && f.type == Int::class.javaPrimitiveType && f.getInt(null) == res
        }.name
        return english.getValue(name)
    }

    @Test
    fun wrong_chain_messages_exact_copy() {
        assertEquals("This is an XBT Lightning invoice", wrongChainMessage(LN_CHAIN, LnTargetChain.XBT, isOffer = false, ::str))
        assertEquals("This is an XBT Lightning offer", wrongChainMessage(LN_CHAIN, LnTargetChain.XBT, isOffer = true, ::str))
        assertEquals("This is a BTC Lightning invoice", wrongChainMessage(LN_XBT_CHAIN, LnTargetChain.BTC, isOffer = false, ::str))
        assertEquals("This is a BTC Lightning offer", wrongChainMessage(LN_XBT_CHAIN, LnTargetChain.BTC, isOffer = true, ::str))
        // A match is no refusal.
        assertNull(wrongChainMessage(LN_CHAIN, LnTargetChain.BTC, isOffer = false, ::str))
        assertNull(wrongChainMessage(LN_XBT_CHAIN, LnTargetChain.XBT, isOffer = true, ::str))
        // Unknown is never a permission: no bit 512 is BTC to the XBT wallet.
        assertEquals("This is a BTC Lightning invoice", wrongChainMessage(LN_XBT_CHAIN, LnTargetChain.UNKNOWN, isOffer = false, ::str))
        assertEquals(str(R.string.ln_err_wrong_chain), wrongChainMessage(LN_CHAIN, LnTargetChain.UNKNOWN, isOffer = false, ::str))
        // "Pay from …" names the other wallet; the QR caption.
        assertEquals("Pay from your Lightning (XBT) wallet", str(payFromOtherWalletRes(LN_XBT_CHAIN)))
        assertEquals("Pay from your Lightning wallet", str(payFromOtherWalletRes(LN_CHAIN)))
        assertEquals("XBT Lightning", str(R.string.ln_qr_caption_xbt))
        assertEquals("Lightning (XBT)", str(R.string.wallet_chain_lightning_xbt))
    }

    @Test
    fun an_invoice_is_checked_against_the_wallet_chain() {
        val now = fixtureNow
        val xbtTarget = uniffi.wallet_ffi.lnParseAt(xbtInvoice, "bitcoin", now)
        val btcTarget = uniffi.wallet_ffi.lnParseAt(btcInvoice, "bitcoin", now)
        assertEquals(LnTargetChain.XBT, uniffi.wallet_ffi.lnTargetChain(xbtTarget))
        assertEquals(LnTargetChain.BTC, uniffi.wallet_ffi.lnTargetChain(btcTarget))
        assertNull(targetChainRefusal(LN_CHAIN, btcTarget))
        assertNull(targetChainRefusal(LN_XBT_CHAIN, xbtTarget))
        assertEquals(LnTargetChain.XBT, targetChainRefusal(LN_CHAIN, xbtTarget))
        assertEquals(LnTargetChain.BTC, targetChainRefusal(LN_XBT_CHAIN, btcTarget))
        // Unparseable (Unknown) is refused by both wallets.
        val junk = LnTarget.Bolt11(invoice = "lnbc1", amountMsat = null, description = "", expirySecs = 0uL)
        assertEquals(LnTargetChain.UNKNOWN, targetChainRefusal(LN_CHAIN, junk))
        assertEquals(LnTargetChain.UNKNOWN, targetChainRefusal(LN_XBT_CHAIN, junk))
    }

    // ---- paying: the session refuses before anything reaches the node ------------

    private open class Node(
        var bal: LnBalances = balances(lightningSat = 50_000uL),
        var ch: LnChannelState = LnChannelState.READY,
        private val transit: String = "",
        private val lnurlInvoiceValue: String = "",
    ) : LnNodeApi {
        val paid = mutableListOf<Pair<LnTarget, ULong?>>()
        var lnurlFetches = 0
        var sweptTo: String? = null
        override fun start() {}
        override fun stop() {}
        override fun balances() = bal
        override fun channelState() = ch
        override fun transitAddress() = transit
        override fun lnurlInvoice(target: LnTarget, amountMsat: ULong): String {
            lnurlFetches++
            return lnurlInvoiceValue
        }
        override fun pay(target: LnTarget, amountMsat: ULong?): String {
            paid += target to amountMsat
            return "id-${paid.size}"
        }
        override fun sweepTransit(toAddress: String): String? {
            sweptTo = toAddress
            bal = balances(0uL)
            return "sweep-txid"
        }
    }

    private fun info(network: String = "bitcoin") = LnInfo(
        nodeId = "02" + "11".repeat(32), host = "lsp.test", port = 9735, lsps2Token = null,
        feePpm = 10_000L, feeMinSat = 1_000L, minPaymentSat = 10_000L, maxPaymentSat = 500_000L,
        channelMinSat = 100_000L, channelMaxSat = 500_000L, network = network,
    )

    private fun <T> withSession(
        chain: LnChain,
        node: Node,
        wallets: List<WalletConfig> = listOf(ln, lnXbt),
        sweep: (suspend (String) -> String?)? = null,
        body: suspend (LightningSession) -> T,
    ): T = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val s = LightningSession(
                scope = scope,
                store = InMemoryLightningStore(wallets.associateBy { it.id }),
                nodeFactory = { node },
                info = LnInfoApi { info() },
                sweepAddressProvider = sweep,
                chain = chain,
            )
            val w = wallets.first { it.chain == (if (chain == LnChain.XBT) LN_XBT_CHAIN else LN_CHAIN) }
            s.onUnlocked(w.id, seed).join()
            body(s)
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun lnurl_invoice_for_other_chain_refused() {
        for (t in listOf(LnTarget.LightningAddress("bob", "example.com"), LnTarget.LnurlPay("https://example.com/p"))) {
            val node = Node(lnurlInvoiceValue = xbtInvoice)
            val r = withSession(LnChain.BTC, node) { s -> runCatching { s.pay(t, 1_000_000uL) } }
            assertTrue("$t: $r", r.exceptionOrNull() is LnFfiException.WrongChain)
            assertEquals("the invoice was fetched", 1, node.lnurlFetches)
            assertTrue("pay must never be called: ${node.paid}", node.paid.isEmpty())
        }
        // And the reverse: the XBT wallet refuses a BTC invoice an address returned.
        val node = Node(lnurlInvoiceValue = btcInvoice)
        val r = withSession(LnChain.XBT, node) { s -> runCatching { s.pay(LnTarget.LightningAddress("bob", "example.com"), 1_000_000uL) } }
        assertTrue("$r", r.exceptionOrNull() is LnFfiException.WrongChain)
        assertTrue(node.paid.isEmpty())
    }

    @Test
    fun lnurl_invoice_for_own_chain_pays_that_invoice() {
        val node = Node(lnurlInvoiceValue = btcInvoice)
        val r = withSession(LnChain.BTC, node) { s -> runCatching { s.pay(LnTarget.LightningAddress("bob", "example.com"), 1_000_000uL) } }
        assertTrue("$r", r.isSuccess)
        val (paidTarget, paidAmount) = node.paid.single()
        // The invoice that was checked is the one paid (its own amount).
        assertEquals(btcInvoice, (paidTarget as LnTarget.Bolt11).invoice)
        assertNull(paidAmount)
    }

    @Test
    fun invoices_and_offers_for_other_chain_refused_before_paying() {
        val btcNode = Node()
        val r1 = withSession(LnChain.BTC, btcNode) { s -> runCatching { s.pay(target(xbtInvoice), null) } }
        assertTrue("$r1", r1.exceptionOrNull() is LnFfiException.WrongChain)
        assertTrue(btcNode.paid.isEmpty())

        val xbtNode = Node()
        val r2 = withSession(LnChain.XBT, xbtNode) { s -> runCatching { s.pay(target(btcInvoice), null) } }
        assertTrue("$r2", r2.exceptionOrNull() is LnFfiException.WrongChain)
        assertTrue(xbtNode.paid.isEmpty())

        // Unknown never counts as permission.
        val junkNode = Node()
        val junk = LnTarget.Bolt11(invoice = "lnbc1", amountMsat = 1_000uL, description = "", expirySecs = 0uL)
        val r3 = withSession(LnChain.BTC, junkNode) { s -> runCatching { s.pay(junk, null) } }
        assertTrue("$r3", r3.exceptionOrNull() is LnFfiException.WrongChain)
        assertTrue(junkNode.paid.isEmpty())

        // The BTC wallet still pays a BTC invoice as before, and the XBT wallet an XBT one.
        val ok = Node()
        assertTrue(withSession(LnChain.BTC, ok) { s -> runCatching { s.pay(target(btcInvoice), null) } }.isSuccess)
        assertEquals(1, ok.paid.size)
        val okX = Node()
        assertTrue(withSession(LnChain.XBT, okX) { s -> runCatching { s.pay(target(xbtInvoice), null) } }.isSuccess)
        assertEquals(1, okX.paid.size)
    }

    // ---- routing a scanned or opened Lightning target -------------------------

    @Test
    fun xbt_onchain_scan_offers_ln_xbt() {
        val all = listOf(btc, xbt, ln, lnXbt)
        assertEquals(
            LightningLinkRoute.OpenLightningWallet(lnXbt.id),
            lightningLinkRoute(all, xbtInvoice, "mainnet", fixtureNow, fromChain = "xbt"),
        )
        // An address (chain unknown until its invoice is fetched) goes to the scanning wallet's chain.
        assertEquals(
            LightningLinkRoute.OpenLightningWallet(lnXbt.id),
            lightningLinkRoute(all, "bob@example.com", "mainnet", fixtureNow, fromChain = "xbt"),
        )
        // A BTC invoice is the BTC Lightning wallet's, wherever it was scanned.
        assertEquals(
            LightningLinkRoute.OpenLightningWallet(ln.id),
            lightningLinkRoute(all, btcInvoice, "mainnet", fixtureNow, fromChain = "xbt"),
        )
        // An XBT invoice opened from a BTC wallet (or a link) still goes to the XBT wallet.
        assertEquals(
            LightningLinkRoute.OpenLightningWallet(lnXbt.id),
            lightningLinkRoute(all, xbtInvoice, "bitcoin", fixtureNow),
        )
        // No XBT Lightning wallet: offer to create one, never the BTC one.
        assertEquals(
            LightningLinkRoute.CreateLightningWallet(LN_XBT_CHAIN),
            lightningLinkRoute(listOf(btc, xbt, ln), xbtInvoice, "mainnet", fixtureNow, fromChain = "xbt"),
        )
        assertEquals(LN_XBT_CHAIN, lightningChainForTarget(LnTargetChain.XBT, fromChain = "btc"))
        assertEquals(LN_CHAIN, lightningChainForTarget(LnTargetChain.BTC, fromChain = "xbt"))
        assertEquals(LN_XBT_CHAIN, lightningChainForTarget(LnTargetChain.UNKNOWN, fromChain = "xbt"))
        assertEquals(LN_CHAIN, lightningChainForTarget(LnTargetChain.UNKNOWN, fromChain = "btc"))
    }

    @Test
    fun xbt_offer_tab_only_while_its_lsp_holds_custody() {
        // BTC: always, as before (also before the account has answered).
        assertTrue(offerShown(LN_CHAIN, null))
        assertTrue(offerShown(LN_CHAIN, AccountBalance(0uL, 0uL, 0uL)))
        // XBT: only once its account reports custody room.
        assertFalse(offerShown(LN_XBT_CHAIN, null))
        assertFalse(offerShown(LN_XBT_CHAIN, AccountBalance(0uL, 0uL, 0uL)))
        assertTrue(offerShown(LN_XBT_CHAIN, AccountBalance(0uL, 0uL, 50_000_000uL)))
    }

    // ---- push routing --------------------------------------------------------

    @Test
    fun push_without_chain_opens_btc() {
        assertEquals(LN_CHAIN, pushTargetChain(mapOf("kind" to "ln_received", "amount_msat" to "1000")))
        assertEquals(LN_CHAIN, pushTargetChain(emptyMap()))
        assertEquals(LN_CHAIN, pushTargetChain(mapOf("kind" to "ln_held", "chain" to "btc")))
    }

    @Test
    fun push_xbt_opens_ln_xbt() {
        assertEquals(LN_XBT_CHAIN, pushTargetChain(mapOf("kind" to "ln_held", "chain" to "xbt")))
        assertEquals(LN_XBT_CHAIN, pushTargetChain(mapOf("kind" to "ln_received", "chain" to "XBT")))
    }

    // ---- two nodes, one process ----------------------------------------------

    @Test
    fun both_lightning_wallets_run_side_by_side() = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        val store = InMemoryLightningStore(listOf(ln, lnXbt).associateBy { it.id })
        val configs = java.util.concurrent.ConcurrentHashMap<LnChain, LnNodeConfig>()
        try {
            val made = mutableListOf<Pair<String, LnChain>>()
            val sessions = LightningSessions { walletChain, chain ->
                made += walletChain to chain
                LightningSession(
                    scope = scope,
                    store = store,
                    nodeFactory = { cfg -> configs[cfg.chain] = cfg.copy(bip39Seed = byteArrayOf()); Node() },
                    info = LnInfoApi { info() },
                    chain = chain,
                )
            }
            assertEquals(listOf(LN_CHAIN to LnChain.BTC, LN_XBT_CHAIN to LnChain.XBT), made)
            assertEquals(LnChain.BTC, sessions.of(LN_CHAIN)!!.chain)
            assertEquals(LnChain.XBT, sessions.of(LN_XBT_CHAIN)!!.chain)
            assertNull(sessions.of("btc"))
            sessions.of(LN_CHAIN)!!.onUnlocked(ln.id, seed).join()
            sessions.of(LN_XBT_CHAIN)!!.onUnlocked(lnXbt.id, seed).join()
            assertTrue(sessions.of(LN_CHAIN)!!.state.value is LnState.Ready)
            assertTrue(sessions.of(LN_XBT_CHAIN)!!.state.value is LnState.Ready)
            val b = configs.getValue(LnChain.BTC)
            val x = configs.getValue(LnChain.XBT)
            assertNotEquals(b.dataDir, x.dataDir)
            assertTrue(b.dataDir.endsWith(ln.id))
            assertTrue(x.dataDir.endsWith(lnXbt.id))
            assertTrue(b.esploraUrl.endsWith("/btc"))
            assertTrue(x.esploraUrl.endsWith("/xbt"))
            // Neither session starts the other chain's wallet.
            sessions.of(LN_CHAIN)!!.onUnlocked(lnXbt.id, seed).join()
            assertEquals(LnState.Off, sessions.of(LN_CHAIN)!!.state.value)
            assertTrue(sessions.of(LN_XBT_CHAIN)!!.state.value is LnState.Ready)
        } finally {
            scope.cancel()
        }
    }

    // ---- moving funds: XBT on-chain wallets <-> ln-xbt only --------------------

    @Test
    fun transfer_xbt_onchain_to_ln_xbt_uses_xbt_transit() = runBlocking {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        val store = InMemoryLightningStore(listOf(ln, lnXbt).associateBy { it.id })
        try {
            val sessions = LightningSessions { _, chain ->
                val transit = if (chain == LnChain.XBT) "xbt-transit" else "btc-transit"
                LightningSession(scope = scope, store = store, nodeFactory = { Node(transit = transit) }, info = LnInfoApi { info() }, chain = chain)
            }
            sessions.of(LN_CHAIN)!!.onUnlocked(ln.id, seed).join()
            sessions.of(LN_XBT_CHAIN)!!.onUnlocked(lnXbt.id, seed).join()
            assertEquals(LN_XBT_CHAIN, lightningChainForOnchain("xbt"))
            assertEquals(LN_CHAIN, lightningChainForOnchain("btc"))
            assertNull(lightningChainForOnchain(LN_CHAIN))
            assertEquals("xbt-transit", sessions.forOnchain("xbt")!!.currentNode!!.transitAddress())
            assertEquals("btc-transit", sessions.forOnchain("btc")!!.currentNode!!.transitAddress())
            assertNull(sessions.forOnchain(LN_XBT_CHAIN))
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun xbt_sweep_targets_xbt_wallet_only() {
        val btc2 = btc.copy(id = "b2")
        val xbtWatch = xbt.copy(id = "xw", watchOnly = true, sealed = null, salt = null)
        // Move out / close / orphan sweeps of the XBT wallet: XBT on-chain wallets only, even
        // when the stored choice is a BTC wallet.
        assertEquals(listOf(xbt), lightningSweepWallets(listOf(btc, xbtWatch, xbt, btc2), LN_XBT_CHAIN))
        assertEquals(listOf(btc, btc2), lightningSweepWallets(listOf(btc, xbt, btc2), LN_CHAIN))
        assertEquals(xbt, moveTargetWallet(listOf(btc, xbt), LN_XBT_CHAIN, chosenId = btc.id))
        assertEquals(btc, moveTargetWallet(listOf(btc, xbt), LN_CHAIN, chosenId = xbt.id))
        assertNull(moveTargetWallet(listOf(btc, btc2, xbtWatch), LN_XBT_CHAIN, chosenId = btc.id))

        // The XBT session's orphan transit sweep goes to the XBT wallet's address.
        val addrOf = { w: WalletConfig -> "addr-${w.chain}-${w.id}" }
        val orphan = balances(lightningSat = 0uL, transitSat = 40_000uL)
        val node = Node(bal = orphan, ch = LnChannelState.NONE)
        withSession(LnChain.XBT, node, sweep = { _ -> sweepAddress(listOf(btc, xbt), LN_XBT_CHAIN, btc.id) { addrOf(it) } }) { }
        assertEquals("addr-xbt-x", node.sweptTo)

        // With no XBT on-chain wallet nothing is swept (never to a BTC address).
        val node2 = Node(bal = orphan, ch = LnChannelState.NONE)
        withSession(LnChain.XBT, node2, sweep = { _ -> sweepAddress(listOf(btc, btc2), LN_XBT_CHAIN, btc.id) { addrOf(it) } }) { }
        assertNull(node2.sweptTo)

        // BTC unchanged: its sweep goes to a BTC wallet.
        val node3 = Node(bal = orphan, ch = LnChannelState.NONE)
        withSession(LnChain.BTC, node3, sweep = { _ -> sweepAddress(listOf(xbt, btc), LN_CHAIN, null) { addrOf(it) } }) { }
        assertEquals("addr-btc-b", node3.sweptTo)
    }

    private companion object {
        fun balances(lightningSat: ULong, transitSat: ULong = 0uL) = LnBalances(
            lightningSat = lightningSat, onchainTransitSat = transitSat, pendingCloseSat = 0uL,
            reservedSat = 0uL, pendingTransitSat = 0uL, inboundSat = 0uL,
        )
    }
}
