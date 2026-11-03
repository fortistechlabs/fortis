package com.fortis.wallet.wallet

import com.fortis.wallet.data.LN_CHAIN
import com.fortis.wallet.data.WalletConfig
import com.fortis.wallet.lightning.canAddLightningWallet

/** The app already has its one Lightning wallet. */
class LightningWalletExists : Exception("This app already has a Lightning wallet.")

fun checkCanAddLightning(wallets: List<WalletConfig>) {
    if (!canAddLightningWallet(wallets)) throw LightningWalletExists()
}

/** The Lightning wallet's stored config: always mainnet Bitcoin, sealed like any signing wallet. */
fun newLightningConfig(id: String, name: String, sealed: SealedSeed): WalletConfig =
    WalletConfig(id, name, LN_CHAIN, "bitcoin", sealed.blobHex, sealed.saltHex)
