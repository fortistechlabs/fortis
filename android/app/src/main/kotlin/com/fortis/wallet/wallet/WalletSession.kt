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

/** Held in memory only while unlocked. Wraps the wallet-ffi objects.
 *
 *  `wallet` is null for a watch-only session — [WalletView] (what `view`
 *  wraps) stores only a public `Xpub` and derives addresses via
 *  secp256k1's verification-only context, so a watch-only session is
 *  provably incapable of signing by construction, not just convention.
 *  Private constructor + named factories rather than constructor
 *  overloading, since `xpub`/`view` for the signing path depend on
 *  `wallet` existing first. */
class WalletSession private constructor(
    val chain: String,
    val wallet: Wallet?,
    val xpub: String,
    val fingerprint: String?,
    val view: WalletView,
) {
    companion object {
        fun signing(chain: String, network: String, mnemonic: String, passphrase: String): WalletSession {
            val wallet = Wallet.fromMnemonic(mnemonic, passphrase, network)
            val xpub = wallet.accountXpub(chain, 0u)
            return WalletSession(chain, wallet, xpub, wallet.masterFingerprint(), WalletView(chain, network, xpub))
        }

        fun watchOnly(chain: String, network: String, xpub: String) =
            WalletSession(chain, null, xpub, null, WalletView(chain, network, xpub))
    }

    fun setIndices(nextReceive: Int, nextChange: Int) =
        view.setNextIndices(nextReceive.toUInt(), nextChange.toUInt())

    fun receiveAddress(index: Int) = view.addressAt(0u, index.toUInt())

    /** Validate a send recipient against this wallet's network. Throws a clear
     *  "… is not a valid address" if it doesn't parse — cheap, no I/O. */
    fun checkAddress(address: String): String = view.checkAddress(address)

    /** Defense in depth — the real gate is that a watch-only wallet never
     *  shows a Send tab, so this should never actually be reached. */
    fun sign(planTxHex: String, selected: List<SelectedInput>): String {
        val w = wallet ?: error("watch-only — cannot sign")
        return w.signFundingTx(chain, 0u, planTxHex, selected.map {
            SpentInput(it.valueSat, it.scriptPubkeyHex, it.derivationIndex, it.isChange)
        })
    }

    fun close() {
        wallet?.close()
        view.close()
    }
}
