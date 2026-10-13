package com.fortis.wallet

import com.fortis.wallet.data.WalletConfig
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class WalletNamesTest {
    private val wallets = listOf(
        WalletConfig("1", "Savings", "btc", "mainnet"),
        WalletConfig("2", "Savings", "xbt", "mainnet"),
    )

    @Test
    fun sameNameOnTheSameChainIsTaken() {
        assertTrue(walletNameTaken(wallets, "Savings", "btc"))
        assertTrue(walletNameTaken(wallets, "  savings ", "btc"))
    }

    @Test
    fun sameNameOnTheOtherChainIsAllowed() {
        // A wallet added to the other chain keeps its name by design.
        assertFalse(walletNameTaken(listOf(wallets[0]), "Savings", "xbt"))
    }

    @Test
    fun renamingAWalletToItsOwnNameIsAllowed() {
        assertFalse(walletNameTaken(wallets, "SAVINGS", "btc", exceptId = "1"))
    }

    @Test
    fun blankNamesBecomeWallet() {
        assertEquals("Wallet", cleanWalletName("   "))
        assertEquals("Savings", cleanWalletName("  Savings  "))
    }
}
