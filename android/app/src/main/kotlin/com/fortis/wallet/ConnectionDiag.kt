package com.fortis.wallet

/** What the Settings → Connection card is doing right now. */
enum class ConnBusy { Refreshing, Reconnecting }

fun connectionActivityLabel(b: ConnBusy): String = when (b) {
    ConnBusy.Refreshing -> "Refreshing…"
    ConnBusy.Reconnecting -> "Reconnecting…"
}

/** One debug line for the card after Refresh or Reconnect: what ran, whether it worked, how long
 *  it took, against which server, and the reason when it failed. No secrets, no addresses. */
fun connectionDebugLine(
    action: String,
    ok: Boolean,
    tookMs: Long,
    error: String?,
    server: String,
    detail: String?,
    clock: String,
): String = buildString {
    append(action).append(if (ok) " ok " else " FAILED ").append(clock).append(" · ").append(tookMs).append(" ms")
    if (server.isNotBlank()) append(" · ").append(server)
    if (ok && !detail.isNullOrBlank()) append(" · ").append(detail)
    if (!ok && !error.isNullOrBlank()) append(" · ").append(error)
}
