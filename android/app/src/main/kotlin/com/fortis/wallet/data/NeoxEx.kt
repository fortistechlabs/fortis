package com.fortis.wallet.data

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONObject
import java.io.IOException

/**
 * NeoxEX `BTCB2_BTC` price: sat of BTC per 1 XBT — the phone's own view, checked
 * against a match's terms before accepting it.
 *
 * A port of fortis-edge's `swap::neoxex` (same rule, same constants, same test
 * fixtures): per side, the volume-weighted price of the best levels up to
 * [DEPTH_BTC] of notional; `mid` of the two. `null` wherever the edge refuses to
 * quote — paused (ticker flag or `/pairs` `tradingPaused`), stale (ticker
 * `computedAt` more than [MAX_AGE_SECS] old; missing → refuse), an empty side, a
 * spread over [MAX_SPREAD], or a mid more than [MAX_FROM_LAST] from the last trade.
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

    /** Sat of BTC per 1 XBT from the three NeoxEX bodies, or `null` where the edge
     *  would refuse. `now` is unix seconds. */
    fun priceFrom(bookJson: String, tickerJson: String, pairsJson: String, now: Long): Long? {
        val book = runCatching { JSONObject(bookJson) }.getOrNull() ?: return null
        val bids = levels(book, "bids", descending = true) ?: return null
        val asks = levels(book, "asks", descending = false) ?: return null
        val t = ticker(tickerJson) ?: return null
        val pairPaused = pairPaused(pairsJson) ?: return null
        if (t.paused || pairPaused) return null
        if (now - t.computedAt > MAX_AGE_SECS) return null
        val bid = sideVwap(bids) ?: return null
        val ask = sideVwap(asks) ?: return null
        val mid = (bid + ask) / 2.0
        if ((ask - bid) / mid > MAX_SPREAD) return null
        if (Math.abs(mid - t.last) / t.last > MAX_FROM_LAST) return null
        return Math.round(mid * 1e8)
    }

    /**
     * Fetch and price. `null` means NeoxEX answered and the rule refuses to
     * quote (paused, stale, too wide, far from last, malformed) — a definite
     * "no". A network or HTTP failure throws instead: "unreachable" is not a
     * refusal, and the swap engine retries it rather than declining.
     */
    suspend fun price(
        http: OkHttpClient,
        base: String = BASE,
        now: Long = System.currentTimeMillis() / 1000,
    ): Long? = withContext(Dispatchers.IO) {
        fun get(path: String): String =
            http.newCall(Request.Builder().url("${base.trimEnd('/')}/api/exchange/$path").build()).execute().use { r ->
                if (!r.isSuccessful) throw IOException("neoxex ${r.code} on $path")
                r.body?.string().orEmpty()
            }
        priceFrom(get("orderbook/$PAIR"), get("ticker/$PAIR"), get("pairs"), now)
    }
}
