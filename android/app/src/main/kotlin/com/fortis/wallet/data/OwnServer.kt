package com.fortis.wallet.data

import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import okhttp3.OkHttpClient
import okhttp3.Request
import java.io.IOException
import java.util.concurrent.TimeUnit

/**
 * The user's own Esplora server for a chain, checked before it's saved. BTC and
 * XBT share their history up to the split, so a server's genesis block or tip
 * alone can't say which chain it follows: its block at [FORK_CHECK_HEIGHT] can,
 * since the two chains already hold different blocks there.
 */
const val FORK_CHECK_HEIGHT = 961_640L

/** Each chain's block hash at [FORK_CHECK_HEIGHT] (read from our Core and Knots nodes, 2026-10-22). */
val FORK_CHECK_HASH = mapOf(
    "btc" to "00000000000000000001d82da6ecccf08e07afa383f9212b0e1b95cc72430c00",
    "xbt" to "0000000000000050c1e5f69672f459293be14f46e5a494e7a8c8541396f18eeb",
)

sealed interface OwnServerCheck {
    /** [url] is normalized: trimmed, no trailing `/`, ending in the API root. */
    data class Ok(val url: String) : OwnServerCheck
    data object NotHttps : OwnServerCheck
    data object Unreachable : OwnServerCheck
    data object NotEsplora : OwnServerCheck
    data object WrongChain : OwnServerCheck
    data class NotSynced(val tip: Long) : OwnServerCheck
}

/**
 * Check [url] as [chain]'s Esplora server: HTTPS, a numeric `/blocks/tip/height`
 * (tried as given, then with `/api` added), synced past [FORK_CHECK_HEIGHT],
 * and on [chain] there. Stops at the first failure.
 */
suspend fun validateOwnServer(http: OkHttpClient, chain: String, url: String): OwnServerCheck {
    val parsed = url.trim().toHttpUrlOrNull()
    if (parsed?.isHttps != true) return OwnServerCheck.NotHttps
    // OkHttp's canonical form, so every later parser of the saved URL agrees.
    val given = parsed.toString().trimEnd('/')
    val client = http.newBuilder().callTimeout(10, TimeUnit.SECONDS).build()

    /** (code, body), or null when no answer came back at all. */
    suspend fun get(u: String): Pair<Int, String>? = try {
        client.send(Request.Builder().url(u).build()) { r -> r.code to r.body?.string().orEmpty() }
    } catch (e: IOException) {
        null
    }

    val candidates = if (given.endsWith("/api")) listOf(given) else listOf(given, "$given/api")
    var answered = false
    var base: String? = null
    var tip = 0L
    for (c in candidates) {
        val (code, body) = get("$c/blocks/tip/height") ?: continue
        answered = true
        val t = body.trim().toLongOrNull()
        if (code in 200..299 && t != null) { base = c; tip = t; break }
    }
    if (base == null) return if (answered) OwnServerCheck.NotEsplora else OwnServerCheck.Unreachable
    if (tip < FORK_CHECK_HEIGHT) return OwnServerCheck.NotSynced(tip)

    val want = FORK_CHECK_HASH[chain] ?: return OwnServerCheck.WrongChain
    val (code, body) = get("$base/block-height/$FORK_CHECK_HEIGHT") ?: return OwnServerCheck.Unreachable
    if (code !in 200..299) return OwnServerCheck.NotEsplora
    return if (body.trim().lowercase() == want) OwnServerCheck.Ok(base) else OwnServerCheck.WrongChain
}

/** [url]'s host as OkHttp reads it (underscores and all), or null if it isn't a URL. */
fun ownServerHost(url: String): String? = url.toHttpUrlOrNull()?.host

sealed interface BackendTarget {
    /** The hosted fortis-edge: token, pricing, batch scan. */
    data object Edge : BackendTarget
    /** A plain Esplora server: no token, no pricing, no batch scan. */
    data class Esplora(val url: String) : BackendTarget
}

data class BackendPlan(val primary: BackendTarget, val fallback: BackendTarget?)

/** Where a wallet on [chain]/[network] reads the chain. An own server (mainnet
 *  only) stands alone — no fallback, so nothing leaks elsewhere when it's down. */
fun chooseBackend(chain: String, network: String, ownServer: String?): BackendPlan =
    if (network == "mainnet" && ownServer != null) BackendPlan(BackendTarget.Esplora(ownServer), null)
    else BackendPlan(
        BackendTarget.Edge,
        BackendTarget.Esplora(if (chain == "btc") com.fortis.wallet.PUBLIC_BTC_ESPLORA else com.fortis.wallet.PUBLIC_XBT_ESPLORA),
    )
