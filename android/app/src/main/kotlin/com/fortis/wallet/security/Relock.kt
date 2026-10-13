package com.fortis.wallet.security

import java.security.MessageDigest

/**
 * Locks the app after it has been out of sight for [timeoutMs]: the activity
 * reports leaving ([wentToBackground]) and returning ([shouldLockOnReturn]).
 * A quick trip elsewhere (copying an address, a share sheet) doesn't lock.
 */
class AutoLock(private val timeoutMs: Long, private val now: () -> Long = System::currentTimeMillis) {
    private var leftAt: Long? = null

    fun wentToBackground() {
        leftAt = now()
    }

    /** True once if the app was away at least [timeoutMs]; the answer is used up. */
    fun shouldLockOnReturn(): Boolean {
        val left = leftAt ?: return false
        leftAt = null
        return now() - left >= timeoutMs
    }
}

/**
 * A fresh "it's you" before revealing a recovery phrase or sending: [confirm]
 * with what the user just proved (the app secret their fingerprint unwrapped,
 * or their password), compared in constant time; [consume] spends it — once,
 * and only within [freshMs].
 */
class Reauth(private val freshMs: Long, private val now: () -> Long = System::currentTimeMillis) {
    private var confirmedAt: Long? = null

    fun confirm(given: String, expected: String?): Boolean {
        val ok = expected != null &&
            MessageDigest.isEqual(given.toByteArray(Charsets.UTF_8), expected.toByteArray(Charsets.UTF_8))
        confirmedAt = if (ok) now() else null
        return ok
    }

    fun consume(): Boolean {
        val at = confirmedAt ?: return false
        confirmedAt = null
        return now() - at <= freshMs
    }
}

/** How long the app may be out of sight before it locks. */
const val AUTO_LOCK_MS = 60_000L

/** How long a confirmation stays good for the reveal or send it was asked for. */
const val REAUTH_FRESH_MS = 60_000L
