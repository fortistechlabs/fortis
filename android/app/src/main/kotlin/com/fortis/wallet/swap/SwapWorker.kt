package com.fortis.wallet.swap

import android.content.Context
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import com.fortis.wallet.FortisApplication
import java.util.concurrent.TimeUnit

/**
 * Steps every active swap once, then returns: every 15 minutes (unique periodic
 * work `swap-sync`), on boot, and whenever the foreground service can't run.
 * Each swap is stepped concurrently by the shared [SwapRunner]; the run ends
 * once all of them are done.
 */
class SwapWorker(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {
    override suspend fun doWork(): Result {
        val app = applicationContext as FortisApplication
        app.swapRunner.launchActive(app.appScope).join()
        return Result.success()
    }

    companion object {
        const val PERIODIC = "swap-sync"
        const val ONCE = "swap-sync-now"

        private val online = Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build()

        /** Keep the 15-minute pass enqueued (a no-op if it already is). */
        fun schedule(ctx: Context) {
            WorkManager.getInstance(ctx).enqueueUniquePeriodicWork(
                PERIODIC,
                ExistingPeriodicWorkPolicy.KEEP,
                PeriodicWorkRequestBuilder<SwapWorker>(15, TimeUnit.MINUTES).setConstraints(online).build(),
            )
        }

        /** One pass as soon as there is a network (one queued at most). */
        fun runOnce(ctx: Context) {
            WorkManager.getInstance(ctx).enqueueUniqueWork(
                ONCE,
                ExistingWorkPolicy.KEEP,
                OneTimeWorkRequestBuilder<SwapWorker>().setConstraints(online).build(),
            )
        }
    }
}
