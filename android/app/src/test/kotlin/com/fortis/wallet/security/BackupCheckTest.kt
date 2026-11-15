package com.fortis.wallet.security

import com.fortis.wallet.data.WalletConfig
import kotlin.random.Random
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class BackupCheckTest {
    private val words = (1..12).map { "w$it" }

    @Test
    fun challengePicksThreeDistinctSortedIndicesInRange() {
        repeat(200) { seed ->
            val idx = backupChallenge(12, Random(seed))
            assertEquals(3, idx.size)
            assertEquals(idx.sorted(), idx)
            assertEquals(3, idx.toSet().size)
            assertTrue(idx.all { it in 0 until 12 })
        }
    }

    @Test
    fun challengeNeverAsksForMoreWordsThanThereAre() {
        assertEquals(listOf(0, 1), backupChallenge(2, Random(1)))
        assertEquals(emptyList<Int>(), backupChallenge(0, Random(1)))
    }

    @Test
    fun answersMatchIgnoringCaseAndSurroundingSpace() {
        assertTrue(backupAnswersMatch(words, listOf(0, 4, 11), listOf(" W1", "w5 ", "w12")))
    }

    @Test
    fun wrongOrMissingAnswersFail() {
        assertFalse(backupAnswersMatch(words, listOf(0, 4, 11), listOf("w1", "w5", "w11")))
        assertFalse(backupAnswersMatch(words, listOf(0, 4, 11), listOf("w1", "w5")))
        assertFalse(backupAnswersMatch(words, listOf(0, 4, 11), listOf("", "", "")))
        assertFalse(backupAnswersMatch(words, emptyList(), emptyList()))
    }

    private fun cfg(
        sealed: Boolean = true, watch: Boolean = false, hw: Boolean = false,
        chain: String = "btc", confirmed: Boolean = false,
    ) = WalletConfig(
        id = "i", name = "n", chain = chain, network = "mainnet",
        sealed = if (sealed) "s" else null, salt = if (sealed) "t" else null,
        watchOnly = watch, hardware = hw, backupConfirmed = confirmed,
    )

    @Test
    fun onChainWalletWithFundsAndNoBackupNeedsOne() {
        assertTrue(needsBackupWarning(cfg(), balanceSat = 1))
    }

    @Test
    fun noWarningWithoutFundsOrOnceConfirmed() {
        assertFalse(needsBackupWarning(cfg(), balanceSat = 0))
        assertFalse(needsBackupWarning(cfg(), balanceSat = null))
        assertFalse(needsBackupWarning(cfg(confirmed = true), balanceSat = 5_000))
    }

    @Test
    fun walletsWithNoSeedNeverWarn() {
        assertFalse(needsBackupWarning(cfg(sealed = false, watch = true), balanceSat = 5_000))
        assertFalse(needsBackupWarning(cfg(sealed = false, hw = true), balanceSat = 5_000))
    }

    @Test
    fun lightningWalletWarnsWhileUnconfirmedEvenWithUnknownBalance() {
        assertTrue(needsBackupWarning(cfg(chain = "ln"), balanceSat = null))
        assertFalse(needsBackupWarning(cfg(chain = "ln", confirmed = true), balanceSat = null))
    }

    @Test
    fun configRoundTripKeepsTheFlagAndOldJsonDefaultsToUnconfirmed() {
        val on = WalletConfig.fromJson(cfg(confirmed = true).toJson())
        assertTrue(on.backupConfirmed)
        val legacy = cfg().toJson().apply { remove("backup_confirmed") }
        assertFalse(WalletConfig.fromJson(legacy).backupConfirmed)
    }
}
