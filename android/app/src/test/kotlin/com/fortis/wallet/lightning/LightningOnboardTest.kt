package com.fortis.wallet.lightning

import com.fortis.wallet.data.WalletConfig
import com.fortis.wallet.wallet.LightningWalletKeys
import com.fortis.wallet.wallet.LightningWalletExists
import com.fortis.wallet.wallet.SealedSeed
import com.fortis.wallet.wallet.checkCanAddLightning
import com.fortis.wallet.wallet.newLightningConfig
import com.fortis.wallet.wallet.sealSeed
import com.fortis.wallet.wallet.unsealSeed
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.fail
import org.junit.Test
import uniffi.wallet_ffi.lnBip39Seed

class LightningOnboardTest {
    private val phrase =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"

    @Test
    fun secondLightningWalletIsRefused() {
        val ln = WalletConfig("a", "Spend", "ln", "bitcoin")
        val btc = WalletConfig("b", "Main", "btc", "bitcoin")
        try {
            checkCanAddLightning(listOf(ln))
            fail("expected LightningWalletExists")
        } catch (_: LightningWalletExists) {
        }
        checkCanAddLightning(listOf(btc))
    }

    @Test
    fun newLightningConfigIsLnOnMainnet() {
        val c = newLightningConfig("id", "Spend", SealedSeed("blob", "salt"))
        assertEquals("ln", c.chain)
        assertEquals("bitcoin", c.network)
        assertEquals("blob", c.sealed)
        assertEquals("salt", c.salt)
        assertFalse(c.watchOnly)
    }

    @Test
    fun appUnlockUnsealsALightningOnlyApp() {
        val s = sealSeed(phrase, "", "pw")
        val (m, p) = unsealSeed(s.blobHex, s.saltHex, "pw")
        LightningWalletKeys.fromMnemonic(m, p).use { keys ->
            assertArrayEquals(lnBip39Seed(phrase, ""), keys.copySeed())
        }
    }
}
