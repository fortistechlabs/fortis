package com.fortis.wallet.ui

import com.fortis.wallet.data.WalletConfig
import com.fortis.wallet.lightning.LnState
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.wallet_ffi.LnBalances
import uniffi.wallet_ffi.LnChannelState
import uniffi.wallet_ffi.LnLspLimits
import uniffi.wallet_ffi.LnTarget
import uniffi.wallet_ffi.LnurlParams
import java.net.URI as Uri

class SendRoutingTest {
    private val bolt11 =
        "lnbc25m1pvjluezpp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdq5vdhkven9v5sxyetpdeessp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygs9q5sqqqqqqqqqqqqqqqpqsq67gye39hfg3zd8rgc80k32tvy9xk2xunwm5lzexnvpx6fd77en8qaq424dxgt56cag2dpt359k3ssyhetktkpqh24jqnjyw6uqd08sgptq44qu"
    private val bolt11NoAmt =
        "lnbc1pvjluezsp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygspp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdpl2pkx2ctnv5sxxmmwwd5kgetjypeh2ursdae8g6twvus8g6rfwvs8qun0dfjkxaq9qrsgq357wnc5r2ueh7ck6q93dj32dlqnls087fxdwk8qakdyafkq3yap2r09nt4ndd0unm3z9u5t48y6ucv4r5sg7lk98c77ctvjczkspk5qprc90gx"
    private val addr = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq"
    private val base58 = "1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2"
    private val nowValid = 1_496_314_668uL
    private val offer =
        "lno1pqps7sjqpgtyzm3qv4uxzmtsd3jjqer9wd3hy6tsw35k7msjzfpy7nz5yqcnygrfdej82um5wf5k2uckyypwa3eyt44h6txtxquqh7lz5djge4afgfjn7k4rgrkuag0jsd5xvxg"
    private val btc = WalletConfig("b", "Main", "btc", "mainnet", sealed = "aa", salt = "bb")
    private val ln = WalletConfig("l", "Spend", "ln", "bitcoin", sealed = "cc", salt = "dd")

    private fun ready(lightningSat: ULong): LnState = LnState.Ready(
        balances = LnBalances(
            lightningSat = lightningSat,
            onchainTransitSat = 0uL,
            pendingCloseSat = 0uL,
            reservedSat = 0uL,
            pendingTransitSat = 0uL,
            inboundSat = 500_000uL,
        ),
        channel = LnChannelState.READY,
    )

    @Test
    fun bolt11_with_balance_plans_lightning() {
        val plan = planSend(
            text = bolt11,
            onchainSat = 0L,
            ln = ready(3_000_000uL),
            network = "bitcoin",
            nowSecs = nowValid,
        )
        assertTrue("expected Lightning plan, got $plan", plan is SendPlan.Lightning)
        val lnPlan = plan as SendPlan.Lightning
        assertEquals(2_500_000_000uL, lnPlan.amountMsat)
        assertTrue(lnPlan.target is LnTarget.Bolt11)
        val b11 = lnPlan.target as LnTarget.Bolt11
        assertEquals(bolt11, b11.invoice)
        assertEquals("coffee beans", b11.description)
    }

    @Test
    fun uppercase_scanned_qr_works() {
        val scanned = "LIGHTNING:${bolt11.uppercase()}"
        val plan = planSend(
            text = scanned,
            onchainSat = 0L,
            ln = ready(3_000_000uL),
            network = "bitcoin",
            nowSecs = nowValid,
        )
        assertTrue("expected Lightning plan, got $plan", plan is SendPlan.Lightning)
        val lnPlan = plan as SendPlan.Lightning
        assertEquals(2_500_000_000uL, lnPlan.amountMsat)
        val b11 = lnPlan.target as LnTarget.Bolt11
        assertEquals(bolt11, b11.invoice)
    }

    @Test
    fun no_amount_invoice_needs_amount() {
        val limits = LnLspLimits(
            minPaymentMsat = 10_000_000uL,
            maxPaymentMsat = 500_000_000uL,
            channelMinSat = 100_000uL,
            channelMaxSat = 500_000uL,
        )
        val plan = planSend(
            text = bolt11NoAmt,
            onchainSat = 0L,
            ln = ready(3_000_000uL),
            network = "bitcoin",
            nowSecs = nowValid,
            limits = limits,
        )
        assertTrue("expected NeedsAmount, got $plan", plan is SendPlan.NeedsAmount)
        val na = plan as SendPlan.NeedsAmount
        assertTrue(na.target is LnTarget.Bolt11)
        // The range hint is what we can send (1 sat to the 3 000 000 sat balance), not the LSP's receive bounds.
        assertEquals(1_000uL, na.minMsat)
        assertEquals(3_000_000_000uL, na.maxMsat)
    }

