package com.fortis.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.wallet_ffi.LnReceiveQuote

class ReceiveQrTest {
    private val addr = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq"
    private val sampleInvoice = "lnbc500u1p0testinvoice"
    private val minLnSat = 10_000L

    @Test
    fun lightningWalletQrIsTheInvoiceOnly() {
        val quote = LnReceiveQuote(
            invoice = sampleInvoice,
            amountMsat = 50_000_000uL,
            feeMsat = 1_000_000uL,
            needsChannel = true,
        )
        val qr = buildLightningReceiveQr(50_000L, quote, minLnSat)
        assertTrue(qr.lightning)
        assertEquals("lightning:$sampleInvoice", qr.uri)
        assertEquals(1_000L, qr.feeSat)
        assertEquals(49_000L, qr.youGetSat)
        assertNull(qr.hint)

        val noAmount = buildLightningReceiveQr(null, null, minLnSat)
        assertFalse(noAmount.lightning)
        assertEquals("", noAmount.uri)
        assertEquals(ReceiveHint.AddAmountForLightning, noAmount.hint)
    }

    @Test
    fun lightning_wallet_below_min_has_no_qr_and_a_hint() {
        val qr = buildLightningReceiveQr(5_000L, null, minLnSat)
        assertFalse(qr.lightning)
        assertEquals("", qr.uri)
        assertNull(qr.feeSat)
        assertNull(qr.youGetSat)
        assertEquals(ReceiveHint.BelowLightningMin, qr.hint)
    }

    @Test
    fun lightning_wallet_without_a_quote_yet_has_no_qr() {
        val qr = buildLightningReceiveQr(50_000L, null, minLnSat)
        assertFalse(qr.lightning)
        assertEquals("", qr.uri)
        assertNull(qr.hint)
    }

    @Test
    fun btcWalletQrIsOnChainOnly() {
        val quote = LnReceiveQuote(
            invoice = sampleInvoice,
            amountMsat = 50_000_000uL,
            feeMsat = 0uL,
            needsChannel = false,
        )
        val qr = buildReceiveQr(addr, 50_000L, quote, 10_000L)
        assertFalse(qr.lightning)
        // fmtCoin keeps 8 decimals, as the BTC receive QR always has (BIP21 allows either).
        assertEquals("bitcoin:$addr?amount=0.00050000", qr.uri)
        assertNull(qr.feeSat)
        assertNull(qr.youGetSat)
        assertNull(qr.hint)

        val plain = buildReceiveQr(addr, null, null, 0L)
        assertEquals(addr, plain.uri)
        assertNull(plain.hint)
    }

    @Test
    fun sub_sat_msat_amount_rounds_up_for_display_only() {
        assertEquals(0L, displaySatFromMsat(0uL))
        assertEquals(1L, displaySatFromMsat(1uL))
        assertEquals(1L, displaySatFromMsat(999uL))
        assertEquals(1L, displaySatFromMsat(1_000uL))
        assertEquals(2L, displaySatFromMsat(1_001uL))

        val quote = LnReceiveQuote(
            invoice = sampleInvoice,
            amountMsat = 50_000_100uL, // 50,000.1 sat
            feeMsat = 500_250uL, // 500.25 sat -> rounds up to 501 sat for display
            needsChannel = true,
        )
        val qr = buildLightningReceiveQr(50_000L, quote, minLnSat)
        // Invoice in URI is untouched (carries exact msat), while display fee/amount round up.
        assertEquals("lightning:$sampleInvoice", qr.uri)
        assertEquals(501L, qr.feeSat)
        assertEquals(50_001L - 501L, qr.youGetSat)
    }
}
