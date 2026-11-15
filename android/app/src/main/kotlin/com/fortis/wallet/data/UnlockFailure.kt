package com.fortis.wallet.data

import java.security.UnrecoverableKeyException

/**
 * Why an unlock attempt failed, as far as we can tell.
 *
 * [keyLost]: the Keystore key that wraps the app secret is gone or permanently
 * unusable (screen lock removed, key entry deleted, …). Nothing on the device can
 * bring it back, so the unlock screen should say so instead of showing a raw error.
 * It is only ever true on positive evidence — a transient failure with the key still
 * present must not tell the user their key is gone for good.
 *
 * [detail]: a short, message-free line (key state + exception class chain) that is
 * safe to show and to paste into a bug report.
 */
data class UnlockDiagnosis(val keyLost: Boolean, val detail: String)

private const val MAX_CHAIN = 8

fun diagnoseUnlockFailure(t: Throwable, keyPresent: Boolean): UnlockDiagnosis {
    val chain = generateSequence(t) { it.cause }.take(MAX_CHAIN).toList()
    // Matched by name as well as by type: KeyPermanentlyInvalidatedException is an
    // Android class that a JVM unit test cannot construct.
    val invalidated = chain.any {
        it is UnrecoverableKeyException || it.javaClass.simpleName == "KeyPermanentlyInvalidatedException"
    }
    val detail = "key ${if (keyPresent) "present" else "missing"}; " +
        chain.joinToString(" > ") { it.javaClass.simpleName }
    return UnlockDiagnosis(keyLost = !keyPresent || invalidated, detail = detail)
}
