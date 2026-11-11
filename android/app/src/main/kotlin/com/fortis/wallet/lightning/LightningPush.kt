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

    /** Where the registration of the Lightning wallet on [walletChain] is remembered: BTC's
     *  key is the one it always had; XBT has its own, so the two nodes don't take turns. */
    private fun registeredKey(walletChain: String) =
        if (walletChain == com.fortis.wallet.data.LN_XBT_CHAIN) "${KEY_REGISTERED}_xbt" else KEY_REGISTERED

    /** True when [nodeId] (the node of the Lightning wallet on [walletChain]) already registered [token] with its LSP. */
    fun isRegistered(ctx: Context, walletChain: String, nodeId: String, token: String): Boolean =
        prefs(ctx).getString(registeredKey(walletChain), null) == "$nodeId|$token"

    fun markRegistered(ctx: Context, walletChain: String, nodeId: String, token: String) {
        prefs(ctx).edit().putString(registeredKey(walletChain), "$nodeId|$token").apply()
    }

    /** A new token: registered again at the next chance (both nodes). */
    fun forgetRegistration(ctx: Context) {
        prefs(ctx).edit().remove(KEY_REGISTERED).remove(registeredKey(com.fortis.wallet.data.LN_XBT_CHAIN)).apply()
    }

    private fun prefs(ctx: Context) = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /** Where the nonce of the notification last posted for [walletChain]'s wallet is kept. */
    private fun nonceKey(walletChain: String) = "tap_nonce_$walletChain"

    /** A new single-use nonce for the notification about to be posted for [walletChain]'s
     *  wallet (it replaces that chain's last one, as the notification does). */
    private fun newNonce(ctx: Context, walletChain: String): String {
        val nonce = com.fortis.wallet.wallet.randomBytes(16).joinToString("") { "%02x".format(it) }
        prefs(ctx).edit().putString(nonceKey(walletChain), nonce).commit()
        return nonce
    }

    /** True (once) when [nonce] is the one stored for [walletChain]'s last notification: a tap on
     *  a notification this app posted. Used up either way it matches. */
    fun consumeNonce(ctx: Context, walletChain: String, nonce: String?): Boolean {
        if (nonce == null) return false
        val p = prefs(ctx)
        val stored = p.getString(nonceKey(walletChain), null) ?: return false
        if (!java.security.MessageDigest.isEqual(stored.toByteArray(), nonce.toByteArray())) return false
        p.edit().remove(nonceKey(walletChain)).apply()
        return true
    }

    /** Shows [title] / [body]; tapping opens the app (its lock screen first), then the Lightning
     *  wallet [data]'s `chain` names. Each chain has its own notification. */
    fun show(ctx: Context, title: String, body: String, data: Map<String, String> = emptyMap()) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(ctx, Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) return
        // The same extras a tap on a background notification delivers (FCM puts the data there),
        // under this app's own action with a single-use nonce: MainActivity is exported, so
        // extras alone prove nothing (see [pushedWalletChain]).
        val chain = pushTargetChain(data)
        val launch = ctx.packageManager.getLaunchIntentForPackage(ctx.packageName)?.apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP
            action = LIGHTNING_PUSH_ACTION
            data.forEach { (k, v) -> putExtra(k, v) }
            putExtra(LIGHTNING_PUSH_NONCE, newNonce(ctx, chain))
        }
        val id = if (chain == com.fortis.wallet.data.LN_XBT_CHAIN) NOTIFICATION_ID + 1 else NOTIFICATION_ID
        val open = launch?.let {
            PendingIntent.getActivity(ctx, id, it, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
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
            NotificationManagerCompat.from(ctx).notify(id, n)
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
        LightningPush.show(applicationContext, n.title ?: return, n.body ?: "", message.data)
    }
}

/** The push data's `kind` values the LSP sends (push.rs): a payment held for, or received by, a node. */
private val LIGHTNING_PUSH_KINDS = setOf("ln_held", "ln_received")

/** True when [data] (a push's data, or the extras a tap on it delivered) is a Lightning push. */
fun isLightningPush(data: Map<String, String>): Boolean = data["kind"] in LIGHTNING_PUSH_KINDS

/** The Lightning wallet a push opens: "ln-xbt" when its data `chain` is "xbt", else "ln"
 *  (an older LSP sends no `chain`: those are all BTC). */
fun pushTargetChain(data: Map<String, String>): String =
    if (data["chain"]?.trim()?.lowercase() == "xbt") com.fortis.wallet.data.LN_XBT_CHAIN else com.fortis.wallet.data.LN_CHAIN

/** The action of the intent a notification [LightningPush.show] posted opens. */
const val LIGHTNING_PUSH_ACTION = "com.fortis.wallet.action.OPEN_LIGHTNING_PUSH"

/** The extra carrying that notification's single-use nonce. */
const val LIGHTNING_PUSH_NONCE = "com.fortis.wallet.extra.PUSH_NONCE"

/** The extra FCM puts on the intent of a notification the system posted for a message. */
private const val FCM_MESSAGE_ID = "google.message_id"

/** The Lightning wallet a tap on a push opens, from the intent's extras ([data]) and [action];
 *  null when they are not a Lightning push, or when the activity was reopened from Recents
 *  ([launchedFromHistory], `FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY`): Android hands it the old
 *  push's extras again, which are not a new tap.
 *
 *  MainActivity is exported, so any app can start it with push-looking extras. A notification
 *  this app posted ([LightningPush.show]) comes with [LIGHTNING_PUSH_ACTION] and must carry the
 *  nonce stored for its chain ([consumeNonce]: true once, for the stored one). Otherwise it
 *  must be one the system posted from an FCM message: FCM's `google.message_id` extra and a
 *  Lightning `kind`. Residual: another app can forge that last shape — it can only switch the
 *  wallet in view (behind the app lock), never pay or move funds. */
fun pushedWalletChain(
    data: Map<String, String>,
    action: String?,
    launchedFromHistory: Boolean,
    consumeNonce: (walletChain: String, nonce: String?) -> Boolean,
): String? {
    if (launchedFromHistory || !isLightningPush(data)) return null
    val chain = pushTargetChain(data)
    return when {
        action == LIGHTNING_PUSH_ACTION -> chain.takeIf { consumeNonce(it, data[LIGHTNING_PUSH_NONCE]) }
        !data[FCM_MESSAGE_ID].isNullOrBlank() -> chain
        else -> null
    }
}
