package com.fortis.wallet.data

import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.TimeZone

/**
 * The key-loss trace: a plain-text report of what the app can tell about its unlock key,
 * for the user to send to support. It names the phone, the screen-lock state, the Keystore
 * key and what happened to it over time. It never contains a recovery phrase, a key, an
 * address or any wallet data, and exception text is scrubbed of long tokens.
 */

/** Does the Keystore hold the app key? [ERROR] = the check itself failed (not the same as missing). */
enum class KeyPresence { PRESENT, MISSING, ERROR }

/** Everything read from the device for one report. All plain values, so formatting is testable. */
data class KeySnapshot(
    val appVersion: String,
    val manufacturer: String,
    val model: String,
    val release: String,
    val sdk: Int,
    val securityPatch: String,
    /** `KeyguardManager.isDeviceSecure`; null when it could not be read. */
    val deviceSecure: Boolean?,
    val biometricStatus: String,
    val lockMode: String?,
    val wrappedLength: Int,
    val keyPresence: KeyPresence,
    /** The error behind [KeyPresence.ERROR]. */
    val keyNote: String?,
    /** Where the key lives and what it requires; null when there is no key or it can't be inspected. */
    val keyInfo: String?,
    val aliases: List<String>,
)

/** One line of the key's history. */
data class TraceEvent(val atMs: Long, val text: String)

/** The most recent unlock failure: the diagnosis line and the top stack frames. */
data class FailureNote(val detail: String, val frames: List<String>)

/** A small persisted ring buffer of [TraceEvent]s, as one text blob ("ms|text" per line). */
object KeyTraceLog {
    const val MAX = 40
    private const val MAX_TEXT = 300

    fun append(existing: List<TraceEvent>, e: TraceEvent): List<TraceEvent> {
        val clean = e.copy(text = clean(e.text))
        if (existing.lastOrNull()?.text == clean.text) return existing
        return (existing + clean).takeLast(MAX)
    }

    fun encode(list: List<TraceEvent>): String = list.joinToString("\n") { "${it.atMs}|${clean(it.text)}" }

    fun decode(raw: String?): List<TraceEvent> = raw.orEmpty().lineSequence().mapNotNull { line ->
        val i = line.indexOf('|')
        val ms = if (i > 0) line.substring(0, i).toLongOrNull() else null
        if (ms == null) null else TraceEvent(ms, line.substring(i + 1))
    }.toList().takeLast(MAX)

    private fun clean(t: String) = t.replace('\n', ' ').replace('\r', ' ').replace('|', '/').take(MAX_TEXT)
}

/** Long alphanumeric runs (addresses, hex, keys) are hidden; ordinary words and numbers stay. */
fun scrubForReport(s: String): String = s.replace(Regex("[A-Za-z0-9]{26,}"), "<x>")

private fun utc(ms: Long): String =
    SimpleDateFormat("yyyy-MM-dd HH:mm:ss'Z'", Locale.US).apply { timeZone = TimeZone.getTimeZone("UTC") }.format(Date(ms))

fun formatKeyReport(
    s: KeySnapshot, nowMs: Long, lastOkMs: Long?, events: List<TraceEvent>, failure: FailureNote?,
): String = buildString {
    appendLine("Fortis key diagnostics")
    appendLine("app: ${s.appVersion}")
    appendLine("report time: ${utc(nowMs)}")
    appendLine("device: ${s.manufacturer} ${s.model}, Android ${s.release} (SDK ${s.sdk}), security patch ${s.securityPatch}")
    appendLine("screen lock set: " + when (s.deviceSecure) { true -> "yes"; false -> "NO"; null -> "unknown" })
    appendLine("strong biometric or credential: ${s.biometricStatus}")
    appendLine(
        "lock mode: ${s.lockMode ?: "none"}; wrapped secret: " +
            if (s.wrappedLength > 0) "present (${s.wrappedLength} chars)" else "absent",
    )
    appendLine(
        "key fortis.app: " + when (s.keyPresence) {
            KeyPresence.PRESENT -> "present"
            KeyPresence.MISSING -> "MISSING"
            KeyPresence.ERROR -> "ERROR (${scrubForReport(s.keyNote ?: "unknown")})"
        },
    )
    appendLine("key properties: ${s.keyInfo ?: "n/a"}")
    appendLine("keystore aliases: ${s.aliases.joinToString(", ").ifEmpty { "none" }}")
    appendLine("last successful unlock: ${lastOkMs?.let { utc(it) } ?: "never recorded"}")
    if (failure == null) appendLine("last failure: none") else {
        appendLine("last failure: ${scrubForReport(failure.detail)}")
        failure.frames.forEach { appendLine("  at ${scrubForReport(it)}") }
    }
    appendLine("events (UTC, oldest first):")
    if (events.isEmpty()) appendLine("  (none)") else events.forEach { appendLine("  ${utc(it.atMs)}  ${scrubForReport(it.text)}") }
}.trimEnd()
