package com.fortis.wallet.wallet

import uniffi.wallet_ffi.SelectedInput
import uniffi.wallet_ffi.SpentInput
import uniffi.wallet_ffi.Wallet
import uniffi.wallet_ffi.WalletView
import uniffi.wallet_ffi.generateMnemonic
import uniffi.wallet_ffi.sealMnemonicWithPassword
import uniffi.wallet_ffi.unsealMnemonicWithPassword
import java.security.SecureRandom

fun randomBytes(n: Int): ByteArray = ByteArray(n).also { SecureRandom().nextBytes(it) }
private fun ByteArray.toHex() = joinToString("") { "%02x".format(it) }
private fun String.hexToBytes() = chunked(2).map { it.toInt(16).toByte() }.toByteArray()

/** `words` is 12 or 24. `extra` (optional, from [EntropyCollector.bytes]) is
 *  folded into the CSPRNG bytes inside wallet-ffi — it can only strengthen the seed. */
fun newMnemonic(extra: ByteArray? = null, words: Int = 24): String =
    generateMnemonic(randomBytes(32), extra?.takeIf { it.isNotEmpty() }, words.toUByte())

data class SealedSeed(val blobHex: String, val saltHex: String)

/** Seal `{mnemonic, passphrase}` under a password. Line 1 = words, rest = passphrase. */
fun sealSeed(mnemonic: String, passphrase: String, password: String): SealedSeed {
    val salt = randomBytes(16)
    val nonce = randomBytes(24)
    val payload = mnemonic.trim() + "\n" + passphrase
    return SealedSeed(sealMnemonicWithPassword(payload, password, salt, nonce), salt.toHex())
}

/** Thrown by [unsealSeed] when decryption fails — almost always a wrong
 *  password/secret (the AEAD tag didn't verify). No message: callers decide the
 *  wording (a wrong app password vs. an unexpected failure with the app secret),
 *  instead of leaking the raw `v1=crypto: unseal failed …` string. */
class WrongPassword : Exception()

fun unsealSeed(blobHex: String, saltHex: String, password: String): Pair<String, String> {
    val payload = try {
        unsealMnemonicWithPassword(blobHex, password, saltHex.hexToBytes())
    } catch (e: Exception) {
        throw WrongPassword()
    }
    val nl = payload.indexOf('\n')
    return if (nl < 0) payload to "" else payload.substring(0, nl) to payload.substring(nl + 1)
}

/** Held in memory only while unlocked. Wraps the wallet-ffi objects. */
class WalletSession(
    val chain: String,
    network: String,
    mnemonic: String,
    passphrase: String,
) {
    val wallet: Wallet = Wallet.fromMnemonic(mnemonic, passphrase, network)
    val xpub: String = wallet.accountXpub(chain, 0u)
    val fingerprint: String = wallet.masterFingerprint()
    val view: WalletView = WalletView(chain, network, xpub)

    fun setIndices(nextReceive: Int, nextChange: Int) =
        view.setNextIndices(nextReceive.toUInt(), nextChange.toUInt())

    fun receiveAddress(index: Int) = view.addressAt(0u, index.toUInt())

    /** Validate a send recipient against this wallet's network. Throws a clear
     *  "… is not a valid address" if it doesn't parse — cheap, no I/O. */
    fun checkAddress(address: String): String = view.checkAddress(address)

    fun sign(planTxHex: String, selected: List<SelectedInput>): String =
        wallet.signFundingTx(chain, 0u, planTxHex, selected.map {
            SpentInput(it.valueSat, it.scriptPubkeyHex, it.derivationIndex, it.isChange)
        })

    fun close() {
        wallet.close()
        view.close()
    }
}
