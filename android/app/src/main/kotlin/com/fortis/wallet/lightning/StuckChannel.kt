package com.fortis.wallet.lightning

/**
 * A channel can wait for ever for a funding transaction that never reached the network: ldk-node
 * broadcasts it once and never again, and a refused broadcast (found live 2026-11-16: the edge
 * answered 402) is only logged. The app therefore asks the network whether it has the funding
 * transaction of a pending channel, and when it does not for [STUCK_AFTER_MS] the channel is
 * stuck: the user is told, and can cancel it and get the funds back.
 */

/** How long the network may lack a pending channel's funding transaction before it is called stuck. */
const val STUCK_AFTER_MS = 20L * 1000

/**
 * The time the network was first seen lacking the pending funding transaction, given the
 * previous value. [onNetwork] is the network's answer (`null`: unknown — offline or a server
 * error — which is no evidence either way, so the clock neither starts nor stops).
 */
fun nextMissingSince(prev: Long?, pendingFundingTxid: String?, onNetwork: Boolean?, nowMs: Long): Long? = when {
    pendingFundingTxid == null -> null
    onNetwork == true -> null
    onNetwork == false -> prev ?: nowMs
    else -> prev
}

fun isChannelStuck(missingSince: Long?, nowMs: Long): Boolean =
    missingSince != null && nowMs - missingSince >= STUCK_AFTER_MS

/** What an Esplora `GET /tx/<txid>/status` answer says about the network having the transaction:
 *  200 it has it (in the mempool or a block), 404 it does not, anything else is no answer. */
fun fundingKnownFromStatus(httpCode: Int): Boolean? = when (httpCode) {
    200 -> true
    404 -> false
    else -> null
}

/** Asks [esploraUrl] (the chain's Esplora, with [authHeader] for the hosted edge) whether it has
 *  [txid]. `null` when it can't tell (offline, an error). */
suspend fun txKnownToNetwork(
    http: okhttp3.OkHttpClient,
    esploraUrl: String,
    txid: String,
    authHeader: String?,
): Boolean? = kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
    runCatching {
        val req = okhttp3.Request.Builder().url("${esploraUrl.trimEnd('/')}/tx/$txid/status").get()
            .apply { if (!authHeader.isNullOrBlank()) header("Authorization", authHeader) }
            .build()
        http.newCall(req).execute().use { fundingKnownFromStatus(it.code) }
    }.getOrNull()
}
