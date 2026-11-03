package com.fortis.wallet.lightning

import com.fortis.wallet.wallet.LightningWalletKeys
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.fail
import org.junit.Test
import uniffi.wallet_ffi.lnBip39Seed

class LightningWalletKeysTest {
    private val phrase =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"

    @Test
    fun seedIsTheLnBip39SeedOfThePhrase() {
        LightningWalletKeys.fromMnemonic(phrase, "").use { keys ->
            val seed = keys.copySeed()
            assertEquals(64, seed.size)
            assertArrayEquals(lnBip39Seed(phrase, ""), seed)
        }
    }

    @Test
    fun invalidPhraseIsRefused() {
        try {
            LightningWalletKeys.fromMnemonic("abandon abandon", "")
            fail("expected an exception")
        } catch (_: Exception) {
        }
    }

    @Test
    fun closeZeroesTheSeed() {
        val keys = LightningWalletKeys.fromMnemonic(phrase, "")
        keys.close()
        try {
            keys.copySeed()
            fail("expected IllegalStateException")
        } catch (_: IllegalStateException) {
        }
    }
}
