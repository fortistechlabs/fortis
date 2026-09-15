package com.fortis.wallet.data

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import okhttp3.MediaType.Companion.toMediaTypeOrNull
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray
import org.json.JSONObject
import uniffi.wallet_ffi.WalletUtxo
import uniffi.wallet_ffi.WalletView

private const val GAP = 20

/** Absolute backstop on how far a single gap-limit walk will go per branch —
 *  not a limit any real wallet should hit (the walk already stops itself after
 *  [GAP] consecutive never-used addresses); just a bound on worst-case work
 *  against a pathological xpub. */
private const val WATCH_SET_HARD_CAP = 2_000

/** Mint a per-install token at a fortis-edge base URL (`POST {base}/register`). */
suspend fun edgeRegister(http: OkHttpClient, base: String): String = withContext(Dispatchers.IO) {
    val url = base.trimEnd('/') + "/register"
    val req = Request.Builder().url(url).post(ByteArray(0).toRequestBody()).build()
    http.newCall(req).execute().use { r ->
        val body = r.body?.string().orEmpty()
        check(r.isSuccessful) { "register failed at $base (${r.code})" }
        JSONObject(body).getString("token")
    }
}

/**
 * Esplora / mempool.space REST backend — no node. Esplora is address-based, so
 * the wallet derives its own addresses (via wallet-ffi's WalletView) and this
 * scans them with a gap limit. Port of web/src/esplora.js.
 *
 * `token` + `refresh` back a fortis-edge that requires `Authorization: Bearer`;
 * a 401 triggers one `refresh()` + retry.
 */
