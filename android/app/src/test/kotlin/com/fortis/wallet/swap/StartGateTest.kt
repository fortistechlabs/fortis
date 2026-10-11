package com.fortis.wallet.swap

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The interleavings of [SwapService.onStartCommand] (onStart + claimLoop) with
 * run()'s idle check (latest, then tryStop). A loop must be running whenever the
 * service stays alive.
 */
class StartGateTest {
    @Test
    fun no_start_means_stop_with_the_latest_id() {
        val g = StartGate()
        g.onStart(1)
        val loop = g.claimLoop()!!
        val seen = g.latest
        assertTrue(g.tryStop(loop, seen))
        assertEquals(1, seen)
        g.loopEnded(loop)
        assertFalse(g.isLooping)
    }

    @Test
    fun a_start_before_the_idle_check_keeps_the_loop_and_is_a_nudge() {
        val g = StartGate()
        g.onStart(1)
        val loop = g.claimLoop()!!
        val seen = g.latest      // run() notes the id, then reads active(): empty
        g.onStart(2)             // a new swap is confirmed in between
        assertNull("a running loop: the start only nudges", g.claimLoop())
        assertFalse(g.tryStop(loop, seen))
        assertTrue(g.isLooping)
        assertTrue(g.tryStop(loop, g.latest)) // the next idle pass may stop, with id 2
    }

    @Test
    fun a_start_after_the_idle_check_but_before_the_stop_launches_a_new_loop() {
        val g = StartGate()
        g.onStart(1)
        val old = g.claimLoop()!!
        val seen = g.latest
        assertTrue(g.tryStop(old, seen)) // run() decides to stop…
        g.onStart(2)                     // …and a start lands before stopSelf(1) / the job ends
        val fresh = g.claimLoop()
        assertNotNull("the old loop is ending: the start must launch a new one", fresh)
        g.loopEnded(old)                 // the old job completes afterwards
        assertTrue("the new loop is still the running one", g.isLooping)
        assertNull("no second loop on a further start", g.claimLoop())
        assertFalse("the old loop can't stop over the new one", g.tryStop(old, g.latest))
    }

    @Test
    fun a_loop_that_ends_abnormally_lets_the_next_start_launch_one() {
        val g = StartGate()
        g.onStart(1)
        val loop = g.claimLoop()!!
        g.loopEnded(loop)
        g.onStart(2)
        assertNotNull(g.claimLoop())
    }
}
