package com.fortis.wallet.wallet

/** The Lightning node's BIP-39 seed, derived from the Lightning wallet's own phrase. */
class LightningWalletKeys private constructor(private var seed: ByteArray?) : AutoCloseable {

    @Synchronized
    fun copySeed(): ByteArray =
        checkNotNull(seed) { "Lightning keys are closed" }.copyOf()

    @Synchronized
    override fun close() {
        seed?.fill(0)
        seed = null
    }

    companion object {
        /** Throws if the phrase is not a valid BIP-39 mnemonic. */
        fun fromMnemonic(mnemonic: String, passphrase: String): LightningWalletKeys =
            LightningWalletKeys(uniffi.wallet_ffi.lnBip39Seed(mnemonic, passphrase))
    }
}
