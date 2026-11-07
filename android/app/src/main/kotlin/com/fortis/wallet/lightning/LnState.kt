package com.fortis.wallet.lightning

import com.fortis.wallet.data.LN_CHAIN
import com.fortis.wallet.data.LN_XBT_CHAIN
import com.fortis.wallet.data.WalletConfig
import uniffi.wallet_ffi.LnBalances
import uniffi.wallet_ffi.LnChannelState
import uniffi.wallet_ffi.LnFfiException

typealias LnFfiError = LnFfiException

val ZERO_LN_BALANCES = LnBalances(
    lightningSat = 0uL,
    onchainTransitSat = 0uL,
    pendingCloseSat = 0uL,
    reservedSat = 0uL,
    pendingTransitSat = 0uL,
    inboundSat = 0uL,
)

/** Total satoshis currently in transit or closing on-chain ("Moving…"). */
val LnBalances.movingSat: Long
    get() = (onchainTransitSat + pendingCloseSat + pendingTransitSat).toLong()

/** Total user-owned satoshis on the Lightning side (spendable + transit + closing). */
val LnBalances.totalLightningSideSat: Long
    get() = (lightningSat + onchainTransitSat + pendingCloseSat + pendingTransitSat).toLong()

sealed interface LnState {
    data object Off : LnState
    data object Starting : LnState
    data class Ready(
        val balances: LnBalances,
        val channel: LnChannelState,
    ) : LnState

    data class Error(
        val error: LnFfiError,
        val lastBalances: LnBalances = ZERO_LN_BALANCES,
    ) : LnState
}

/** A Lightning wallet of either chain ("ln" or "ln-xbt"). */
fun isLightningWallet(w: WalletConfig?): Boolean = w != null && w.isLightning

/**
 * Whether "Add wallet" offers the Lightning wallet on [chain] ([LN_CHAIN] or [LN_XBT_CHAIN]):
 * one of each per app. Lightning (XBT) only while `/ln/info`'s `xbt` object is enabled
 * ([xbtEnabled]) or in a debug build ([debug]).
 */
fun canAddLightningWallet(wallets: List<WalletConfig>, chain: String, xbtEnabled: Boolean, debug: Boolean): Boolean = when (chain) {
    LN_CHAIN -> wallets.none { it.chain == LN_CHAIN }
    LN_XBT_CHAIN -> (xbtEnabled || debug) && wallets.none { it.chain == LN_XBT_CHAIN }
    else -> false
}

/** The app's Lightning wallet on [chain] ([LN_CHAIN] or [LN_XBT_CHAIN]), if it has one. */
fun lightningWalletOf(wallets: List<WalletConfig>, chain: String): WalletConfig? = wallets.firstOrNull { it.chain == chain }

/** The wallet [lnWalletChain]'s exit sweeps to: the first eligible on-chain wallet of its chain. */
fun lightningSweepWallet(wallets: List<WalletConfig>, lnWalletChain: String): WalletConfig? =
    lightningSweepWallets(wallets, lnWalletChain).firstOrNull()

/** The wallets the Lightning wallet on [lnWalletChain] can move funds to: software mainnet
 *  wallets of its own chain only (BTC for "ln", XBT for "ln-xbt"; they can derive a fresh
 *  address without a device). Never another chain's: its addresses look the same. */
fun lightningSweepWallets(wallets: List<WalletConfig>, lnWalletChain: String): List<WalletConfig> {
    val onchain = onchainChainForLightning(lnWalletChain) ?: return emptyList()
    return wallets.filter {
        it.chain == onchain &&
            (it.network == "mainnet" || it.network == "bitcoin") &&
            !it.watchOnly &&
            !it.hardware &&
            it.sealed != null
    }
}

fun deleteWalletLightningWarningSat(lightningSat: Long): Long? =
    lightningSat.takeIf { it > 0L }

fun deleteWalletLightningWarningSat(
    ln: LnState,
    lastKnownLightningSat: Long = 0L,
): Long? = com.fortis.wallet.homeBalance(0L, ln, lastKnownLightningSat).lightningSat.takeIf { it > 0L }

/** Deleting the Lightning wallet while its node is not Ready: the balance could not be checked
 *  (and no last balance is known), so the dialog warns that funds may be there. */
fun deleteWalletLightningBalanceUnknown(ln: LnState): Boolean =
    ln !is LnState.Ready && deleteWalletLightningWarningSat(ln) == null
