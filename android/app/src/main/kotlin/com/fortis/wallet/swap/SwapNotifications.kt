package com.fortis.wallet.swap

import android.Manifest
import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.app.ActivityCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import com.fortis.wallet.R
import com.fortis.wallet.data.Step
import com.fortis.wallet.data.SwapRecord

/**
 * The swap notifications: one persistent "in progress" line on [CHANNEL_PROGRESS]
 * (the foreground service's), and an alert per [Event] on [CHANNEL_EVENTS].
 * Tapping either opens the app (and so its unlock), which steps the swaps at once.
 */
object SwapNotifications {
    const val CHANNEL_PROGRESS = "swap_progress"
    const val CHANNEL_EVENTS = "swap_events"

    /** The foreground service's notification id. */
    const val PROGRESS_ID = 0x5A0001

    private const val STEPS = 5

    /**
     * Where the swap is, out of five: matched = 1 (until our funding is in),
     * funded = 2, they funded = 3, redeemed = 4, done = 5.
     */
    fun progressText(r: SwapRecord): String = when (r.step) {
        Step.MATCHED, Step.NEED_FUNDING -> step(1)
        Step.FUNDED -> step(2)
        Step.THEY_FUNDED -> step(3)
        Step.REDEEMED -> step(4)
        Step.DONE -> step(5)
        Step.PROVISIONAL -> "Swap in progress · posting the offer"
        Step.WAITING_MATCH -> "Swap in progress · waiting for a match"
        Step.REFUNDING -> "Swap in progress · refunding"
        Step.REFUNDED -> "Swap refunded"
        Step.CANCELLED -> "Swap cancelled"
        Step.FAILED -> "Swap failed"
    }

    private fun step(n: Int) = "Swap in progress · step $n of $STEPS"

    /** Time left, coarse: "1h 52m", "52m", or "<1m" under a minute (or past it). */
    fun formatDeadline(secondsLeft: Long): String {
        if (secondsLeft < 60) return "<1m"
        val h = secondsLeft / 3600
        val m = secondsLeft % 3600 / 60
        return if (h > 0) "${h}h ${m}m" else "${m}m"
    }

    /**
     * The "your turn to fund" text. [withDeadline] holds `%1$s` for the time
     * left; with no deadline known the plain [plain] text is used rather than
     * guessing one.
     */
    fun needFundingText(withDeadline: String, plain: String, deadline: Long?, now: Long): String =
        if (deadline == null) plain else withDeadline.format(formatDeadline(deadline - now))

    fun createChannels(ctx: Context) {
        val nm = ctx.getSystemService(NotificationManager::class.java) ?: return
        nm.createNotificationChannel(
            NotificationChannel(
                CHANNEL_PROGRESS, ctx.getString(R.string.swap_channel_progress), NotificationManager.IMPORTANCE_LOW,
            ).apply { description = ctx.getString(R.string.swap_channel_progress_desc) },
        )
        nm.createNotificationChannel(
            NotificationChannel(
                CHANNEL_EVENTS, ctx.getString(R.string.swap_channel_events), NotificationManager.IMPORTANCE_HIGH,
            ).apply { description = ctx.getString(R.string.swap_channel_events_desc) },
        )
    }

    /** Opens the app (its lock screen first, if locked). */
    private fun openApp(ctx: Context, requestCode: Int): PendingIntent? {
        val launch = ctx.packageManager.getLaunchIntentForPackage(ctx.packageName)?.apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP
        } ?: return null
        return PendingIntent.getActivity(
            ctx, requestCode, launch, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }

    /** The foreground service's persistent notification. */
    fun progress(ctx: Context, text: String): Notification =
        NotificationCompat.Builder(ctx, CHANNEL_PROGRESS)
            .setSmallIcon(R.drawable.ic_mark)
            .setContentTitle(ctx.getString(R.string.swap_notification_title))
            .setContentText(text)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setSilent(true)
            .setCategory(NotificationCompat.CATEGORY_PROGRESS)
            .setContentIntent(openApp(ctx, PROGRESS_ID))
            .build()

    /** Replace the persistent notification's text (no-op without the permission). */
    fun updateProgress(ctx: Context, text: String) = post(ctx, PROGRESS_ID, progress(ctx, text))

    /** Alert the user to [e] on swap [r]. */
    fun event(ctx: Context, r: SwapRecord, e: Event, now: Long = System.currentTimeMillis() / 1000) {
        val (title, text) = when (e) {
            Event.MATCHED -> R.string.swap_event_matched_title to ctx.getString(R.string.swap_event_matched)
            Event.THEY_FUNDED -> R.string.swap_event_they_funded_title to ctx.getString(R.string.swap_event_they_funded)
            Event.NEED_FUNDING -> R.string.swap_event_need_funding_title to
                needFundingText(
                    ctx.getString(R.string.swap_event_need_funding, "%1\$s"),
                    ctx.getString(R.string.swap_event_need_funding_plain),
                    r.deadline, now,
                )
            Event.DONE -> R.string.swap_event_done_title to ctx.getString(R.string.swap_event_done)
            Event.FAILED -> R.string.swap_event_failed_title to ctx.getString(R.string.swap_event_failed)
            Event.REFUNDING -> R.string.swap_event_refunding_title to ctx.getString(R.string.swap_event_refunding)
        }
        // One alert per swap: a newer event replaces the older one.
        val id = r.id.hashCode().let { if (it == PROGRESS_ID) it + 1 else it }
        val n = NotificationCompat.Builder(ctx, CHANNEL_EVENTS)
            .setSmallIcon(R.drawable.ic_mark)
            .setContentTitle(ctx.getString(title))
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setCategory(if (e == Event.NEED_FUNDING) NotificationCompat.CATEGORY_REMINDER else NotificationCompat.CATEGORY_STATUS)
            .setAutoCancel(true)
            .setContentIntent(openApp(ctx, id))
            .build()
        post(ctx, id, n)
    }

    private fun canPost(ctx: Context): Boolean =
        Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
            ContextCompat.checkSelfPermission(ctx, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED

    private fun post(ctx: Context, id: Int, n: Notification) {
        if (!canPost(ctx)) return
        try {
            NotificationManagerCompat.from(ctx).notify(id, n)
        } catch (_: SecurityException) {
            // revoked between the check and the post
        }
    }

    /**
     * Ask for `POST_NOTIFICATIONS` (API 33+) unless it is already granted. Called
     * the first time the user confirms a swap; the system itself stops asking
     * once the user has declined twice.
     */
    fun requestNotificationPermission(activity: Activity) {
        if (canPost(activity)) return
        ActivityCompat.requestPermissions(activity, arrayOf(Manifest.permission.POST_NOTIFICATIONS), REQUEST_NOTIFICATIONS)
    }

    private const val REQUEST_NOTIFICATIONS = 0x5A01
}
