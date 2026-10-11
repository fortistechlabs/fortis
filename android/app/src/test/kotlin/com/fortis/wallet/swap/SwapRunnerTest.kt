package com.fortis.wallet.swap

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.plus
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.Collections

class SwapRunnerTest {
    @Test
    fun a_blocked_swap_does_not_hold_up_the_others() = runBlocking {
        val release = CompletableDeferred<Unit>()
        val bDone = CompletableDeferred<Unit>()
        val runner = SwapRunner(active = { listOf("a", "b") }) { id ->
            if (id == "a") release.await() else bDone.complete(Unit)
        }
        val pass = runner.launchActive(this + Dispatchers.Default)
        // "a" is stuck on its long-poll; "b" still steps.
        withTimeout(5_000) { bDone.await() }
        assertFalse(runner.idle)
        release.complete(Unit)
        pass.join()
        assertTrue(runner.idle)
    }

    @Test
    fun a_swap_still_stepping_is_not_stepped_again() = runBlocking {
        val release = CompletableDeferred<Unit>()
        val started = CompletableDeferred<Unit>()
        val steps = Collections.synchronizedList(mutableListOf<String>())
        val runner = SwapRunner(active = { listOf("a") }) { id ->
            steps += id
            started.complete(Unit)
            release.await()
        }
        val first = runner.launchActive(this + Dispatchers.Default)
        withTimeout(5_000) { started.await() }
        // The next 20 s tick while "a" is still in its step: skipped, not stacked
        // (a stacked step would wait on `release` too, so the pass wouldn't end).
        withTimeout(5_000) { runner.launchActive(this@runBlocking + Dispatchers.Default).join() }
        assertEquals(listOf("a"), steps.toList())
        release.complete(Unit)
        first.join()
        // Once it's done, the next pass steps it again.
        runner.launchActive(this + Dispatchers.Default).join()
        assertEquals(listOf("a", "a"), steps.toList())
    }

    @Test
    fun a_failing_step_does_not_stop_the_pass() = runBlocking {
        val steps = Collections.synchronizedList(mutableListOf<String>())
        val runner = SwapRunner(active = { listOf("a", "b") }) { id ->
            steps += id
            if (id == "a") error("boom")
        }
        runner.launchActive(this + Dispatchers.Default).join()
        assertEquals(setOf("a", "b"), steps.toSet())
        assertTrue(runner.idle)
    }
}
