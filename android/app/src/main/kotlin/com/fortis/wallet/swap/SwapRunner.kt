package com.fortis.wallet.swap

import android.util.Log
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeout
import java.util.concurrent.ConcurrentHashMap

/**
 * Steps every active swap, each in its own coroutine. A step can block for up to
 * 25 s on the edge's relay long-poll, so one swap must not hold up another; and
 * a swap still in its step when the next pass comes round (the service's 20 s
 * tick, the worker, an unlock) is skipped rather than queued behind itself.
 *
 * The one instance lives in `FortisApplication`, shared by the service, the
 * worker and the unlock hook, so "in flight" covers all of them in a process.
 */
class SwapRunner(
    private val active: suspend () -> List<String>,
    /** The most one step may take. Past it, the step is cancelled (its HTTP
     *  calls with it) and the swap steps again next pass — from its saved
     *  record, so nothing is lost; a stuck step can't hold a swap forever. */
    private val stepTimeoutMs: Long = STEP_TIMEOUT_MS,
    /** Told when a step is abandoned at its time limit (into the swap's trace). */
    private val onTimeout: (String) -> Unit = {},
    private val step: suspend (String) -> Unit,
) {
    private val inFlight = ConcurrentHashMap.newKeySet<String>()

    /** No swap is being stepped right now. */
    val idle: Boolean get() = inFlight.isEmpty()

    /**
     * Start a step for each active swap not already in one. The returned job
     * completes once every step it started is done (the worker waits on it; the
     * service doesn't).
     */
    fun launchActive(scope: CoroutineScope): Job = scope.launch {
        val ids = try {
            active()
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            log("reading the swaps failed", e)
            return@launch
        }
        for (id in ids) {
            if (!inFlight.add(id)) {
                if (com.fortis.wallet.BuildConfig.DEBUG) runCatching { Log.i("SwapRunner", "${id.take(8)}: still in its last step, skipped") }
                continue
            }
            launch {
                try {
                    withTimeout(stepTimeoutMs) { step(id) }
                } catch (e: TimeoutCancellationException) {
                    // Only this step's own limit — not the pass being cancelled.
                    log("step $id took over ${stepTimeoutMs / 1000} s; abandoned, retried next pass", e)
                    runCatching { onTimeout(id) }
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    log("step $id failed", e)
                } finally {
                    inFlight.remove(id)
                }
            }
        }
    }

    // android.util.Log is a stub under JVM unit tests (it throws); logging must never fail a pass.
    private fun log(msg: String, e: Exception) {
        // Debug builds only: step errors can carry outpoints and txids.
        if (com.fortis.wallet.BuildConfig.DEBUG) runCatching { Log.w("SwapRunner", msg, e) }
    }

    companion object {
        /** Generous next to a normal step (one 25 s relay long-poll plus a few
         *  calls, each capped at 60 s), so only a stuck step ever reaches it. */
        const val STEP_TIMEOUT_MS = 5 * 60_000L
    }
}
