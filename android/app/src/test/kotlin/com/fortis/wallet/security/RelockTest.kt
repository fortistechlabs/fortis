package com.fortis.wallet.security

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class RelockTest {
    private var now = 1_000_000L

    @Test
    fun the_app_locks_after_a_minute_in_the_background_not_before() {
        val a = AutoLock(timeoutMs = 60_000) { now }
        assertFalse("never left the screen", a.shouldLockOnReturn())
        a.wentToBackground(); now += 59_999
        assertFalse(a.shouldLockOnReturn())
        a.wentToBackground(); now += 60_000
        assertTrue(a.shouldLockOnReturn())
        // Answered once: coming back again without leaving doesn't re-lock.
        assertFalse(a.shouldLockOnReturn())
    }

    @Test
    fun a_confirmation_is_good_once_and_only_while_fresh() {
        val r = Reauth(freshMs = 60_000) { now }
        assertFalse("nothing confirmed yet", r.consume())
        assertFalse("wrong secret", r.confirm(given = "nope", expected = "s3cret"))
        assertFalse(r.consume())
        assertTrue(r.confirm(given = "s3cret", expected = "s3cret"))
        assertTrue(r.consume())
        assertFalse("used once", r.consume())
        assertTrue(r.confirm("s3cret", "s3cret")); now += 60_001
        assertFalse("stale", r.consume())
        assertFalse("no expected secret (locked)", r.confirm("x", null))
    }
}
