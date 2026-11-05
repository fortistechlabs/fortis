package com.fortis.wallet.lightning

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import com.fortis.wallet.R
import com.google.firebase.messaging.FirebaseMessaging
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlin.coroutines.resume

/**
 * Push notifications for Lightning: the LSP holds a payment for this phone while it is
 * offline and wakes it through Firebase Cloud Messaging ("Incoming Lightning payment").
 * The phone registers its FCM token with the LSP (account op `push_register`), again
 * whenever the token or the Lightning node changes.
 */
object LightningPush {
    /** Must match the `channel_id` fortis-lsp sends (push.rs). */
    const val CHANNEL = "lightning"
    private const val PREFS = "lightning_push"
    private const val KEY_REGISTERED = "registered"
    private const val NOTIFICATION_ID = 0x4C4E01

    fun createChannel(ctx: Context) {
        val nm = ctx.getSystemService(NotificationManager::class.java) ?: return
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL, ctx.getString(R.string.ln_push_channel), NotificationManager.IMPORTANCE_HIGH)
                .apply { description = ctx.getString(R.string.ln_push_channel_desc) },
        )
    }

    /** This phone's FCM token, or null (no Google Play services, or no network yet).
     *  Auto-init is off in the manifest; asking here (only with a Lightning wallet)
     *  turns it on, so the token is kept fresh from then on. */
    suspend fun token(): String? = runCatching {
        suspendCancellableCoroutine<String?> { cont ->
            val fcm = FirebaseMessaging.getInstance()
            fcm.isAutoInitEnabled = true
            fcm.token.addOnCompleteListener { t ->
                cont.resume(if (t.isSuccessful) t.result else null)
            }
        }
    }.getOrNull()

    /** True when [nodeId] already registered [token] with the LSP. */
    fun isRegistered(ctx: Context, nodeId: String, token: String): Boolean =
        prefs(ctx).getString(KEY_REGISTERED, null) == "$nodeId|$token"

    fun markRegistered(ctx: Context, nodeId: String, token: String) {
        prefs(ctx).edit().putString(KEY_REGISTERED, "$nodeId|$token").apply()
    }

    /** A new token: registered again at the next chance. */
    fun forgetRegistration(ctx: Context) {
        prefs(ctx).edit().remove(KEY_REGISTERED).apply()
    }

    private fun prefs(ctx: Context) = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /** Shows [title] / [body]; tapping opens the app (its lock screen first). */
    fun show(ctx: Context, title: String, body: String) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(ctx, Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) return
        val launch = ctx.packageManager.getLaunchIntentForPackage(ctx.packageName)?.apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP
        }
        val open = launch?.let {
            PendingIntent.getActivity(ctx, NOTIFICATION_ID, it, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
        }
        val n = NotificationCompat.Builder(ctx, CHANNEL)
            .setSmallIcon(R.drawable.ic_mark)
            .setContentTitle(title)
            .setContentText(body)
            .setStyle(NotificationCompat.BigTextStyle().bigText(body))
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setCategory(NotificationCompat.CATEGORY_STATUS)
            .setAutoCancel(true)
            .setContentIntent(open)
            .build()
        try {
            NotificationManagerCompat.from(ctx).notify(NOTIFICATION_ID, n)
        } catch (_: SecurityException) {
            // permission revoked between the check and the post
        }
    }
}

/**
 * Receives the LSP's pushes. In the background the system shows the notification
 * itself; while the app is in front Firebase hands it here instead, so it is shown here.
 */
class FortisMessagingService : FirebaseMessagingService() {
    override fun onNewToken(token: String) {
        LightningPush.forgetRegistration(applicationContext)
    }

    override fun onMessageReceived(message: RemoteMessage) {
        val n = message.notification ?: return
        LightningPush.show(applicationContext, n.title ?: return, n.body ?: "")
    }
}
