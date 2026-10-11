package com.fortis.wallet.data

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Wraps a swap's two signing secrets at rest. Modelled on [SeedKeystore], but the
 * AES-GCM key (`fortis_swap_v1`) deliberately does **not** require user
 * authentication: a swap in flight must be able to refund or redeem from the
 * background (service, worker, after a reboot) while the app is locked. Each
 * secret only ever controls one swap's HTLCs, and is blanked once it settles.
 */
object SwapKeystore {
    private const val KS = "AndroidKeyStore"
    private const val ALIAS = "fortis_swap_v1"
    private const val IV_LEN = 12

    private fun keyStore() = KeyStore.getInstance(KS).apply { load(null) }

    @Synchronized
    private fun key(): SecretKey {
        (keyStore().getEntry(ALIAS, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }
        val spec = KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .build()
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KS).run {
            init(spec)
            generateKey()
        }
    }

    /** `iv || ciphertext`, base64. */
    fun wrap(secret: ByteArray): String {
        val c = Cipher.getInstance("AES/GCM/NoPadding").apply { init(Cipher.ENCRYPT_MODE, key()) }
        return Base64.encodeToString(c.iv + c.doFinal(secret), Base64.NO_WRAP)
    }

    fun unwrap(wrapped: String): ByteArray {
        val blob = Base64.decode(wrapped, Base64.NO_WRAP)
        val c = Cipher.getInstance("AES/GCM/NoPadding").apply {
            init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, blob.copyOfRange(0, IV_LEN)))
        }
        return c.doFinal(blob.copyOfRange(IV_LEN, blob.size))
    }
}
