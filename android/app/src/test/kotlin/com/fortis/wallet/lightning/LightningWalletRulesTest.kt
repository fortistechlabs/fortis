package com.fortis.wallet.lightning

import com.fortis.wallet.data.WalletConfig
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class LightningWalletRulesTest {
    private fun w(id: String, chain: String = "btc", network: String = "mainnet", sealed: String? = "s",
                  watchOnly: Boolean = false, hardware: Boolean = false) =
        WalletConfig(id, id, chain, network, sealed, "salt", watchOnly = watchOnly, hardware = hardware)

    @Test
    fun onlyOneLightningWalletPerApp() {
        val btc = w("btc")
        val ln = w("ln", chain = "ln", network = "bitcoin")
        assertTrue(canAddLightningWallet(listOf(btc)))
        assertFalse(canAddLightningWallet(listOf(btc, ln)))
        assertEquals(ln, lightningWalletOf(listOf(btc, ln)))
        assertTrue(isLightningWallet(ln))
        assertFalse(isLightningWallet(btc))
        assertFalse(isLightningWallet(null))
    }

    @Test
    fun sweepWalletIsTheFirstSoftwareMainnetBtcWallet() {
        val xbt = w("xbt", chain = "xbt")
        val watch = w("watch", watchOnly = true, sealed = null)
        val hw = w("hw", watchOnly = true, hardware = true, sealed = null)
        val regtest = w("reg", network = "regtest")
        val a = w("a")
        val b = w("b")
        assertEquals(a, lightningSweepWallet(listOf(xbt, watch, hw, regtest, a, b)))
        assertNull(lightningSweepWallet(listOf(xbt, w("ln", chain = "ln", network = "bitcoin"))))
    }
}
