package com.fortis.wallet

import android.os.Build
import org.json.JSONObject
import java.io.PrintWriter
import java.io.StringWriter
import java.net.HttpURLConnection
import java.net.URL

/**
 * Minimal, self-hosted crash reporting. On an uncaught exception it POSTs a small
 * JSON report to the fortis backend and then lets the normal crash proceed.
 *
 * Best-effort: the report is sent synchronously on the dying thread with a short
 * timeout and no retry — if the device is offline the report is lost. It never
 * contains wallet data (no addresses, keys, transactions, or the recovery phrase).
 */
object CrashReporter {
    private const val ENDPOINT = "$HOSTED_EDGE/crash"

    fun install() {
        val previous = Thread.getDefaultUncaughtExceptionHandler()
        Thread.setDefaultUncaughtExceptionHandler { thread, error ->
            // The handler runs on the crashing thread, often the main thread, so
            // the send can't happen inline (NetworkOnMainThreadException). Hand it
            // to a worker and wait briefly before letting the process die.
            val worker = Thread {
                runCatching { send(thread, error) }
                    .onFailure { android.util.Log.w("CrashReporter", "report not sent: $it") }
            }
            worker.start()
            runCatching { worker.join(4_000) }
            previous?.uncaughtException(thread, error)
        }
    }

    private fun send(thread: Thread, error: Throwable) {
        post(reportBody(thread.name, error, where = null))
    }

    /** One JSON report; `where` marks a caught (non-fatal) error, absent for a real crash. */
    internal fun reportBody(threadName: String, error: Throwable, where: String?): String {
        val trace = StringWriter().also { error.printStackTrace(PrintWriter(it)) }.toString()
        return JSONObject().apply {
            put("app_version", BuildConfig.VERSION_NAME)
            put("version_code", BuildConfig.VERSION_CODE)
            put("build_type", if (BuildConfig.DEBUG) "debug" else "release")
            put("android_sdk", Build.VERSION.SDK_INT)
            put("model", Build.MODEL)
            put("manufacturer", Build.MANUFACTURER)
            put("thread", threadName)
            if (where != null) put("caught_at", where)
            put("exception", error.javaClass.name)
            put("message", redact(error.message ?: ""))
            put("stack", redact(trace).take(12_000))
        }.toString()
    }

    private val caughtSeen = HashMap<String, Long>()
    private const val CAUGHT_REPEAT_MS = 60 * 60 * 1000L

    /** True at most once per [CAUGHT_REPEAT_MS] for the same key, so a retry loop can't flood. */
    internal fun firstInWindow(key: String, now: Long, seen: MutableMap<String, Long> = caughtSeen): Boolean =
        synchronized(seen) {
            val last = seen[key]
            if (last != null && now - last < CAUGHT_REPEAT_MS) false else { seen[key] = now; true }
        }

    /**
     * Report an error the app caught and showed instead of crashing (e.g. an unlock that
     * failed before the system prompt). Same redaction and endpoint as a crash; best effort,
     * off the calling thread, never throws, at most once an hour per place and exception.
     */
    fun reportCaught(where: String, error: Throwable) {
        if (!firstInWindow("$where|${error.javaClass.name}", System.currentTimeMillis())) return
        Thread {
            runCatching { post(reportBody("caught", error, where)) }
                .onFailure { android.util.Log.w("CrashReporter", "report not sent: $it") }
        }.start()
    }

    /**
     * Strip anything that could be wallet data before a report leaves the device:
     * bech32 addresses, extended keys, and long hex runs (private keys, script
     * hex, txids). Defensive — no current code path puts these in an exception
     * message or stack trace, and the recovery phrase never appears in either
     * (`crypto::unseal` only ever returns it as a success value) — but the report
     * must not carry them even if that changes.
     */
    internal fun redact(s: String): String = s
        .replace(Regex("(?i)\\b(bc|tb|bcrt)1[a-z0-9]{8,}"), "<addr>")
        .replace(Regex("\\b[xtyzuv]p(ub|rv)[1-9A-HJ-NP-Za-km-z]{40,}"), "<xkey>")
        .replace(Regex("\\b[13mn2][1-9A-HJ-NP-Za-km-z]{25,34}\\b"), "<addr>")
        .replace(Regex("\\b[0-9a-fA-F]{64,}\\b"), "<hex>")

    private fun post(body: String) {
        (URL(ENDPOINT).openConnection() as HttpURLConnection).run {
            requestMethod = "POST"
            connectTimeout = 2_000
            readTimeout = 3_000
            doOutput = true
            setRequestProperty("Content-Type", "application/json")
            outputStream.use { it.write(body.toByteArray()) }
            responseCode // force the request
            disconnect()
        }
    }
}
