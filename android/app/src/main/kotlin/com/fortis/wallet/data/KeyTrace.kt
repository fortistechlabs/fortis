package com.fortis.wallet.data

import android.app.KeyguardManager
import android.content.Context
import android.os.Build
import android.util.Log
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricManager.Authenticators.BIOMETRIC_STRONG
import androidx.biometric.BiometricManager.Authenticators.DEVICE_CREDENTIAL
import com.fortis.wallet.BuildConfig

/**
 * Persistent trace of the app's unlock key, in every build, so a lost key can be
 * explained: when the screen lock changed, when the key was created or deleted, what
 * each failed unlock saw. Small (a ring buffer), local, and free of secrets; it leaves
 * the phone only when the user sends or shares the report ([report]).
 */
object KeyTrace {
    private const val TAG = "KeyTrace"
    private const val PREFS = "key_trace"
    private var app: Context? = null

    private fun prefs() = app?.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /** Call once from `Application.onCreate`. Notes a change of the screen-lock state since the last launch. */
    fun init(ctx: Context) {
        app = ctx.applicationContext
        runCatching {
            val secure = isDeviceSecure(ctx)?.toString() ?: "unknown"
            val prev = prefs()?.getString("last_secure", null)
            if (prev != secure) {
                record("screen lock set: ${prev ?: "(first launch)"} -> $secure")
                prefs()?.edit()?.putString("last_secure", secure)?.apply()
            }
        }
    }

    /** Add a timestamped line. Never throws; identical consecutive lines are kept once. */
    fun record(text: String) {
        Log.i(TAG, text)
        runCatching {
            val p = prefs() ?: return
            val next = KeyTraceLog.append(KeyTraceLog.decode(p.getString("events", null)), TraceEvent(System.currentTimeMillis(), text))
            p.edit().putString("events", KeyTraceLog.encode(next)).apply()
        }
    }

    /** The key opened: remember when. */
    fun unlockOk() {
        runCatching { prefs()?.edit()?.putLong("last_ok", System.currentTimeMillis())?.apply() }
    }

    /** An unlock failed: keep the diagnosis and where it was thrown, and add it to the history. */
    fun failure(t: Throwable, detail: String) {
        Log.w(TAG, "unlock failed: $detail", t)
        runCatching {
            prefs()?.edit()
                ?.putString("failure_detail", detail)
                ?.putString("failure_frames", frames(t).joinToString("\n"))
                ?.apply()
            record("unlock failed: ${scrubForReport(detail)}; screen lock set: ${isDeviceSecure(app ?: return)}")
        }
    }

    /** The root cause's first frames, then the first few of ours. */
    internal fun frames(t: Throwable): List<String> {
        val chain = generateSequence(t) { it.cause }.take(8).toList()
        val root = chain.last()
        fun f(e: StackTraceElement) = "${e.className.substringAfterLast('.')}.${e.methodName}(${e.fileName}:${e.lineNumber})"
        val top = root.stackTrace.take(4).map(::f)
        val ours = chain.flatMap { it.stackTrace.asList() }.filter { it.className.startsWith("com.fortis.") }.take(4).map(::f)
        return (listOf("${root.javaClass.name}: ${root.message.orEmpty().take(160)}") + top + ours).distinct()
    }

    private fun isDeviceSecure(ctx: Context): Boolean? =
        runCatching { (ctx.getSystemService(Context.KEYGUARD_SERVICE) as KeyguardManager).isDeviceSecure }.getOrNull()

    private fun biometricStatus(ctx: Context): String = runCatching {
        when (BiometricManager.from(ctx).canAuthenticate(BIOMETRIC_STRONG or DEVICE_CREDENTIAL)) {
            BiometricManager.BIOMETRIC_SUCCESS -> "BIOMETRIC_SUCCESS"
            BiometricManager.BIOMETRIC_ERROR_NONE_ENROLLED -> "NONE_ENROLLED"
            BiometricManager.BIOMETRIC_ERROR_NO_HARDWARE -> "NO_HARDWARE"
            BiometricManager.BIOMETRIC_ERROR_HW_UNAVAILABLE -> "HW_UNAVAILABLE"
            BiometricManager.BIOMETRIC_ERROR_SECURITY_UPDATE_REQUIRED -> "SECURITY_UPDATE_REQUIRED"
            BiometricManager.BIOMETRIC_ERROR_UNSUPPORTED -> "UNSUPPORTED"
            else -> "UNKNOWN"
        }
    }.getOrElse { "error: ${it.javaClass.simpleName}" }

    /** The report text for support: current device and key state plus the history. */
    fun report(ctx: Context, lockMode: String?, wrapped: String?): String {
        val (presence, note) = SeedKeystore.keyPresence()
        val p = prefs()
        val snap = KeySnapshot(
            appVersion = "${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE})",
            manufacturer = Build.MANUFACTURER, model = Build.MODEL,
            release = Build.VERSION.RELEASE, sdk = Build.VERSION.SDK_INT,
            securityPatch = Build.VERSION.SECURITY_PATCH.orEmpty(),
            deviceSecure = isDeviceSecure(ctx), biometricStatus = biometricStatus(ctx),
            lockMode = lockMode, wrappedLength = wrapped?.length ?: 0,
            keyPresence = presence, keyNote = note,
            keyInfo = if (presence == KeyPresence.PRESENT) SeedKeystore.keyInfoText() else null,
            aliases = SeedKeystore.aliases(),
        )
        val detail = p?.getString("failure_detail", null)
        val failure = detail?.let { FailureNote(it, p.getString("failure_frames", "").orEmpty().lines().filter(String::isNotBlank)) }
        val ok = p?.getLong("last_ok", 0L)?.takeIf { it > 0 }
        return formatKeyReport(snap, System.currentTimeMillis(), ok, KeyTraceLog.decode(p?.getString("events", null)), failure)
    }
}
