package com.fortis.wallet.data

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.coroutineScope
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

/** Addresses fetched concurrently per batch, in both [EsploraBackend.watchSet]
 *  and [EsploraBackend.scan] — same shape and same value as web's `CHUNK` in
 *  `esplora.js`. Strictly sequential (one request, wait for the full round
 *  trip, then the next) was the default until 2026-09-15: for a wallet with
 *  hundreds of used addresses, that round-trip latency is pure dead time that
 *  doesn't need to be serial — the edge's own pacer, not the client, is what
 *  should govern the real request rate to the upstream. */
private const val CHUNK = 8

/** How deep [EsploraBackend.guessWatchAddresses]'s prewarm guess goes per
 *  branch. 500×2 branches = 1000, matching the edge's own `/btc/prewarm`
 *  address cap exactly — no point guessing wider than the edge will accept. */
private const val PREWARM_DEPTH = 500

/** Absolute backstop on how far a single gap-limit walk will go per branch —
 *  not a limit any real wallet should hit (the walk already stops itself after
 *  [GAP] consecutive never-used addresses); just a bound on worst-case work
 *  against a pathological xpub. */
private const val WATCH_SET_HARD_CAP = 2_000

/** Batched scan (`POST {edge}/scan`): how many addresses per branch the first
 *  request asks about. Deriving addresses is local and free, and the edge
 *  answers hundreds in one round trip, so start wide enough that an ordinary
 *  wallet is one request; a deeper one grows the window (see
 *  [EsploraBackend.batchSnapshot]). */
private const val BATCH_MIN_WINDOW = 100

/** The edge refuses more than this many addresses in one `/scan`. */
private const val BATCH_MAX_ADDRESSES = 1_000

/** Confirmed transactions asked back per `/scan` — what the history view shows. */
private const val SCAN_HISTORY = 50

private val JSON_MEDIA = "application/json".toMediaTypeOrNull()

/** The edge has no `/scan` route (an older backend, or a public explorer). */
private class ScanUnsupported : Exception()

/** Just what [EsploraBackend.history] needs from one `/txs` entry — not the raw
 *  JSON. A real, heavily-automated wallet found live, 2026-09-15, kept several
 *  hundred used addresses (still climbing past 300 when this was caught) with
 *  many-input/many-output transactions; caching the *raw* `JSONArray` per
 *  address (full scriptpubkey/witness hex for every vin and vout) across that
 *  many addresses grew the heap fast enough to crash the app with a genuine
 *  `OutOfMemoryError` mid-scan. Keeping only the txid, fee, confirmation
 *  status, and each vin/vout's (address, value) pair cuts the retained size by
 *  roughly the ratio of "a script hex + witness stack" to "an address string"
 *  — the bulk of what made the raw JSON big was never used for anything past
 *  this point anyway. Top-level (not private to [EsploraBackend]) so
 *  [TxCache] can persist and reload it without a separate row shape. */