    @Test
    fun lnurl_range_needs_amount_with_bounds() {
        val plan = planSend(
            text = "alice@example.com",
            onchainSat = 0L,
            ln = ready(500_000uL),
            network = "bitcoin",
            lnurlResolver = {
                LnurlParams(
                    callback = "https://example.com/lnurlp/alice/cb",
                    minMsat = 5_000_000uL,
                    maxMsat = 150_000_000uL,
                    description = "Pay Alice",
                    metadata = "[[\"text/plain\",\"Pay Alice\"]]",
                )
            },
        )
        assertTrue("expected NeedsAmount, got $plan", plan is SendPlan.NeedsAmount)
        val na = plan as SendPlan.NeedsAmount
        assertTrue(na.target is LnTarget.LightningAddress)
        assertEquals(5_000_000uL, na.minMsat)
        assertEquals(150_000_000uL, na.maxMsat)
    }

    @Test
    fun unified_prefers_lightning_when_covered_else_onchain() {
        val unifiedUri = "bitcoin:$addr?amount=0.025&lightning=$bolt11"

        val covered = planSend(
            text = unifiedUri,
            onchainSat = 5_000_000L,
            ln = ready(3_000_000uL),
            network = "bitcoin",
            nowSecs = nowValid,
        )
        assertTrue("expected Lightning when covered, got $covered", covered is SendPlan.Lightning)
        assertEquals(2_500_000_000uL, (covered as SendPlan.Lightning).amountMsat)

        val fallback = planSend(
            text = unifiedUri,
            onchainSat = 5_000_000L,
            ln = ready(1_000_000uL),
            network = "bitcoin",
            nowSecs = nowValid,
        )
        assertTrue("expected OnChain fallback, got $fallback", fallback is SendPlan.OnChain)
        val oc = fallback as SendPlan.OnChain
        assertEquals(addr, oc.address)
        assertEquals(2_500_000L, oc.amountSat)
    }

    @Test
    fun lightning_target_with_only_onchain_funds_needs_move() {
        val plan = planSend(
            text = bolt11,
            onchainSat = 5_000_000L,
            ln = ready(500_000uL),
            network = "bitcoin",
            nowSecs = nowValid,
        )
        assertTrue("expected NeedsMove, got $plan", plan is SendPlan.NeedsMove)
        val nm = plan as SendPlan.NeedsMove
        assertEquals(2_000_000L, nm.shortfallSat)
    }

    @Test
    fun lightning_disabled_rejects_invoice_with_explanation() {
        val plan = planSend(
            text = bolt11,
            onchainSat = 5_000_000L,
            ln = LnState.Off,
            network = "bitcoin",
            nowSecs = nowValid,
        )
        assertTrue("expected Invalid when LN disabled, got $plan", plan is SendPlan.Invalid)
        val inv = plan as SendPlan.Invalid
        assertTrue(
            "expected explanation mentioning Lightning disabled, got: ${inv.reason}",
            inv.reason.contains("Lightning", ignoreCase = true) &&
                inv.reason.contains("disabled", ignoreCase = true),
        )
    }

    @Test
    fun onchain_address_unchanged_behaviour() {
        val plainBech32 = planSend(
            text = addr,
            onchainSat = 100_000L,
            ln = LnState.Off,
            network = "bitcoin",
        )
        assertEquals(SendPlan.OnChain(address = addr, amountSat = null), plainBech32)

        val plainBase58 = planSend(
            text = base58,
            onchainSat = 100_000L,
            ln = ready(50_000uL),
            network = "bitcoin",
        )
        assertEquals(SendPlan.OnChain(address = base58, amountSat = null), plainBase58)

        val bip21 = planSend(
            text = "bitcoin:$addr?amount=0.00050000",
            onchainSat = 100_000L,
            ln = ready(50_000uL),
            network = "bitcoin",
        )
        assertEquals(SendPlan.OnChain(address = addr, amountSat = 50_000L), bip21)
    }

