package com.fortis.wallet.data

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class WalletConfigTest {
    private val xpub = "xpub6BkU445MSEBXbPjD3g2c2ch6mn8yy1SXXQUM7EwjgYiq6Wt1NDwDZ45npqWcV8uQC5oi2gHuVukoCoZZyT4HKq8EpotPMqGqxdZRuapCQ23"

    private fun hw(fp: String?) = WalletConfig(
        id = "w1", name = "Ledger", chain = "btc", network = "mainnet",
        watchOnly = true, xpub = xpub, hardware = true, masterFingerprint = fp,
    )

    @Test
    fun hardwareWalletSurvivesAJsonRoundTrip() {
        val back = WalletConfig.fromJson(JSONObject(hw("37b5eed4").toJson().toString()))
        assertTrue(back.hardware)
        assertEquals("37b5eed4", back.masterFingerprint)
        assertEquals(hw("37b5eed4"), back)
    }

    @Test
    fun hardwareWalletWithoutAFingerprintRoundTrips() {
        val back = WalletConfig.fromJson(JSONObject(hw(null).toJson().toString()))
        assertTrue(back.hardware)
        assertNull(back.masterFingerprint)
    }

    @Test
    fun walletsSavedBeforeThisFeatureLoadAsNotHardware() {
        val legacy = JSONObject("""{"id":"w1","name":"Old","chain":"btc","network":"mainnet","watch_only":true,"xpub":"$xpub"}""")
        val c = WalletConfig.fromJson(legacy)
        assertFalse(c.hardware)
        assertNull(c.masterFingerprint)
    }

    @Test
    fun pickerLabelSaysHardwareRatherThanWatchOnly() {
        assertEquals("BTC · Ledger · hardware", hw("37b5eed4").display)
        assertEquals("BTC · Ledger · watch-only", hw(null).copy(hardware = false).display)
    }

    @Test
    fun cloningToTheOtherChainKeepsTheHardwareMarker() {
        // cloneToOtherChain uses copy(); the marker must ride along so the XBT
        // side is labelled too.
        val xbt = hw("37b5eed4").copy(id = "w2", chain = "xbt")
        assertTrue(xbt.hardware)
        assertEquals("XBT · Ledger · hardware", xbt.display)
    }
}
