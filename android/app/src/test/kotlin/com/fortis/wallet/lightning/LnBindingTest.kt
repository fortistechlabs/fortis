package com.fortis.wallet.lightning

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import uniffi.wallet_ffi.LnFfiException
import uniffi.wallet_ffi.LnTarget
import uniffi.wallet_ffi.LnUnifiedLightning
import uniffi.wallet_ffi.lnBip39Seed
import uniffi.wallet_ffi.lnEstimateFeeMsat
import uniffi.wallet_ffi.lnParse
import uniffi.wallet_ffi.lnParseAt

class LnBindingTest {
    private val bolt11 =
        "lnbc25m1pvjluezpp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdq5vdhkven9v5sxyetpdeessp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygs9q5sqqqqqqqqqqqqqqqpqsq67gye39hfg3zd8rgc80k32tvy9xk2xunwm5lzexnvpx6fd77en8qaq424dxgt56cag2dpt359k3ssyhetktkpqh24jqnjyw6uqd08sgptq44qu"
    private val addr = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq"
    private val nowValid = 1_496_314_668uL

    @Test
    fun parseBolt11AndOnchain() {
        val parsedBolt11 = lnParseAt(bolt11, "bitcoin", nowValid)
        assertTrue(parsedBolt11 is LnTarget.Bolt11)
        val b11 = parsedBolt11 as LnTarget.Bolt11
        assertEquals(bolt11, b11.invoice)
        assertEquals(2_500_000_000uL, b11.amountMsat)
        assertEquals("coffee beans", b11.description)
        assertEquals(3600uL, b11.expirySecs)

        val parsedAddr = lnParse(addr, "bitcoin")
        assertTrue(parsedAddr is LnTarget.OnChain)
        val onChain = parsedAddr as LnTarget.OnChain
        assertEquals(addr, onChain.address)
        assertEquals(null, onChain.amountSat)

        val unified = lnParseAt("bitcoin:$addr?amount=0.025&lightning=$bolt11", "bitcoin", nowValid)
        assertTrue(unified is LnTarget.Unified)
        val u = unified as LnTarget.Unified
        assertEquals(addr, u.address)
        assertEquals(2_500_000uL, u.amountSat)
        assertTrue(u.lightning is LnUnifiedLightning.Bolt11)

        // Expired invoice via lnParse (uses wall clock) surfaces InvoiceExpired
        try {
            lnParse(bolt11, "bitcoin")
            fail("expected InvoiceExpired")
        } catch (e: LnFfiException.InvoiceExpired) {
            // expected
        }

        // Wrong network surfaces WrongNetwork
        try {
            lnParseAt(bolt11, "regtest", nowValid)
            fail("expected WrongNetwork")
        } catch (e: LnFfiException.WrongNetwork) {
            // expected
        }

        val fee = lnEstimateFeeMsat(parsedBolt11, 0uL)
        assertEquals(25_050_000uL, fee)

        val seed = lnBip39Seed(
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
            "",
        )
        assertEquals(64, seed.size)
    }
}