    @Test
    fun wrong_network_and_expired_are_invalid_with_distinct_reasons() {
        val wrongNet = planSend(
            text = bolt11,
            onchainSat = 0L,
            ln = ready(3_000_000uL),
            network = "regtest",
            nowSecs = nowValid,
        )
        val expired = planSend(
            text = bolt11,
            onchainSat = 0L,
            ln = ready(3_000_000uL),
            network = "bitcoin",
            nowSecs = nowValid + 7_200uL,
        )
        assertTrue("expected Invalid for wrong network, got $wrongNet", wrongNet is SendPlan.Invalid)
        assertTrue("expected Invalid for expired, got $expired", expired is SendPlan.Invalid)
        val rWrong = (wrongNet as SendPlan.Invalid).reason
        val rExpired = (expired as SendPlan.Invalid).reason
        assertNotEquals(rWrong, rExpired)
        assertTrue("expected wrong network reason, got $rWrong", rWrong.contains("network", ignoreCase = true))
        assertTrue("expected expired reason, got $rExpired", rExpired.contains("expired", ignoreCase = true))
    }

    @Test
    fun view_intent_with_lightning_uri_prefills_send() {
        val lnUri = Uri.create("lightning:$bolt11")
        val extracted = textFromIntent(lnUri)
        assertNotNull(extracted)
        val plan = planSend(
            text = extracted!!,
            onchainSat = 0L,
            ln = ready(3_000_000uL),
            network = "bitcoin",
            nowSecs = nowValid,
        )
        assertTrue("expected Lightning plan from intent URI, got $plan", plan is SendPlan.Lightning)

        val btcUri = Uri.create("bitcoin:$addr?amount=0.00050000")
        val btcExtracted = textFromIntent(btcUri)
        assertNotNull(btcExtracted)
        val btcPlan = planSend(
            text = btcExtracted!!,
            onchainSat = 100_000L,
            ln = LnState.Off,
            network = "bitcoin",
        )
        assertEquals(SendPlan.OnChain(address = addr, amountSat = 50_000L), btcPlan)

        assertNull(textFromIntent(Uri.create("https://example.com")))
    }

    @Test
    fun lightningTargetsAreRecognised() {
        assertTrue(isLightningTarget(bolt11, "bitcoin", nowValid))
        assertTrue(isLightningTarget(offer, "bitcoin", nowValid))
        assertTrue(isLightningTarget("alice@example.com", "bitcoin", nowValid))
        assertTrue(isLightningTarget("bitcoin:$addr?lightning=$bolt11", "bitcoin", nowValid))
        assertFalse(isLightningTarget(addr, "bitcoin", nowValid))
        assertFalse(isLightningTarget("bitcoin:$addr?amount=0.1", "bitcoin", nowValid))
        // A BTC wallet's network name is "mainnet": the same answers.
        assertTrue(isLightningTarget(bolt11, "mainnet", nowValid))
        assertFalse(isLightningTarget(addr, "mainnet", nowValid))
    }

    @Test
    fun lightningLinkWithoutLightningWalletAsksToCreateOne() {
        assertEquals(
            LightningLinkRoute.CreateLightningWallet,
            lightningLinkRoute(listOf(btc), bolt11, "bitcoin", nowValid),
        )
        assertEquals(
            LightningLinkRoute.OpenLightningWallet(ln.id),
            lightningLinkRoute(listOf(btc, ln), bolt11, "bitcoin", nowValid),
        )
        assertEquals(
            LightningLinkRoute.OnChain,
            lightningLinkRoute(listOf(btc, ln), addr, "bitcoin", nowValid),
        )
        // A BIP21 URI with a Lightning part can still be paid on-chain from the BTC wallet.
        val unified = "bitcoin:$addr?amount=0.025&lightning=$bolt11"
        assertEquals(LightningLinkRoute.OnChain, lightningLinkRoute(listOf(btc), unified, "bitcoin", nowValid))
        assertEquals(LightningLinkRoute.OpenLightningWallet(ln.id), lightningLinkRoute(listOf(btc, ln), unified, "bitcoin", nowValid))
        assertEquals(LightningLinkRoute.CreateLightningWallet, lightningLinkRoute(emptyList(), unified, "bitcoin", nowValid))
    }
}
