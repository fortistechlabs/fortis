package com.fortis.wallet.swap

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import com.fortis.wallet.FortisApplication
import com.fortis.wallet.R
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

/**
 * Drives the active swaps every [TICK_MS] while any are running, under a
 * `dataSync` foreground service with the "Swap in progress" notification. It
 * stops itself once no swap is active. Where Android won't run it (started
 * from the background, or the daily `dataSync` budget spent) the work falls
 * back, silently, to [SwapWorker].
 */
class SwapService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private val starts = StartGate()

    private val app get() = application as FortisApplication

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        starts.onStart(startId)
        try {
            ServiceCompat.startForeground(
                this, SwapNotifications.PROGRESS_ID,
                SwapNotifications.progress(this, getString(R.string.swap_in_progress)),
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC else 0,
            )
        } catch (e: Exception) {
            // ForegroundServiceStartNotAllowedException (API 31+, from the
            // background or with the dataSync time used up) or a missing
            // permission: let the worker carry on instead.
            Log.w(TAG, "can't run in the foreground; falling back to the worker", e)
            SwapWorker.runOnce(this)
            stopSelf()
            return START_NOT_STICKY
        }
        val loop = starts.claimLoop()
        if (loop == null) {
            // Already looping: a start is a nudge (an unlock, a new swap) — step now.
            app.swapRunner.launchActive(scope)
        } else {
            scope.launch {
                try {
                    run(loop)
                } finally {
                    starts.loopEnded(loop)
                }
            }
        }
        return START_STICKY
    }

    private suspend fun run(loop: Int) {
        while (scope.isActive) {
            val seen = starts.latest
            val active = try {
                app.swapStore.active()
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                Log.w(TAG, "reading the swaps failed", e)
                delay(TICK_MS)
                continue
            }
            if (active.isEmpty() && app.swapRunner.idle) {
                // A start that came in after `seen` (a new swap) may not be in
                // `active` yet: go round again instead of stopping over it.
                if (starts.tryStop(loop, seen)) {
                    // The loop is given up. Stop first; drop the notification only
                    // if Android took the stop. A refusal means a newer start is
                    // on its way to onStartCommand, which launches a fresh loop.
                    if (stopSelfResult(seen)) {
                        ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
                    }
                    return
                }
                continue
            }
            active.firstOrNull()?.let { SwapNotifications.updateProgress(this, SwapNotifications.progressText(it)) }
            app.swapRunner.launchActive(scope)
            delay(TICK_MS)
        }
    }

    /** Android 15+: the daily `dataSync` budget ran out. Stop; the worker carries on. */
    override fun onTimeout(startId: Int, fgsType: Int) {
        Log.w(TAG, "dataSync time limit reached; falling back to the worker")
        SwapWorker.runOnce(this)
        ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }

    companion object {
        private const val TAG = "SwapService"

        /** The phone engine's pace in the foreground. */
        const val TICK_MS = 20_000L

        /** Start (or nudge) the service. If Android won't allow it, the worker runs instead. */
        fun start(ctx: Context) {
            try {
                ContextCompat.startForegroundService(ctx, Intent(ctx, SwapService::class.java))
            } catch (e: IllegalStateException) {
                // ForegroundServiceStartNotAllowedException, or a plain
                // background-start refusal: no foreground service now.
                Log.w(TAG, "can't start the swap service; falling back to the worker", e)
                SwapWorker.runOnce(ctx)
            }
        }

        /** Stop the service if no swap is active (it also stops itself on its next tick). */
        fun stopIfIdle(ctx: Context) {
            val app = ctx.applicationContext as FortisApplication
            app.appScope.launch {
                val idle = runCatching { app.swapStore.active().isEmpty() }.getOrDefault(false)
                if (idle && app.swapRunner.idle) ctx.stopService(Intent(ctx, SwapService::class.java))
            }
        }
    }
}
