package com.fortis.wallet.lightning

import com.fortis.wallet.data.WalletConfig
import com.fortis.wallet.wallet.LightningWalletKeys
import com.fortis.wallet.wallet.LightningWalletExists
import com.fortis.wallet.wallet.LightningSeedShared
import com.fortis.wallet.wallet.checkLightningSeedNotShared
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
            checkCanAddLightning(listOf(ln), "ln", xbtEnabled = false, debug = false)
            fail("expected LightningWalletExists")
        } catch (_: LightningWalletExists) {
        }
        checkCanAddLightning(listOf(btc), "ln", xbtEnabled = false, debug = false)
    }

    @Test
    fun newLightningConfigIsLnOnMainnet() {
        val c = newLightningConfig("id", "Spend", SealedSeed("blob", "salt"), "ln")
        assertEquals("ln", c.chain)
        assertEquals("bitcoin", c.network)
        assertEquals("blob", c.sealed)
        assertEquals("salt", c.salt)
        assertFalse(c.watchOnly)
        // The XBT one is stored the same way, on its own chain.
        val x = newLightningConfig("id2", "Spend", SealedSeed("blob", "salt"), "ln-xbt")
        assertEquals("ln-xbt", x.chain)
        assertEquals("bitcoin", x.network)
    }

    @Test
    fun appUnlockUnsealsALightningOnlyApp() {
        val s = sealSeed(phrase, "", "pw")
        val (m, p) = unsealSeed(s.blobHex, s.saltHex, "pw")
        LightningWalletKeys.fromMnemonic(m, p).use { keys ->
            assertArrayEquals(lnBip39Seed(phrase, ""), keys.copySeed())
        }
    }

    /** The two Lightning wallets never share a BIP-39 seed (same phrase and passphrase). */
    @Test
    fun aSeedSharedWithTheOtherLightningWalletIsRefused() {
        val seed = lnBip39Seed(phrase, "")
        try {
            checkLightningSeedNotShared(seed, lnBip39Seed(phrase, ""))
            fail("expected LightningSeedShared")
        } catch (_: LightningSeedShared) {
        }
        // No other Lightning wallet, or another phrase / passphrase: allowed.
        checkLightningSeedNotShared(seed, null)
        checkLightningSeedNotShared(seed, lnBip39Seed(phrase, "other"))
        checkLightningSeedNotShared(
            seed,
            lnBip39Seed("legal winner thank year wave sausage worth useful legal winner thank yellow", ""),
        )
    }
}
