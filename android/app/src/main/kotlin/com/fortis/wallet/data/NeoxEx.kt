package com.fortis.wallet.data

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONObject
import java.io.IOException

/**
 * NeoxEX `BTCB2_BTC` reference price: sat of BTC per 1 XBT — the phone's own view,
 * shown next to the price the user sets. It never blocks a swap.
 *
 * A port of fortis-edge's `swap::neoxex` (same rule, same constants, same test
 * fixtures): per side, the volume-weighted price of the best levels up to
 * [DEPTH_BTC] of notional; `mid` of the two. [Reference.ok] is false wherever the
 * edge would call the reference unreliable — paused (ticker flag or `/pairs`
 * `tradingPaused`), stale (ticker `computedAt` more than [MAX_AGE_SECS] old;
 * missing → not ok), a spread over [MAX_SPREAD], or a mid more than
 * [MAX_FROM_LAST] from the last trade. No price (`null`) only without book data.
 */
object NeoxEx {
    const val DEPTH_BTC = 0.05
    const val MAX_SPREAD = 0.02
    const val MAX_AGE_SECS = 60L
    const val MAX_FROM_LAST = 0.02
    const val BASE = "https://neoxa.exchange"
    private const val PAIR = "BTCB2_BTC"

    private class Ticker(val last: Double, val paused: Boolean, val computedAt: Long)

    /** Levels of one side, best first; `null` if any level is malformed. */
    private fun levels(book: JSONObject, side: String, descending: Boolean): List<Pair<Double, Double>>? {
        val arr = book.optJSONArray(side) ?: return null
        val out = ArrayList<Pair<Double, Double>>(arr.length())
        for (i in 0 until arr.length()) {
            val l = arr.optJSONObject(i) ?: return null
            val p = (l.opt("price") as? Number)?.toDouble()
            val q = (l.opt("quantity") as? Number)?.toDouble()
            if (p == null || q == null || !p.isFinite() || !q.isFinite() || p <= 0.0 || q <= 0.0) return null
            out += p to q
        }
        return if (descending) out.sortedByDescending { it.first } else out.sortedBy { it.first }
    }

    private fun ticker(body: String): Ticker? {
        val t = runCatching { JSONObject(body) }.getOrNull()?.optJSONObject("ticker") ?: return null
        val last = (t.opt("lastPrice") as? Number)?.toDouble()?.takeIf { it.isFinite() && it > 0.0 } ?: return null
        // an integer, as the edge's `as_u64` demands; a string or fraction fails closed
        val v = t.opt("computedAt")
        val ms = (v as? Int)?.toLong() ?: (v as? Long) ?: return null
        if (ms < 0) return null
        val paused = t.optBoolean("tradingPaused", false) || t.optBoolean("paused", false)
        return Ticker(last, paused, ms / 1000)
    }

    /** `null`: the pair isn't listed or the body doesn't parse (fail closed). */
    private fun pairPaused(body: String): Boolean? {
        val pairs = runCatching { JSONObject(body) }.getOrNull()?.optJSONArray("pairs") ?: return null
        for (i in 0 until pairs.length()) {
            val p = pairs.optJSONObject(i) ?: continue
            if (p.optString("pair") == PAIR) return p.optBoolean("tradingPaused", false)
        }
        return null
    }

    private fun sideVwap(levels: List<Pair<Double, Double>>): Double? {
        var btc = 0.0
        var xbt = 0.0
        var rem = DEPTH_BTC
        for ((p, s) in levels) {
            val take = minOf(p * s, rem)
            btc += take
            xbt += take / p
            rem -= take
            if (rem <= 0.0) break
        }
        return if (xbt > 0.0) btc / xbt else null
    }

    /** NeoxEX's depth-weighted mid, sat of BTC per 1 XBT. [ok] is whether the old
     *  quoting rules pass; the price is shown as a reference either way. */
    data class Reference(val priceSatPerXbt: Long, val ok: Boolean)

    /** The [Reference] from the three NeoxEX bodies, or `null` when there is no data
     *  to price from (an unreadable book or an empty side). `now` is unix seconds. */
    fun referenceFrom(bookJson: String, tickerJson: String, pairsJson: String, now: Long): Reference? {
        val book = runCatching { JSONObject(bookJson) }.getOrNull() ?: return null
        val bids = levels(book, "bids", descending = true) ?: return null
        val asks = levels(book, "asks", descending = false) ?: return null
        val bid = sideVwap(bids) ?: return null
        val ask = sideVwap(asks) ?: return null
        val mid = (bid + ask) / 2.0
        val price = Math.round(mid * 1e8).takeIf { it > 0 } ?: return null
        val t = ticker(tickerJson)
        val pairPaused = pairPaused(pairsJson)
        val ok = t != null && pairPaused != null && !t.paused && !pairPaused &&
            now - t.computedAt <= MAX_AGE_SECS &&
            (ask - bid) / mid <= MAX_SPREAD &&
            Math.abs(mid - t.last) / t.last <= MAX_FROM_LAST
        return Reference(price, ok)
    }

    /**
     * Fetch and price. `null` means NeoxEX answered with no data to price from;
     * a network or HTTP failure throws instead: "unreachable" is not "no data".
     */
    suspend fun reference(
        http: OkHttpClient,
        base: String = BASE,
        now: Long = System.currentTimeMillis() / 1000,
    ): Reference? = withContext(Dispatchers.IO) {
        fun get(path: String): String =
            http.newCall(Request.Builder().url("${base.trimEnd('/')}/api/exchange/$path").build()).execute().use { r ->
                if (!r.isSuccessful) throw IOException("neoxex ${r.code} on $path")
                r.body?.string().orEmpty()
            }
        referenceFrom(get("orderbook/$PAIR"), get("ticker/$PAIR"), get("pairs"), now)
    }
}