class EsploraBackend(
    private val http: OkHttpClient,
    baseUrl: String,
    private val view: WalletView,
    private val counters: () -> Pair<Int, Int>,
    private var token: String? = null,
    /** `{edge}/pricing` — when set and the edge advertises a service fee, it's
     *  surfaced in [status] so a send attaches the fee output. Null on the
     *  public-explorer fallback → no fee. */
    private val pricingUrl: String? = null,
    /** POST the wallet's address set to `{base}/prewarm` before a scan so the
     *  edge batch-loads them (BTC via Haskoin). Only the hosted BTC backend. */
    private val bulkPrewarm: Boolean = false,
    private val refresh: (suspend () -> String)? = null,
) : Backend {
    private val base = baseUrl.trimEnd('/')
    override val label: String = Regex("https?://([^/]+)").find(base)?.groupValues?.get(1) ?: base
    private var pricing: ServicePricing? = null

    /** Execute `build()`, adding the bearer token; on 401 re-register once and retry. */
    private suspend fun send(build: () -> Request.Builder): okhttp3.Response {
        fun withAuth() = build().apply { token?.let { header("Authorization", "Bearer $it") } }.build()
        var resp = http.newCall(withAuth()).execute()
        if (resp.code == 401 && refresh != null) {
            resp.close()
            token = refresh.invoke()
            resp = http.newCall(withAuth()).execute()
        }
        return resp
    }

    private class WatchAddr(val address: String, val spk: String, val branch: UInt, val index: UInt)

    private var cachedWatchSet: List<Pair<WatchAddr, JSONArray>>? = null
    private var watchSetAt = 0L

    /** Every address worth checking right now, each paired with its `/txs`
     *  response (so [history] never re-fetches what this walk already has).
     *
     *  For each branch, walks forward from the stored next-index in [GAP]
     *  -sized batches, extending the window whenever the batch just checked
     *  had *any* address with transaction history — the standard BIP-44
     *  gap-limit walk — instead of one fixed-size `[from, from+GAP)` pass.
     *  That fixed pass is wrong for a freshly imported watch-only xpub: its
     *  counters start at 0, so a real wallet with more than ~20 used receive
     *  or change addresses had everything past index ~20 silently invisible —
     *  wrong balance, missing history, forever (nothing about the bug is
     *  self-correcting, since the window never had a reason to grow).
     *
     *  "Used" here means *ever* appeared in a transaction (`/txs` non-empty),
     *  not "currently has an unspent output" (`/utxo` non-empty) — an address
     *  that received funds and was later fully spent shows an *empty* `/utxo`
     *  response but is still a real, used address. Deciding gap continuation
     *  on `/utxo` alone stops the walk right after a run of spent-through
     *  addresses, before it ever reaches a live balance sitting past them.
     */
    private suspend fun watchSet(): List<Pair<WatchAddr, JSONArray>> {
        val now = System.currentTimeMillis()
        cachedWatchSet?.let { if (now - watchSetAt < 10_000) return it }
        val (nr, nc) = counters()
        val out = ArrayList<Pair<WatchAddr, JSONArray>>()
        for ((branch, from) in listOf(0 to nr, 1 to nc)) {
            var next = from
            var end = from + GAP
            while (next < end && next < WATCH_SET_HARD_CAP) {
                val derived = view.addressAt(branch.toUInt(), next.toUInt())
                val a = WatchAddr(derived.address, derived.scriptPubkeyHex, branch.toUInt(), next.toUInt())
                // A persistent failure here (upstream still 429ing after get()'s own
                // retries) must not blow up the whole scan — stop extending *this*
                // branch's window but keep every address already gathered, on this
                // branch and the other. Silently treating it as "unused" would be
                // wrong (we don't know), but wiping out an otherwise-good partial
                // result is worse: it's exactly what turned "some data" into "no
                // balance or history at all" for a deep wallet hitting a transient
                // rate limit mid-walk.
                val txs = try {
                    JSONArray(get("/address/${a.address}/txs"))
                } catch (e: Exception) {
                    break
                }
                val used = txs.length() > 0
                noteUsed(a, used)
                out += a to txs
                if (used) end = next + 1 + GAP
                next += 1
            }
        }
        cachedWatchSet = out
        watchSetAt = now
        return out
    }

    /** A wallet scan is dozens of these back to back, and public explorers (the
     *  no-token fallback especially — straight to mempool.space/mempool.guide,
     *  no pacing at all) throw an occasional `429`/`5xx` under that fan-out.
     *  Retry those a couple of times with backoff before giving up, same shape
     *  as fortis-edge's own retry against its upstream — so a transient blip
     *  resolves here instead of surfacing as a failed probe to [watchSet] /
     *  [scan], which would otherwise cut a deep scan short for no real reason. */
    private suspend fun get(path: String): String = withContext(Dispatchers.IO) {
        var lastErr: Exception? = null
        for (attempt in 0..2) {
            if (attempt > 0) delay(200L * attempt)
            try {
                val (code, body) = send { Request.Builder().url(base + path) }.use { r ->
                    r.code to r.body?.string().orEmpty()
                }
                if (code in 200..299) return@withContext body
                if (attempt == 2 || (code != 429 && code !in 500..599)) {
                    throw IllegalStateException("explorer $code on $path")
                }
                lastErr = IllegalStateException("explorer $code on $path")
            } catch (e: java.io.IOException) {
                if (attempt == 2) throw e
                lastErr = e
            }
        }
        throw lastErr!!
    }

    private suspend fun tip(): ULong = get("/blocks/tip/height").trim().toULong()

    private var cachedUtxos: List<WalletUtxo>? = null
    private var cachedAt = 0L

    /** Highest receive-branch index seen with any on-chain activity (a UTXO or a
     *  tx). Grows as [scan] / [history] walk the watch set; drives
     *  [firstUnusedReceive]. -1 until the first scan. */
    @Volatile
    private var usedReceiveMax = -1

    private fun noteUsed(a: WatchAddr, active: Boolean) {
        if (active && a.branch == 0u && a.index.toInt() > usedReceiveMax) usedReceiveMax = a.index.toInt()
    }

    override suspend fun firstUnusedReceive(floor: Int): Int = maxOf(floor, usedReceiveMax + 1)

    private suspend fun scan(force: Boolean = false): List<WalletUtxo> {
        val now = System.currentTimeMillis()
        if (!force && cachedUtxos != null && now - cachedAt < 10_000) return cachedUtxos!!
        val t = tip()
        val out = ArrayList<WalletUtxo>()
        for ((a, _) in watchSet()) {
            // Same reasoning as watchSet()'s break above: one address that still
            // fails after get()'s retries shouldn't cost the balance of every other
            // address already known. Skip just this one.
            val arr = try {
                JSONArray(get("/address/${a.address}/utxo"))
            } catch (e: Exception) {
                continue
            }
            for (i in 0 until arr.length()) {
                val u = arr.getJSONObject(i)
                val st = u.optJSONObject("status")
                val confirmed = st?.optBoolean("confirmed") == true
                val h = st?.optLong("block_height") ?: 0L
                val conf = if (confirmed) maxOf(1L, t.toLong() - h + 1) else 0L
                out += WalletUtxo(
                    txid = u.getString("txid"),
                    vout = u.getInt("vout").toUInt(),
                    valueSat = u.getLong("value").toULong(),
                    scriptPubkeyHex = a.spk,
                    confirmations = conf.toUInt(),
                    derivationIndex = a.index,
                    isChange = a.branch == 1u,
                )
            }
        }
        cachedUtxos = out
        cachedAt = now
        return out
    }

    private suspend fun loadPricing() {
        val url = pricingUrl ?: return
        if (pricing != null) return
        pricing = runCatching {
            withContext(Dispatchers.IO) {
                http.newCall(Request.Builder().url(url).build()).execute().use { r ->
                    if (!r.isSuccessful) return@use null
                    val o = JSONObject(r.body?.string().orEmpty())
                    ServicePricing(o.getString("address"), o.getInt("bps"), o.getLong("floor_sat"), o.getLong("cap_sat"))
                }
            }
        }.getOrNull()
    }

    private var lastPrewarm = 0L

    /** Cheap, no-network guess at the watch set — the plain `[from, from+GAP)`
     *  window, same shape [watchSet] used before it learned to expand. Good
     *  enough to warm the edge's cache for the common case (a wallet whose
     *  real gap doesn't exceed [GAP], which is every wallet the app itself has
     *  been managing) without paying [watchSet]'s own per-address probing —
     *  that would defeat the point of prewarming before the real scan. A
     *  freshly imported deep-history xpub just gets a smaller head start;
     *  [scan]/[history] still find everything via [watchSet]'s real walk. */
    private fun guessWatchAddresses(): List<String> {
        val (nr, nc) = counters()
        val out = ArrayList<String>()
        for ((branch, from) in listOf(0 to nr, 1 to nc)) {
            for (i in from until from + GAP) out += view.addressAt(branch.toUInt(), i.toUInt()).address
        }
        return out
    }

    /** One POST of a watch-set guess to `{base}/prewarm`; the edge fills its
     *  address cache from a batch source so [scan]/[history] hit it. Throttled
     *  to sit just inside the edge's cache TTL. Best-effort — a failure just
     *  means the scan falls back to per-address fetches. */
    override suspend fun prewarm() {
        if (!bulkPrewarm) return
        val now = System.currentTimeMillis()
        if (now - lastPrewarm < 45_000) return
        val body = JSONArray(guessWatchAddresses()).toString()
        val ok = runCatching {
            withContext(Dispatchers.IO) {
                send {
                    Request.Builder().url("$base/prewarm")
                        .post(body.toRequestBody("application/json".toMediaTypeOrNull()))
                }.use { it.isSuccessful }
            }
        }.getOrDefault(false)
        if (ok) lastPrewarm = now
    }

    override suspend fun status(): ChainStatus {
        val t = tip()
        loadPricing()
        return ChainStatus(
            blocks = t, synced = true, via = label, chain = "explorer", subversion = "esplora",
            pricing = pricing,
        )
    }

    override suspend fun balances(): Balances {
        val u = scan()
        return Balances(
            confirmedSat = u.filter { it.confirmations >= 1u }.sumOf { it.valueSat.toLong() },
            pendingSat = u.filter { it.confirmations < 1u }.sumOf { it.valueSat.toLong() },
        )
    }

    override suspend fun utxos(minConf: UInt): List<WalletUtxo> =
        scan(force = true).filter { it.confirmations >= minConf }

    private var priceUsd: Double? = null
    private var priceAt = 0L

    /** `GET /v1/prices` → the `USD` field. Cached ~2 min per instance (the edge
     *  also caches 60 s); a missing/unpriced feed just returns null. */
    override suspend fun price(): Double? {
        val now = System.currentTimeMillis()
        if (priceUsd != null && now - priceAt < 120_000) return priceUsd
        val v = runCatching { JSONObject(get("/v1/prices")).optDouble("USD") }
            .getOrNull()
            ?.takeIf { it.isFinite() && it > 0.0 }
        if (v != null) { priceUsd = v; priceAt = now }
        return v ?: priceUsd
    }

    override suspend fun feerateSatVb(confTarget: Int): ULong = try {
        val f = JSONObject(get("/v1/fees/recommended"))
        val pick = when {
            confTarget <= 1 -> f.optDouble("fastestFee")
            confTarget <= 6 -> f.optDouble("halfHourFee")
            else -> f.optDouble("economyFee", f.optDouble("hourFee"))
        }
        maxOf(1L, Math.round(pick)).toULong()
    } catch (e: Exception) { 1uL }

    override suspend fun history(count: Int): List<HistoryEntry> {
        val set = watchSet()
        val mine = set.map { (a, _) -> a.address }.toHashSet()
        val tipH = tip().toLong()
        val seen = LinkedHashMap<String, HistoryEntry>()
        for ((a, arr) in set) {
            for (i in 0 until arr.length()) {
                val tx = arr.getJSONObject(i)
                val id = tx.getString("txid")
                if (seen.containsKey(id)) continue
                var inOurs = 0L; var outOurs = 0L
                tx.getJSONArray("vin").let { vin ->
                    for (j in 0 until vin.length()) {
                        val po = vin.getJSONObject(j).optJSONObject("prevout") ?: continue
                        if (po.optString("scriptpubkey_address") in mine) inOurs += po.optLong("value")
                    }
                }
                tx.getJSONArray("vout").let { vout ->
                    for (j in 0 until vout.length()) {
                        val o = vout.getJSONObject(j)
                        if (o.optString("scriptpubkey_address") in mine) outOurs += o.optLong("value")
                    }
                }
                val delta = outOurs - inOurs
                val send = delta < 0
                val fee = tx.optLong("fee")
                val stTx = tx.optJSONObject("status")
                val confirmed = stTx?.optBoolean("confirmed") == true
                seen[id] = HistoryEntry(
                    txid = id,
                    send = send,
                    amountSat = if (send) delta + fee else delta,
                    feeSat = if (send) fee else 0L,
                    confirmations = if (confirmed) maxOf(1L, tipH - stTx.optLong("block_height") + 1) else 0L,
                    // `optLong` yields 0 for a missing key (unconfirmed txs carry no
                    // block_time); treat that as "now" so a pending tx sorts to the top.
                    time = stTx?.optLong("block_time")?.takeIf { it > 0L }
                        ?: (System.currentTimeMillis() / 1000),
                )
            }
        }
        return seen.values.sortedByDescending { it.time }.take(count)
    }

    override suspend fun broadcast(rawHex: String): String = withContext(Dispatchers.IO) {
        send {
            Request.Builder().url("$base/tx")
                .post(rawHex.toRequestBody("text/plain".toMediaTypeOrNull()))
        }.use { r ->
            val body = r.body?.string()?.trim().orEmpty()
            check(r.isSuccessful) { body.ifBlank { "explorer rejected the transaction (${r.code})" } }
            cachedUtxos = null
            body
        }
    }
}
