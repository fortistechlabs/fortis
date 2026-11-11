package com.fortis.wallet.lightning

import com.fortis.wallet.data.WalletConfig
import com.fortis.wallet.wallet.LightningWalletKeys
import com.fortis.wallet.wallet.LightningWalletExists
import com.fortis.wallet.wallet.LightningSeedShared
import com.fortis.wallet.wallet.checkLightningSeedNotShared
import com.fortis.wallet.wallet.SealedSeed
import com.fortis.wallet.wallet.checkCanAddLightning
import com.fortis.wallet.wallet.checkCanOnboardLightning
import com.fortis.wallet.wallet.LightningSeedUnverified
import com.fortis.wallet.wallet.OtherLightningSeed
import com.fortis.wallet.wallet.SeedCheck
import com.fortis.wallet.wallet.LightningWalletUnavailable
import com.fortis.wallet.data.LN_CHAIN
import com.fortis.wallet.data.LN_XBT_CHAIN
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

    /** Restoring a Lightning (XBT) wallet never waits on the edge's `xbt.enabled`: a user who
     *  lost their phone must get to their funds (the node starts in exit mode). One per app
     *  still holds; only creating a new one is gated. */
    @Test
    fun restoringXbtLightningIsNotGatedOnTheEdgeSwitch() {
        val btc = WalletConfig("b", "Main", "btc", "mainnet")
        val ln = WalletConfig("l", "Spend", LN_CHAIN, "bitcoin")
        val lnXbt = WalletConfig("lx", "Spend", LN_XBT_CHAIN, "bitcoin")
        checkCanOnboardLightning(listOf(btc), LN_XBT_CHAIN, restoring = true, xbtEnabled = false, debug = false)
        checkCanOnboardLightning(listOf(btc, ln), LN_XBT_CHAIN, restoring = true, xbtEnabled = false, debug = false)
        try {
            checkCanOnboardLightning(listOf(btc), LN_XBT_CHAIN, restoring = false, xbtEnabled = false, debug = false)
            fail("a new Lightning (XBT) wallet was created while it is off")
        } catch (_: LightningWalletUnavailable) {
        }
        checkCanOnboardLightning(listOf(btc), LN_XBT_CHAIN, restoring = false, xbtEnabled = true, debug = false)
        try {
            checkCanOnboardLightning(listOf(lnXbt), LN_XBT_CHAIN, restoring = true, xbtEnabled = false, debug = false)
            fail("a second Lightning (XBT) wallet was restored")
        } catch (_: LightningWalletExists) {
        }
        try {
            checkCanOnboardLightning(listOf(ln), LN_CHAIN, restoring = true, xbtEnabled = true, debug = true)
            fail("a second Lightning wallet was restored")
        } catch (_: LightningWalletExists) {
        }
    }

    /** "Add wallet" → Restore offers Lightning (XBT) whatever the edge says (like the BTC one);
     *  Create offers it only while it is on (or in a debug build). */
    @Test
    fun restoreOffersLightningXbtWhileCreateWaitsForTheSwitch() {
        val btc = WalletConfig("b", "Main", "btc", "mainnet")
        val ln = WalletConfig("l", "Spend", LN_CHAIN, "bitcoin")
        val lnXbt = WalletConfig("lx", "Spend", LN_XBT_CHAIN, "bitcoin")
        assertEquals(listOf(LN_CHAIN, LN_XBT_CHAIN), lightningOnboardChoices(listOf(btc), restoring = true, xbtEnabled = false, debug = false))
        assertEquals(listOf(LN_CHAIN), lightningOnboardChoices(listOf(btc), restoring = false, xbtEnabled = false, debug = false))
        assertEquals(listOf(LN_CHAIN, LN_XBT_CHAIN), lightningOnboardChoices(listOf(btc), restoring = false, xbtEnabled = true, debug = false))
        assertEquals(listOf(LN_XBT_CHAIN), lightningOnboardChoices(listOf(ln), restoring = true, xbtEnabled = false, debug = false))
        assertEquals(emptyList<String>(), lightningOnboardChoices(listOf(ln, lnXbt), restoring = true, xbtEnabled = true, debug = true))
    }

    /** The other Lightning wallet's seed, read as a typed result: compared when read; a lock
     *  race (the app locked meanwhile) refuses a restore with "unlock and try again"; a seed that
     *  can't be read under the valid app secret (corrupt) never blocks — the new wallet goes
     *  ahead unchecked (with a warning). Creating never blocks on a read failure either. */
    @Test
    fun theOtherLightningSeedIsComparedOrSkippedButOnlyALockRaceBlocksARestore() {
        val seed = lnBip39Seed(phrase, "")
        val same = OtherLightningSeed.Seed(lnBip39Seed(phrase, ""))
        val other = OtherLightningSeed.Seed(lnBip39Seed(phrase, "other"))
        val corrupt = OtherLightningSeed.Unreadable(IllegalStateException("bad blob"))
        for (restoring in listOf(true, false)) {
            assertEquals(SeedCheck.Checked, checkLightningSeedNotShared(seed, OtherLightningSeed.None, restoring))
            assertEquals(SeedCheck.Checked, checkLightningSeedNotShared(seed, other, restoring))
            try {
                checkLightningSeedNotShared(seed, same, restoring)
                fail("expected LightningSeedShared")
            } catch (_: LightningSeedShared) {
            }
            // Corrupt: fail open, whatever the flow.
            assertEquals(SeedCheck.Unchecked, checkLightningSeedNotShared(seed, corrupt, restoring))
        }
        // Lock race: a restore is refused with a retry; a create goes ahead unchecked.
        try {
            checkLightningSeedNotShared(seed, OtherLightningSeed.LockRace, restoring = true)
            fail("expected LightningSeedUnverified")
        } catch (_: LightningSeedUnverified) {
        }
        assertEquals(SeedCheck.Unchecked, checkLightningSeedNotShared(seed, OtherLightningSeed.LockRace, restoring = false))
    }
}
