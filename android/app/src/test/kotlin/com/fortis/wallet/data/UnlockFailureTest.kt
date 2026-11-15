package com.fortis.wallet.data

import java.security.InvalidKeyException
import java.security.UnrecoverableKeyException
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

// Same simple name as android.security.keystore.KeyPermanentlyInvalidatedException:
// the android.jar stub can't be constructed in a JVM unit test.
private class KeyPermanentlyInvalidatedException(msg: String) : InvalidKeyException(msg)

class UnlockFailureTest {
    @Test
    fun missingKeyIsLostWhateverTheException() {
        // The reported case: getEntry() returned null and a NullPointerException surfaced.
        val d = diagnoseUnlockFailure(NullPointerException("null cannot be cast"), keyPresent = false)
        assertTrue(d.keyLost)
    }

    @Test
    fun invalidatedKeyIsLostEvenWhenWrappedDeep() {
        val inner = KeyPermanentlyInvalidatedException("invalidated")
        val d = diagnoseUnlockFailure(RuntimeException("outer", IllegalStateException("mid", inner)), keyPresent = true)
        assertTrue(d.keyLost)
    }

    @Test
    fun unrecoverableKeyIsLost() {
        val d = diagnoseUnlockFailure(UnrecoverableKeyException("gone"), keyPresent = true)
        assertTrue(d.keyLost)
    }

    @Test
    fun anOrdinaryFailureWithTheKeyPresentIsNotLost() {
        // A transient failure must not tell the user their key is gone for good.
        val d = diagnoseUnlockFailure(IllegalStateException("prompt busy"), keyPresent = true)
        assertFalse(d.keyLost)
    }

    @Test
    fun detailNamesKeyStateAndCauseChainWithoutMessages() {
        val d = diagnoseUnlockFailure(
            RuntimeException("secret-looking message", NullPointerException("x")),
            keyPresent = false,
        )
        assertEquals("key missing; RuntimeException > NullPointerException", d.detail)
        assertFalse(d.detail.contains("secret-looking"))
    }

    @Test
    fun detailIsBoundedOnACyclicChain() {
        val a = RuntimeException("a")
        val b = RuntimeException("b", a)
        a.initCause(b)
        val d = diagnoseUnlockFailure(a, keyPresent = true)
        assertTrue(d.detail.startsWith("key present; "))
        assertTrue(d.detail.length < 300)
    }
}
