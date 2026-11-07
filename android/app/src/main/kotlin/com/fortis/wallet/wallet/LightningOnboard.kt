package com.fortis.wallet.wallet

import com.fortis.wallet.data.LN_CHAIN
import com.fortis.wallet.data.WalletConfig
import com.fortis.wallet.data.isLightningChain
import com.fortis.wallet.lightning.canAddLightningWallet

/** The app already has its one Lightning wallet on that chain. */
class LightningWalletExists : Exception("This app already has a Lightning wallet.")

/** Lightning (XBT) is not offered yet (the edge has it off, and this is not a debug build). */
class LightningWalletUnavailable : Exception("Lightning (XBT) is not available yet.")

/** The new Lightning wallet's BIP-39 seed (phrase + passphrase) is the other Lightning
 *  wallet's. Their nodes would still be apart (XBT derives its own node key and VSS store),
 *  but one phrase for both is refused so the two wallets stay independent backups. */
class LightningSeedShared : Exception("This recovery phrase is already your other Lightning wallet's.")

/** Throws [LightningSeedShared] when [seed] equals [otherSeed], the BIP-39 seed of the app's
 *  Lightning wallet on the other chain (null: there is none). */
fun checkLightningSeedNotShared(seed: ByteArray, otherSeed: ByteArray?) {
    if (otherSeed != null && java.security.MessageDigest.isEqual(seed, otherSeed)) {
        throw LightningSeedShared()
    }
}

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
