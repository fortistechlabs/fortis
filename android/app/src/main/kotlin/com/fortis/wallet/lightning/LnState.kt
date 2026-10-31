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

/** True only for software mainnet BTC wallets (never watch-only, hardware/BitBox, XBT, or non-mainnet). */
fun showLightningToggle(w: WalletConfig?): Boolean =
    w != null &&
        w.chain == "btc" &&
        (w.network == "mainnet" || w.network == "bitcoin") &&
        !w.watchOnly &&
        !w.hardware &&
        w.sealed != null
