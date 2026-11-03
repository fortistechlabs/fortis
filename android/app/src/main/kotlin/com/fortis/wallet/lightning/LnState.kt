package com.fortis.wallet.lightning

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

fun isLightningWallet(w: WalletConfig?): Boolean = w != null && w.isLightning

/** One Lightning wallet per app. */
fun canAddLightningWallet(wallets: List<WalletConfig>): Boolean = wallets.none { it.isLightning }

fun lightningWalletOf(wallets: List<WalletConfig>): WalletConfig? = wallets.firstOrNull { it.isLightning }

/** The wallet a Lightning exit sweeps to: the first software mainnet BTC wallet. */
fun lightningSweepWallet(wallets: List<WalletConfig>): WalletConfig? = wallets.firstOrNull {
    it.chain == "btc" &&
        (it.network == "mainnet" || it.network == "bitcoin") &&
        !it.watchOnly &&
        !it.hardware &&
        it.sealed != null
}

fun deleteWalletLightningWarningSat(lightningSat: Long): Long? =
    lightningSat.takeIf { it > 0L }

fun deleteWalletLightningWarningSat(
    ln: LnState,
    lastKnownLightningSat: Long = 0L,
): Long? = com.fortis.wallet.homeBalance(0L, ln, lastKnownLightningSat).lightningSat.takeIf { it > 0L }
