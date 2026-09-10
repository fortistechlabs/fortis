package com.fortis.wallet.data

import android.content.Context
import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyInfo
import android.security.keystore.KeyProperties
import android.util.Base64
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricManager.Authenticators.BIOMETRIC_STRONG
import androidx.biometric.BiometricManager.Authenticators.DEVICE_CREDENTIAL
import java.security.KeyStore
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.SecretKeyFactory
import javax.crypto.spec.GCMParameterSpec

/**
 * The app's single unlock key.
 *
 * Every wallet seed is sealed by wallet-ffi (Argon2id + XChaCha20) under one
 * random 32-byte **app secret**. In biometric mode that secret is wrapped by an
 * AES-GCM key in the Android Keystore (`fortis.app`) that can only be used after
 * a successful device unlock — fingerprint/face *or* the device PIN/pattern
 * (`BIOMETRIC_STRONG | DEVICE_CREDENTIAL`), so enrolling a new fingerprint does
 * not lock the user out. Unlock once, all wallets open.
 */
object SeedKeystore {
    private const val KS = "AndroidKeyStore"
    private const val ALIAS = "fortis.app"
    private const val IV_LEN = 12

    /** Can the device do a strong biometric *or* device-credential unlock? */
    fun available(ctx: Context): Boolean =
        BiometricManager.from(ctx).canAuthenticate(BIOMETRIC_STRONG or DEVICE_CREDENTIAL) ==
            BiometricManager.BIOMETRIC_SUCCESS

    private fun keyStore() = KeyStore.getInstance(KS).apply { load(null) }

    private fun secretKey(): SecretKey =
        (keyStore().getEntry(ALIAS, null) as KeyStore.SecretKeyEntry).secretKey

    fun hasKey(): Boolean = runCatching { keyStore().containsAlias(ALIAS) }.getOrDefault(false)

    /** Where the app key actually lives — for the Settings security line.
     *  `null` when there is no key (password mode) or it can't be inspected. */
    enum class KeySecurity { STRONGBOX, HARDWARE, SOFTWARE }

    fun keySecurity(): KeySecurity? = runCatching {
        val key = secretKey()
        val info = SecretKeyFactory.getInstance(key.algorithm, KS)
            .getKeySpec(key, KeyInfo::class.java) as KeyInfo
        when {
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.S ->
                when (info.securityLevel) {
                    KeyProperties.SECURITY_LEVEL_STRONGBOX -> KeySecurity.STRONGBOX
                    KeyProperties.SECURITY_LEVEL_SOFTWARE,
                    KeyProperties.SECURITY_LEVEL_UNKNOWN_SECURE -> KeySecurity.SOFTWARE
                    else -> KeySecurity.HARDWARE
                }
            @Suppress("DEPRECATION") info.isInsideSecureHardware -> KeySecurity.HARDWARE
            else -> KeySecurity.SOFTWARE
        }
    }.getOrNull()

    private fun keySpec(strongBox: Boolean): KeyGenParameterSpec {
        val b = KeyGenParameterSpec.Builder(
            ALIAS,
            KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        )
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .setUserAuthenticationRequired(true)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            b.setUserAuthenticationParameters(
                0,
                KeyProperties.AUTH_BIOMETRIC_STRONG or KeyProperties.AUTH_DEVICE_CREDENTIAL,
            )
        } else {
            @Suppress("DEPRECATION")
            b.setUserAuthenticationValidityDurationSeconds(-1)
        }
        if (strongBox && Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) b.setIsStrongBoxBacked(true)
        return b.build()
    }

    /** Create (or replace) the app key. Prefer a discrete StrongBox secure
     *  element (API 28+, hardware permitting); fall back to the TEE where there
     *  is no StrongBox — its absence surfaces only when the key is generated. */
    fun createKey() {
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KS)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            try {
                gen.init(keySpec(strongBox = true))
                gen.generateKey()
                return
            } catch (e: Exception) {
                // StrongBoxUnavailableException (or a strongbox-provisioning failure
                // wrapped in one) — retry in the TEE. Matched by name to keep the
                // class off the API < 28 verification path.
                val noStrongBox = generateSequence<Throwable>(e) { it.cause }
                    .any { it.javaClass.simpleName == "StrongBoxUnavailableException" }
                if (!noStrongBox) throw e
                runCatching { deleteKey() } // clear any half-made entry
            }
        }
        gen.init(keySpec(strongBox = false))
        gen.generateKey()
    }

    fun deleteKey() {
        runCatching { keyStore().deleteEntry(ALIAS) }
    }

    fun newSecret(): ByteArray = ByteArray(32).also { SecureRandom().nextBytes(it) }

    /** A cipher to encrypt with the app key — must go through the unlock prompt. */
    fun encryptCipher(): Cipher =
        Cipher.getInstance("AES/GCM/NoPadding").apply { init(Cipher.ENCRYPT_MODE, secretKey()) }

    /** A cipher to decrypt `wrapped` — must go through the unlock prompt.
     *  Throws `KeyPermanentlyInvalidatedException` if device security was removed. */
    fun decryptCipher(wrapped: String): Cipher {
        val blob = Base64.decode(wrapped, Base64.NO_WRAP)
        return Cipher.getInstance("AES/GCM/NoPadding").apply {
            init(Cipher.DECRYPT_MODE, secretKey(), GCMParameterSpec(128, blob.copyOfRange(0, IV_LEN)))
        }
    }

    fun wrap(cipher: Cipher, secret: ByteArray): String {
        val ct = cipher.doFinal(secret)
        return Base64.encodeToString(cipher.iv + ct, Base64.NO_WRAP)
    }

    fun unwrap(cipher: Cipher, wrapped: String): ByteArray {
        val blob = Base64.decode(wrapped, Base64.NO_WRAP)
        return cipher.doFinal(blob.copyOfRange(IV_LEN, blob.size))
    }
}

fun ByteArray.toHex(): String = joinToString("") { "%02x".format(it) }