class TxSummary(
    val txid: String,
    val fee: Long,
    val confirmed: Boolean,
    val blockHeight: Long,
    val blockTime: Long,
    val vin: List<Pair<String?, Long>>,
    val vout: List<Pair<String?, Long>>,
)

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
    /** "btc" / "xbt" — keys [txCache]'s rows, since the same address string
     *  can carry different confirmed history depending which chain it's on. */
    private val chain: String = "btc",
    /** Permanent local store of confirmed tx history, shared across every
     *  `EsploraBackend` for this wallet (the in-view one and the separate
     *  Home-screen overview instance both benefit from whatever either one
     *  already found). Null → behaves exactly as before caching existed. */
    private val txCache: TxCache? = null,
    /** Ask the backend for the whole wallet in one `POST {base}/scan` instead of
     *  walking addresses one request at a time. True only for the hosted
     *  fortis-edge; a public explorer has no such route. If the edge turns out
     *  not to serve it (an older deployment) this quietly reverts to the
     *  per-address walk. */
    batchScan: Boolean = false,
    // Kept last: callers pass this as a trailing lambda.
    private val refresh: (suspend () -> String)? = null,
) : Backend {
    private val base = baseUrl.trimEnd('/')
    override val label: String = Regex("https?://([^/]+)").find(base)?.groupValues?.get(1) ?: base
    private var pricing: ServicePricing? = null
    private var batchOn = batchScan

    /** A batched scan can legitimately take longer than one address lookup
     *  (the edge does the whole wallet's work in that one call). */
    private val scanHttp by lazy { http.newBuilder().readTimeout(20, java.util.concurrent.TimeUnit.SECONDS).build() }

    /** Execute `build()`, adding the bearer token; on 401 re-register once and retry. */
    private suspend fun send(client: OkHttpClient = http, build: () -> Request.Builder): okhttp3.Response {
        fun withAuth() = build().apply { token?.let { header("Authorization", "Bearer $it") } }.build()
        var resp = client.newCall(withAuth()).execute()
        if (resp.code == 401 && refresh != null) {
            resp.close()
            token = refresh.invoke()
            resp = client.newCall(withAuth()).execute()
        }
        return resp
    }

    private class WatchAddr(val address: String, val spk: String, val branch: UInt, val index: UInt)

    private fun summarize(tx: JSONObject): TxSummary {
        val vin = ArrayList<Pair<String?, Long>>()
        tx.getJSONArray("vin").let { arr ->
            for (j in 0 until arr.length()) {
                val po = arr.getJSONObject(j).optJSONObject("prevout")
                vin += (po?.optString("scriptpubkey_address") to (po?.optLong("value") ?: 0L))
            }
        }
        val vout = ArrayList<Pair<String?, Long>>()
        tx.getJSONArray("vout").let { arr ->
            for (j in 0 until arr.length()) {
                val o = arr.getJSONObject(j)
                vout += (o.optString("scriptpubkey_address") to o.optLong("value"))
            }
        }
        val st = tx.optJSONObject("status")
        return TxSummary(
            txid = tx.getString("txid"),
            fee = tx.optLong("fee"),
            confirmed = st?.optBoolean("confirmed") == true,
            blockHeight = st?.optLong("block_height") ?: 0L,
            blockTime = st?.optLong("block_time") ?: 0L,
            vin = vin,
            vout = vout,
        )
    }

    /** Everything one batched scan found, already reduced to what the rest of
     *  this class needs. */
    private class Snapshot(
        val utxos: List<WalletUtxo>,
        val txs: List<TxSummary>,
        /** Every address the scan asked about — what "is this vin/vout ours" is checked against. */
        val mine: Set<String>,
        val tip: Long,
    )

    private var snapshot: Snapshot? = null
    private var snapshotAt = 0L

    /** Per-branch window ([receive, change]) an earlier scan by this instance
     *  already proved big enough, so a refresh after the first is one request
     *  instead of re-growing the window from [BATCH_MIN_WINDOW]. */
    private val knownEnd = intArrayOf(0, 0)

    /** The whole wallet in as few requests as it takes — normally one.
     *
     *  Derive a window of addresses per branch (local, free), send them in one
     *  `POST {base}/scan`, and get back which are used, their unspent outputs,
     *  and the newest transactions. Only when a used address lands within [GAP]
     *  of a window's end (the standard BIP-44 gap-limit rule) is the window
     *  grown and the *new* addresses asked about, so a deep wallet costs a few
     *  round trips the first time and one after — not a request per address.
     *
     *  Any failure — HTTP error, malformed reply, or the edge reporting
     *  addresses it couldn't check — throws. A balance summed from a scan that
     *  skipped addresses looks exactly like a correct one, so this never
     *  returns a partial answer.
     *
     *  Cached ~10s, like the per-address scan it replaces; [force] (used when
     *  building a payment) always goes to the network, since spend selection
     *  must never run on stale coins. */
    private suspend fun batchSnapshot(force: Boolean): Snapshot {
        val startedAt = System.currentTimeMillis()
        snapshot?.let { if (!force && startedAt - snapshotAt < 10_000) return it }

        val (nextReceive, nextChange) = counters()
        val want = intArrayOf(
            maxOf(BATCH_MIN_WINDOW, nextReceive + GAP, knownEnd[0]),
            maxOf(BATCH_MIN_WINDOW, nextChange + GAP, knownEnd[1]),
        )
        val asked = intArrayOf(0, 0) // addresses [0, asked[b]) of each branch already sent
        val addrs = HashMap<String, WatchAddr>()
        val used = HashSet<String>()
        val unspent = ArrayList<JSONObject>()
        val txs = LinkedHashMap<String, TxSummary>()
        var tip = 0L

        repeat(16) { // far more rounds than any real wallet needs; just a runaway backstop
            val fresh = withContext(Dispatchers.Default) {
                (0..1).flatMap { b ->
                    val end = minOf(want[b], WATCH_SET_HARD_CAP)
                    val from = asked[b]
                    asked[b] = maxOf(from, end)
                    (from until end).map { i ->
                        val d = view.addressAt(b.toUInt(), i.toUInt())
                        WatchAddr(d.address, d.scriptPubkeyHex, b.toUInt(), i.toUInt())
                    }
                }
            }
            if (fresh.isEmpty()) return@repeat
            fresh.forEach { addrs[it.address] = it }

            for (part in fresh.chunked(BATCH_MAX_ADDRESSES)) {
                val r = postScan(part.map { it.address })
                tip = maxOf(tip, r.getLong("tip"))
                r.getJSONArray("used").let { a -> for (i in 0 until a.length()) used += a.getString(i) }
                r.getJSONArray("utxos").let { a -> for (i in 0 until a.length()) unspent += a.getJSONObject(i) }
                r.getJSONArray("txs").let { a ->
                    for (i in 0 until a.length()) {
                        val t = summarize(a.getJSONObject(i))
                        txs.putIfAbsent(t.txid, t)
                    }
                }
            }

            // Grow a branch's window when a used address reaches into its last GAP.
            for (b in 0..1) {
                val maxUsed = used.mapNotNull { addrs[it] }
                    .filter { it.branch == b.toUInt() }
                    .maxOfOrNull { it.index.toInt() } ?: -1
                val needed = maxUsed + 1 + GAP
                if (needed > want[b]) want[b] = maxOf(needed, minOf(want[b] * 2, WATCH_SET_HARD_CAP))
            }
        }
        knownEnd[0] = asked[0]; knownEnd[1] = asked[1]

        for (a in used) addrs[a]?.let { noteUsed(it, true) }
        val coins = unspent.map { u ->
            val a = addrs[u.getString("address")]
                ?: throw java.io.IOException("scan returned a coin for an address that wasn't asked about")
            val st = u.optJSONObject("status")
            val confirmed = st?.optBoolean("confirmed") == true
            val h = st?.optLong("block_height") ?: 0L
            WalletUtxo(
                txid = u.getString("txid"),
                vout = u.getInt("vout").toUInt(),
                valueSat = u.getLong("value").toULong(),
                scriptPubkeyHex = a.spk,
                confirmations = (if (confirmed) maxOf(1L, tip - h + 1) else 0L).toUInt(),
                derivationIndex = a.index,
                isChange = a.branch == 1u,
            )
        }
        return Snapshot(coins, txs.values.toList(), addrs.keys.toHashSet(), tip).also {
            snapshot = it
            snapshotAt = startedAt
        }
    }

    /** One `POST {base}/scan` for `addresses`. Retried on 429/5xx like [get]; a
     *  404/405 means this edge doesn't serve `/scan` at all ([ScanUnsupported]). */
    private suspend fun postScan(addresses: List<String>): JSONObject = withContext(Dispatchers.IO) {
        val body = JSONObject().put("addresses", JSONArray(addresses)).put("history", SCAN_HISTORY).toString()
        var lastErr: Exception? = null
        for (attempt in 0..2) {
            if (attempt > 0) delay(300L * attempt)
            try {
                val (code, text) = send(scanHttp) {
                    Request.Builder().url("$base/scan").post(body.toRequestBody(JSON_MEDIA))
                }.use { r -> r.code to r.body?.string().orEmpty() }
                if (code in 200..299) {
                    val o = JSONObject(text)
                    val failed = o.optJSONArray("failed")
                    if (failed != null && failed.length() > 0) {
                        throw java.io.IOException("scan incomplete: ${failed.length()} addresses could not be checked")
                    }
                    return@withContext o
                }
                if (code == 404 || code == 405) throw ScanUnsupported()
                if (attempt == 2 || (code != 429 && code !in 500..599)) {
                    throw IllegalStateException("scan $code")
                }
                lastErr = IllegalStateException("scan $code")
            } catch (e: java.io.IOException) {
                if (attempt == 2) throw e
                lastErr = e
            }
        }
        throw lastErr!!
    }

    private var cachedWatchSet: List<Pair<WatchAddr, List<TxSummary>>>? = null
    private var watchSetAt = 0L

    /** Best-effort seed for [watchSet] from [txCache]: derive the same
     *  guessed address window [prewarm] uses (cheap, local, no network), bulk
     *  -load whichever of them have persisted confirmed history, and keep
     *  only the ones that hit — an address absent from the cache is simply
     *  unknown yet, not "confirmed unused" (that distinction still needs a
     *  live check, same as any cache miss elsewhere in this class). */
    private suspend fun seedFromCache(): List<Pair<WatchAddr, List<TxSummary>>> {
        val cache = txCache ?: return emptyList()
        // Off the calling dispatcher (viewModelScope defaults to Main): up to
        // 1000 real EC point derivations plus a chunked SQLite read have no
        // business running synchronously on the UI thread, same reasoning as
        // the write-through path a few dozen lines below already gets.
        return withContext(Dispatchers.IO) {
            val guessed = (0..1).flatMap { branch ->
                (0 until PREWARM_DEPTH).map { i ->
                    val d = view.addressAt(branch.toUInt(), i.toUInt())
                    WatchAddr(d.address, d.scriptPubkeyHex, branch.toUInt(), i.toUInt())
                }
            }
            val loaded = cache.load(chain, guessed.map { it.address })
            guessed.mapNotNull { wa -> loaded[wa.address]?.let { wa to it } }
        }
    }

    /** Every address worth checking right now, each paired with its `/txs`
     *  response — summarized, not raw (see [TxSummary]) — so [history] never
     *  re-fetches what this walk already has.
     *
     *  For each branch, walks forward from index 0 in [GAP]-sized batches,
     *  extending the window whenever the batch just checked had *any* address
     *  with transaction history — the standard BIP-44 gap-limit walk —
     *  instead of one fixed-size `[0, GAP)` pass. That fixed pass is wrong for
     *  a wallet with more than ~20 used receive or change addresses: had
     *  everything past index ~20 silently invisible — wrong balance, missing
     *  history, forever (nothing about the bug is self-correcting, since the
     *  window never had a reason to grow).
     *
     *  "Used" here means *ever* appeared in a transaction (`/txs` non-empty),
     *  not "currently has an unspent output" (`/utxo` non-empty) — an address
     *  that received funds and was later fully spent shows an *empty* `/utxo`
     *  response but is still a real, used address. Deciding gap continuation
     *  on `/utxo` alone stops the walk right after a run of spent-through
     *  addresses, before it ever reaches a live balance sitting past them.
     *
     *  Each branch always walks from index 0, never from [counters]' stored
     *  next-index. That counter exists purely to pick which address the UI
     *  offers next for "Receive" — treating it as the scan floor too (as this
     *  used to) drops every address below it from balance *and* history the
     *  moment it advances, silently and permanently: found live on a
     *  deep-history watch-only import, where the very first scan's own
     *  gap-limit auto-advance (see WalletViewModel.refresh()) pushed
     *  next-receive past dozens of addresses that were still holding real,
     *  unspent balance. Nothing stops a later deposit landing on an address
     *  below that counter either, hot wallet or watch-only, so there is no
     *  index this can safely stop rechecking.
     */
    private suspend fun watchSet(): List<Pair<WatchAddr, List<TxSummary>>> = coroutineScope {
        val now = System.currentTimeMillis()
        cachedWatchSet?.let { if (now - watchSetAt < 10_000) return@coroutineScope it }

        // Before ever doing a live walk (this instance's very first call —
        // `cachedWatchSet` is only null until the first resolution, cache-seeded
        // or live), try painting from the permanent local cache instead: lets a
        // cold app start / wallet switch show last-known balance/history with
        // zero network calls. `watchSetAt` is left at 0 (not `now`) so the very
        // next call — the next 20s poll tick — treats this as stale and does a
        // real walk, which both confirms the seed and persists anything new.
        // Confirmed-tx history never changes, but "is there anything new" still
        // needs a live check eventually; this only defers that, never skips it.
        if (cachedWatchSet == null) {
            val seed = seedFromCache()
            if (seed.isNotEmpty()) {
                for ((a, txs) in seed) noteUsed(a, txs.isNotEmpty())
                cachedWatchSet = seed
                return@coroutineScope seed
            }
        }

        val out = ArrayList<Pair<WatchAddr, List<TxSummary>>>()
        var anySuccess = false
        for (branch in 0..1) {
            var next = 0
            var end = GAP
            while (next < end && next < WATCH_SET_HARD_CAP) {
                val batchEnd = minOf(end, next + CHUNK, WATCH_SET_HARD_CAP)
                val batch = (next until batchEnd).map { i ->
                    async {
                        val derived = view.addressAt(branch.toUInt(), i.toUInt())
                        val a = WatchAddr(derived.address, derived.scriptPubkeyHex, branch.toUInt(), i.toUInt())
                        val txs = try {
                            val raw = JSONArray(get("/address/${a.address}/txs"))
                            List(raw.length()) { j -> summarize(raw.getJSONObject(j)) }
                        } catch (e: Exception) {
                            null
                        }
                        Triple(a, i, txs)
                    }
                }.awaitAll()
                for ((a, i, txs) in batch) {
                    if (txs == null) {
                        // A persistent failure here (upstream still failing after get()'s
                        // own retries) must never be treated as "confirmed unused" — doing
                        // so is exactly the bug this whole rewrite exists to fix, just
                        // triggered mid-walk instead of by a stale scan floor. Found live,
                        // 2026-09-15: a spell of Maestro 403s (its metered credit budget
                        // exhausted) used to stop extending here and silently capped a
                        // real ~300-address wallet's walk at index ~19, undercounting its
                        // balance by ~0.8 BTC with no error shown anywhere. Extend the
                        // window defensively instead of shrinking the "consecutive
                        // unused" runway — nothing here is cached on failure, so a later
                        // refresh's fresh `get()` attempt gets another try at this exact
                        // address. `anySuccess` staying false across an entire scan still
                        // throws below, so a genuinely total outage is not silently
                        // swallowed by this.
                        end = maxOf(end, i + 1 + GAP)
                    } else {
                        anySuccess = true
                        val used = txs.isNotEmpty()
                        noteUsed(a, used)
                        out += a to txs
                        if (used) end = maxOf(end, i + 1 + GAP)
                    }
                }
                // Write-through: persist this batch's confirmed-only results
                // (never pending — those can still change or vanish) so the
                // *next* cold start / wallet switch can paint from them
                // instantly via [seedFromCache]. Off the calling dispatcher —
                // this loop otherwise runs on whatever context called
                // [watchSet] (viewModelScope defaults to Main), and disk I/O
                // has no business blocking it.
                if (txCache != null) {
                    withContext(Dispatchers.IO) {
                        for ((a, _, txs) in batch) {
                            // Best-effort, like every other cache write in this
                            // class — an unguarded throw here (disk full, a
                            // corrupted local db) would propagate out of
                            // watchSet() and be misread by refresh()'s
                            // runCatching as the *edge* failing, silently
                            // switching the wallet to the degraded public
                            // fallback even though the live scan succeeded.
                            txs?.let { runCatching { txCache.put(chain, a.address, it.filter { t -> t.confirmed }) } }
                        }
                    }
                }
                next = batchEnd
            }
        }
        // Every single probe failed — this is "the upstream is unreachable right
        // now", not "a freshly imported xpub with no history". Returning an empty
        // list here would look identical to a genuinely empty wallet: balances()
        // sums zero UTXOs and history() finds zero transactions, both perfectly
        // confident results. Throwing instead lets refresh() do what it already
        // does for a hard failure — fall back to the other backend, or leave the
        // last-known-good balance on screen — rather than the wallet quietly
        // reporting a wrong zero as fact. Confirmed live, 2026-09-15: the
        // public-explorer fallback did exactly this after blockstream.info
        // (the hosted edge's own upstream) started 429ing this machine outright.
        if (!anySuccess) throw java.io.IOException("no address probe succeeded — upstream unreachable")
        cachedWatchSet = out
        watchSetAt = now
        out
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

    private suspend fun scan(force: Boolean = false): List<WalletUtxo> = coroutineScope {
        if (batchOn) {
            try {
                return@coroutineScope batchSnapshot(force).utxos
            } catch (e: ScanUnsupported) {
                batchOn = false // this backend doesn't serve /scan — use the per-address walk below
            }
        }
        val now = System.currentTimeMillis()
        if (!force && cachedUtxos != null && now - cachedAt < 10_000) return@coroutineScope cachedUtxos!!
        val t = tip()
        val set = watchSet()
        val out = ArrayList<WalletUtxo>()
        for (batchStart in set.indices step CHUNK) {
            val batch = set.subList(batchStart, minOf(batchStart + CHUNK, set.size)).map { (a, _) ->
                async {
                    // Same reasoning as watchSet()'s failure handling: one address that
                    // still fails after get()'s retries shouldn't cost the balance of
                    // every other address already known. Skip just this one.
                    val arr = try {
                        JSONArray(get("/address/${a.address}/utxo"))
                    } catch (e: Exception) {
                        null
                    }
                    a to arr
                }
            }.awaitAll()
            for ((a, arr) in batch) {
                if (arr == null) continue
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
        }
        cachedUtxos = out
        cachedAt = now
        out
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

    /** Cheap, no-network guess at the watch set: every address from 0 up to
     *  [PREWARM_DEPTH] on each branch, handed to the edge's batch Haskoin
     *  path so the real per-address walk that follows is served from cache
     *  instead of the slow, paced, one-at-a-time route this exists to avoid.
     *
     *  Used to be just `[from, from+GAP)` near the persisted next-index —
     *  cheap, but only ever covered a wallet with a handful of used
     *  addresses. Found live, 2026-09-15: a real watch-only import with
     *  several hundred used addresses barely benefited from prewarm at all,
     *  since the guessed window covered a small fraction of what the walk
     *  actually needed — almost the whole scan still paid the slow path.
     *  Deriving addresses is a local, no-network computation regardless of
     *  how many, so guessing wide costs nothing extra on this side; the
     *  edge's own `/btc/prewarm` cap and chunked batch calls bound the real
     *  cost of a guess this size. */
    private fun guessWatchAddresses(): List<String> {
        val out = ArrayList<String>()
        for (branch in 0..1) {
            for (i in 0 until PREWARM_DEPTH) out += view.addressAt(branch.toUInt(), i.toUInt()).address
        }
        return out
    }

    /** One POST of a watch-set guess to `{base}/prewarm`; the edge fills its
     *  address cache from a batch source so [scan]/[history] hit it. Throttled
     *  to sit just inside the edge's cache TTL. Best-effort — a failure just
     *  means the scan falls back to per-address fetches. */
    override suspend fun prewarm() {
        if (!bulkPrewarm || batchOn) return // a batched scan makes the per-address prewarm pointless
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
        if (batchOn) {
            try {
                val s = batchSnapshot(false)
                return historyFrom(s.txs, s.mine, s.tip, count)
            } catch (e: ScanUnsupported) {
                batchOn = false // fall through to the per-address walk
            }
        }
        val set = watchSet()
        val mine = set.map { (a, _) -> a.address }.toHashSet()
        val tipH = tip().toLong()
        return historyFrom(set.flatMap { (_, txs) -> txs }, mine, tipH, count)
    }

    /** `txs` may repeat a transaction (one that touches several of the wallet's
     *  addresses); each is counted once. */
    private fun historyFrom(txs: Iterable<TxSummary>, mine: Set<String>, tipH: Long, count: Int): List<HistoryEntry> {
        val seen = LinkedHashMap<String, HistoryEntry>()
        for (tx in txs) {
            if (seen.containsKey(tx.txid)) continue
            var inOurs = 0L; var outOurs = 0L
            for ((addr, value) in tx.vin) if (addr != null && addr in mine) inOurs += value
            for ((addr, value) in tx.vout) if (addr != null && addr in mine) outOurs += value
            val delta = outOurs - inOurs
            val send = delta < 0
            seen[tx.txid] = HistoryEntry(
                txid = tx.txid,
                send = send,
                amountSat = if (send) delta + tx.fee else delta,
                feeSat = if (send) tx.fee else 0L,
                confirmations = if (tx.confirmed) maxOf(1L, tipH - tx.blockHeight + 1) else 0L,
                // 0 block_time (unconfirmed txs carry none) sorts a pending tx as "now".
                time = tx.blockTime.takeIf { it > 0L } ?: (System.currentTimeMillis() / 1000),
            )
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
            snapshot = null
            body
        }
    }
}
