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
    fun amount_with_ln_builds_bip21_with_lightning_param() {
        val quote = LnReceiveQuote(
            invoice = sampleInvoice,
            amountMsat = 50_000_000uL,
            feeMsat = 0uL,
            needsChannel = false,
        )
        val qr = buildReceiveQr(
            address = addr,
            amountSat = 50_000L,
            quote = quote,
            minLnSat = minLnSat,
        )
        assertTrue(qr.lightning)
        assertEquals("bitcoin:$addr?amount=0.00050000&lightning=$sampleInvoice", qr.uri)
        assertEquals(0L, qr.feeSat)
        assertEquals(50_000L, qr.youGetSat)
        assertNull(qr.hint)
    }

    @Test
    fun amount_below_ln_min_is_onchain_only_with_hint() {
        val qr = buildReceiveQr(
            address = addr,
            amountSat = 5_000L,
            quote = null,
            minLnSat = minLnSat,
        )
        assertFalse(qr.lightning)
        assertEquals("bitcoin:$addr?amount=0.00005000", qr.uri)
        assertNull(qr.feeSat)
        assertNull(qr.youGetSat)
        assertEquals(ReceiveHint.BelowLightningMin, qr.hint)
    }

    @Test
    fun no_amount_is_onchain_with_add_amount_hint() {
        val qr = buildReceiveQr(
            address = addr,
            amountSat = null,
            quote = null,
            minLnSat = minLnSat,
        )
        assertFalse(qr.lightning)
        assertEquals(addr, qr.uri)
        assertNull(qr.feeSat)
        assertNull(qr.youGetSat)
        assertEquals(ReceiveHint.AddAmountForLightning, qr.hint)
    }

    @Test
    fun first_receive_shows_fee_and_you_get() {
        val quote = LnReceiveQuote(
            invoice = sampleInvoice,
            amountMsat = 50_000_000uL,
            feeMsat = 500_000uL,
            needsChannel = true,
        )
        val qr = buildReceiveQr(
            address = addr,
            amountSat = 50_000L,
            quote = quote,
            minLnSat = minLnSat,
        )
        assertTrue(qr.lightning)
        assertEquals(500L, qr.feeSat)
        assertEquals(49_500L, qr.youGetSat)
        assertNull(qr.hint)
    }

    @Test
    fun lightning_disabled_is_plain_address() {
        val qr = buildReceiveQr(
            address = addr,
            amountSat = null,
            quote = null,
            minLnSat = 0L,
            lightningEnabled = false,
        )
        assertFalse(qr.lightning)
        assertEquals(addr, qr.uri)
        assertNull(qr.feeSat)
        assertNull(qr.youGetSat)
        assertNull(qr.hint)
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
        val qr = buildReceiveQr(
            address = addr,
            amountSat = 50_000L,
            quote = quote,
            minLnSat = minLnSat,
        )
        // Invoice in URI is untouched (carries exact msat), while display fee/amount round up.
        assertEquals("bitcoin:$addr?amount=0.00050000&lightning=$sampleInvoice", qr.uri)
        assertEquals(501L, qr.feeSat)
        assertEquals(50_001L - 501L, qr.youGetSat)
    }
}
