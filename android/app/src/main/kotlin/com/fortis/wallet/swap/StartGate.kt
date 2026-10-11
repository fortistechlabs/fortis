package com.fortis.wallet.swap

/**
 * [SwapService]'s loop lifecycle decisions, made atomically under one lock so
 * the service is never left alive with no loop:
 * - `onStartCommand` records its id ([onStart]), then [claimLoop]s: a token
 *   means launch a loop, null means one is running (just nudge it).
 * - run() notes [latest] before reading the active swaps; on an empty read it
 *   stops only if [tryStop] holds. That gives the loop up in the same step, so
 *   a start landing after it launches a fresh loop instead of nudging a loop
 *   that is about to return.
 */
class StartGate {
    private var last = 0
    private var current = 0
    private var looping = false

    val latest: Int @Synchronized get() = last

    val isLooping: Boolean @Synchronized get() = looping

    @Synchronized
    fun onStart(startId: Int) {
        last = startId
    }

    /** A token for a new loop to launch, or null while one is running. */
    @Synchronized
    fun claimLoop(): Int? {
        if (looping) return null
        looping = true
        return ++current
    }

    /**
     * True: no start since [seen] was noted, and [loop] is given up — the caller
     * must return and stop with `stopSelfResult(seen)`.
     */
    @Synchronized
    fun tryStop(loop: Int, seen: Int): Boolean {
        if (loop != current || !looping || last != seen) return false
        looping = false
        return true
    }

    /** [loop]'s job ended (any way); frees the slot unless a newer loop holds it. */
    @Synchronized
    fun loopEnded(loop: Int) {
        if (loop == current) looping = false
    }
}
