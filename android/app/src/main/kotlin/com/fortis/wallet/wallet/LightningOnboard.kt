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

/** The other Lightning wallet's seed could not be compared because the app locked meanwhile:
 *  a restore is refused until the user unlocks and tries again. */
class LightningSeedUnverified : Exception("Couldn't check this phrase against the other Lightning wallet.")

/** The app's Lightning wallet on the other chain, as read for the shared-phrase check. */
sealed interface OtherLightningSeed {
    /** There is no other Lightning wallet. */
    data object None : OtherLightningSeed
    /** Its BIP-39 seed (the caller wipes it). */
    class Seed(val bytes: ByteArray) : OtherLightningSeed
    /** The app locked while it was being read (lock generation changed): try again unlocked. */
    data object LockRace : OtherLightningSeed
    /** It can't be read under the current, valid app secret (unseal or key derivation failed:
     *  a corrupt stored seed). */
    class Unreadable(val cause: Throwable) : OtherLightningSeed
}

/** Whether the shared-phrase check compared the seeds, or let the new wallet through unchecked. */
enum class SeedCheck { Checked, Unchecked }

/** The shared-phrase check of a new Lightning wallet's [seed] against [other]. Throws
 *  [LightningSeedShared] when they are the same. A [OtherLightningSeed.LockRace] refuses a restore
 *  ([restoring]) with [LightningSeedUnverified]. An unreadable other seed, or a lock race while
 *  creating, never blocks: [SeedCheck.Unchecked]. The check is hygiene: each chain derives its
 *  own node key and VSS store, so a corrupt wallet must never stop another (which may hold
 *  funds) being restored. */
fun checkLightningSeedNotShared(seed: ByteArray, other: OtherLightningSeed, restoring: Boolean): SeedCheck =
    when (other) {
        OtherLightningSeed.None -> SeedCheck.Checked
        is OtherLightningSeed.Seed -> {
            if (java.security.MessageDigest.isEqual(seed, other.bytes)) throw LightningSeedShared()
            SeedCheck.Checked
        }
        OtherLightningSeed.LockRace -> if (restoring) throw LightningSeedUnverified() else SeedCheck.Unchecked
        is OtherLightningSeed.Unreadable -> SeedCheck.Unchecked
    }

/** Throws [LightningSeedShared] when [seed] equals [otherSeed], the BIP-39 seed of the app's
 *  Lightning wallet on the other chain (null: there is none). */
fun checkLightningSeedNotShared(seed: ByteArray, otherSeed: ByteArray?) {
    checkLightningSeedNotShared(seed, otherSeed?.let { OtherLightningSeed.Seed(it) } ?: OtherLightningSeed.None, restoring = true)
}

/** Throws unless a Lightning wallet on [chain] ([LN_CHAIN] or "ln-xbt") may be added now. */
fun checkCanAddLightning(wallets: List<WalletConfig>, chain: String, xbtEnabled: Boolean, debug: Boolean) {
    if (canAddLightningWallet(wallets, chain, xbtEnabled, debug)) return
    if (wallets.any { it.chain == chain }) throw LightningWalletExists()
    throw LightningWalletUnavailable()
}

/** Throws unless a Lightning wallet on [chain] may be created ([restoring] false) or restored
 *  now. A restore is one per chain like a creation, but never waits on the edge's switch. */
fun checkCanOnboardLightning(wallets: List<WalletConfig>, chain: String, restoring: Boolean, xbtEnabled: Boolean, debug: Boolean) {
    if (!restoring) return checkCanAddLightning(wallets, chain, xbtEnabled, debug)
    if (com.fortis.wallet.lightning.canRestoreLightningWallet(wallets, chain)) return
    if (wallets.any { it.chain == chain }) throw LightningWalletExists()
    throw LightningWalletUnavailable()
}

/** A Lightning wallet's stored config on [chain] ([LN_CHAIN] or "ln-xbt"): always network
 *  "bitcoin" (the node's), sealed like any signing wallet. */
fun newLightningConfig(id: String, name: String, sealed: SealedSeed, chain: String): WalletConfig {
    require(isLightningChain(chain)) { "not a Lightning chain: $chain" }
    return WalletConfig(id, name, chain, "bitcoin", sealed.blobHex, sealed.saltHex)
}
