package com.fortis.wallet.wallet

import com.fortis.wallet.data.LN_CHAIN
import com.fortis.wallet.data.WalletConfig
import com.fortis.wallet.data.isLightningChain
import com.fortis.wallet.lightning.canAddLightningWallet

/** The app already has its one Lightning wallet on that chain. */
class LightningWalletExists : Exception("This app already has a Lightning wallet.")

/** Lightning (XBT) is not offered yet (the edge has it off, and this is not a debug build). */
class LightningWalletUnavailable : Exception("Lightning (XBT) is not available yet.")

/** Throws unless a Lightning wallet on [chain] ([LN_CHAIN] or "ln-xbt") may be added now. */
fun checkCanAddLightning(wallets: List<WalletConfig>, chain: String, xbtEnabled: Boolean, debug: Boolean) {
    if (canAddLightningWallet(wallets, chain, xbtEnabled, debug)) return
    if (wallets.any { it.chain == chain }) throw LightningWalletExists()
    throw LightningWalletUnavailable()
}

/** A Lightning wallet's stored config on [chain] ([LN_CHAIN] or "ln-xbt"): always network
 *  "bitcoin" (the node's), sealed like any signing wallet. */
fun newLightningConfig(id: String, name: String, sealed: SealedSeed, chain: String): WalletConfig {
    require(isLightningChain(chain)) { "not a Lightning chain: $chain" }
    return WalletConfig(id, name, chain, "bitcoin", sealed.blobHex, sealed.saltHex)
}
