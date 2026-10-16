package com.fortis.wallet

import android.app.Application
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.fortis.wallet.data.*
import com.fortis.wallet.swap.SwapService
import com.fortis.wallet.swap.indexUsed
import com.fortis.wallet.swap.withFreshIndex
import com.fortis.wallet.swap.recoveredRecord
import com.fortis.wallet.swap.recoveryWallets
import com.fortis.wallet.swap.scanTags
import com.fortis.wallet.ui.fmtCoin
import com.fortis.wallet.ui.screens.amountError
import com.fortis.wallet.ui.screens.amountLimit
import com.fortis.wallet.ui.screens.btcLeg
import com.fortis.wallet.wallet.WalletSession
import com.fortis.wallet.wallet.newMnemonic
import com.fortis.wallet.wallet.sealSeed
import com.fortis.wallet.wallet.unsealSeed
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import okhttp3.Dispatcher
import okhttp3.OkHttpClient
import uniffi.wallet_ffi.FundingPlan
import uniffi.wallet_ffi.ImportedAccount
import java.net.Proxy
import java.util.UUID
import java.util.concurrent.TimeUnit

enum class Phase { Loading, AppLock, Onboard, Gen, Create, Restore, Watch, HwConnect, Shell, RevealSeed }

/** The three top-level destinations inside [Phase.Shell]'s nav bar. */
enum class NavTab { Home, Wallet, Swaps, Settings }

/** How many wallets one install can hold. */
const val MAX_WALLETS = 10
/** How many places Back remembers. */
private const val MAX_BACK = 30

/** Longest a wallet name may be. */
const val MAX_WALLET_NAME = 30

/** A wallet name as stored: trimmed, at most [MAX_WALLET_NAME], "Wallet" when blank. */
fun cleanWalletName(name: String): String = name.trim().take(MAX_WALLET_NAME).ifBlank { "Wallet" }

/** True if another wallet on [chain] already has [name] (trimmed, ignoring case).
 *  The same name on the other chain is fine: "add to the other chain" keeps it. */
fun walletNameTaken(wallets: List<WalletConfig>, name: String, chain: String, exceptId: String? = null): Boolean {
    val n = cleanWalletName(name)
    return wallets.any { it.id != exceptId && it.chain == chain && it.name.equals(n, ignoreCase = true) }
}

/** The one backend the mobile app talks to. Not user-configurable, not shown. */
const val HOSTED_EDGE = "https://api.fortistechlabs.com"

/** Public Esplora fallbacks, used when [HOSTED_EDGE] is unreachable.
 *  mempool.space itself is unreachable from at least this project's home
 *  network (DNS resolves, TCP times out, confirmed repeatedly 2026-09-28);
 *  mempool.emzy.de runs the same open-source backend and is confirmed
 *  reliable from here — see fortis-edge's own --btc-upstream for the
 *  server-side twin of this same fix. */
const val PUBLIC_BTC_ESPLORA = "https://mempool.emzy.de/api"
const val PUBLIC_XBT_ESPLORA = "https://mempool.guide/api"

data class PlanPreview(
    val plan: FundingPlan, val feerate: ULong, val to: String,
    val sweep: Boolean, val replayProtected: Boolean = false,
)

/** The result of setting up the app lock, produced by the UI for the first wallet. */
data class LockSetup(val mode: String, val secret: String, val appWrapped: String?)

/** The swap screens shown over the Shell (Home → Swap). */
sealed interface SwapRoute {
    data object List : SwapRoute
    data object New : SwapRoute
    data object Take : SwapRoute
    /** Taking all or part of a listed offer. */
    data class TakeListed(val entry: com.fortis.wallet.data.BookEntry) : SwapRoute
    data class Detail(val id: String) : SwapRoute
}

/** A taken share code's terms, shown before Accept. [reference] is this phone's
 *  own NeoxEX reference (`null` if unknown); [problem] why the terms fail our own check. */
data class TakePreview(
    val taken: Taken,
    val termsJson: String,
    val role: String,
    val reference: NeoxEx.Reference?,
    val problem: String?,
) {
    private val terms get() = org.json.JSONObject(termsJson)

    /** The initiator sends XBT; the participant sends BTC. */
    val sendChain: String get() = if (role == "initiator") "xbt" else "btc"
    val sendSat: Long get() = terms.getLong(if (sendChain == "xbt") "xbt_sat" else "btc_sat")
    val receiveSat: Long get() = terms.getLong(if (sendChain == "xbt") "btc_sat" else "xbt_sat")
    val priceSatPerXbt: Long get() = terms.getLong("price_sat_per_xbt")
    val acceptBy: Long get() = terms.getLong("accept_by")
}

/** A new swap's keys: its index on [seed] (the master fingerprint), the
 *  recovery tag the edge files it under, and the two wrapped secrets. */
private data class SwapKeys(
    val index: Int, val seed: String, val recoveryTag: String,
    val wrappedXbt: String, val wrappedBtc: String,
)

class WalletViewModel(app: Application) : AndroidViewModel(app) {
    private val store = Store(app)
    private val txCache = TxCache(app)
    private fun str(id: Int, vararg args: Any) = getApplication<Application>().getString(id, *args)
    private fun plural(id: Int, n: Int) = getApplication<Application>().resources.getQuantityString(id, n, n)
    private val http = OkHttpClient.Builder()
        .proxy(Proxy.NO_PROXY) // ignore any Wi-Fi/Studio proxy — local hosts must be direct
        // A watch-only scan makes dozens to hundreds of these; EsploraBackend's
        // get() retries each one up to 3 times on top of whatever this client
        // itself waits out. At the old 15s/30s, one genuinely dead address cost
        // up to ~135s before get() gave up on it — found live, 2026-09-15, when
        // a scan against a degraded upstream took many minutes because most of
        // that time was spent waiting out timeouts one at a time, not doing
        // useful work. 6s/8s is still generous for a JSON REST call and cuts
        // that same worst case to well under a minute.
        .connectTimeout(6, TimeUnit.SECONDS)
        .readTimeout(8, TimeUnit.SECONDS)
        // OkHttp's own default `Dispatcher` caps concurrent requests at 5 per
        // host — silently below `EsploraBackend`'s `CHUNK` (8), which both
        // `watchSet()`'s gap-limit walk and `scan()`'s balance fetch batch by.
        // Every "batch of 8" was really only running 5 at a time, with 3
        // queued behind them, on top of the live-network cost `scan()`
        // already can't avoid (its UTXO fetch is deliberately never cached
        // beyond 10s, for spend safety — see `scan()`'s doc comment). Found
        // live, 2026-09-18: a deep watch-only wallet (100+ used addresses)
        // felt slow to reopen even well past any warm-cache window. Raised
        // past `CHUNK` so a full batch actually dispatches at once; all
        // traffic here goes to one host at a time (the hosted edge, or a
        // public-explorer fallback), so `maxRequestsPerHost` is the limit
        // that actually matters, not the (already-sufficient) global cap.
        .dispatcher(Dispatcher().apply { maxRequestsPerHost = 16 })
        .build()

    var phase by mutableStateOf(Phase.Loading); private set
    var nav by mutableStateOf(NavTab.Home); private set

    // --- wallets ---
    var wallets by mutableStateOf<List<WalletConfig>>(emptyList()); private set
    var selectedId by mutableStateOf<String?>(null); private set
    private val sessions = mutableStateMapOf<String, WalletSession>()

    // --- app lock ---
    var lockMode by mutableStateOf<String?>(null); private set
    private var appWrapped: String? = null
    /** The seal secret for every wallet, held only while the app is unlocked. */
    private var appSecret by mutableStateOf<String?>(null)
    val locked: Boolean get() = appSecret == null && wallets.any { !it.watchOnly }
    val appWrappedSecret: String? get() = appWrapped

    /** Where the biometric-mode unlock key actually lives (StrongBox / TEE /
     *  software), or null in password mode. For the Settings security note. */
    fun keySecurity(): com.fortis.wallet.data.SeedKeystore.KeySecurity? =
        if (lockMode == com.fortis.wallet.data.LOCK_BIOMETRIC) com.fortis.wallet.data.SeedKeystore.keySecurity() else null

    /** The wallet currently in view. */
    val config: WalletConfig? get() = wallets.firstOrNull { it.id == selectedId }
    val session: WalletSession? get() = selectedId?.let { sessions[it] }
    fun isUnlocked(id: String) = sessions.containsKey(id)
    /** The account xpub for a wallet, if its session is loaded (it is once the
     *  Home / Settings balance scan has run). */
    fun accountKey(id: String): String? = sessions[id]?.xpub

    // --- the selected wallet's backends + view state ---
    var backend: Backend? = null; private set
    private var fallback: Backend? = null
    private var usingFallback by mutableStateOf(false)

    private fun active(): Backend = (if (usingFallback) fallback else backend) ?: backend
        ?: error("no backend")

    var draftMnemonic by mutableStateOf<String?>(null); private set
    var status by mutableStateOf<ChainStatus?>(null); private set
    var balances by mutableStateOf<Balances?>(null); private set

    /** True for the duration of an in-flight [refresh] call — lets the UI show
     *  a "Refreshing…" state distinct from the initial, never-yet-loaded
     *  "Connecting…" one, so tapping refresh visibly does something instead
     *  of looking like a no-op until it either finishes or fails. */
    var refreshing by mutableStateOf(false); private set

    /** The exception message from the most recent failed [refresh] attempt
     *  (both the primary edge and the fallback failing), or null once a
     *  refresh has succeeded since. Distinguishes "never loaded yet" from
     *  "just tried and failed" in the UI, and gives a real, specific string
     *  to show instead of a silent, indefinite "Connecting…" — found live,
     *  2026-09-28: a malformed server response threw deep inside a batch
     *  scan parse, `runCatching` swallowed it with zero log output, and the
     *  wallet sat on "connecting…" forever with nothing anywhere — not even
     *  logcat — hinting at what had actually gone wrong. */
    var lastRefreshError by mutableStateOf<String?>(null); private set

    /** Price per whole coin by currency code, keyed by chain ("btc" / "xbt").
     *  Shared by every wallet on that chain; absent = no fiat value shown. */
    var coinPrices by mutableStateOf<Map<String, Map<String, Double>>>(emptyMap()); private set

    private val fiatPrefs = FiatPrefs(app)

    /** The currency fiat values are shown in: the one chosen in Settings, else
     *  the device region's (see [defaultFiat]). */
    var fiat by mutableStateOf(fiatPrefs.chosen() ?: defaultFiat()); private set

    /** Whether [fiat] was chosen (false = following the device region). */
    var fiatChosen by mutableStateOf(fiatPrefs.chosen() != null); private set

    /** Choose the display currency; null = follow the device region. */
    fun chooseFiat(code: String?) {
        fiatPrefs.set(code)
        fiat = code ?: defaultFiat()
        fiatChosen = code != null
    }

    /** Approximate value of `sat` on `chain` in [fiat], formatted, or null if
     *  there's no price in that currency yet. Never another currency's. */
    fun fiatText(sat: Long, chain: String): String? =
        coinPrices[chain]?.get(fiat)?.let { fmtFiat(it * (sat / 100_000_000.0), fiat) }
    var history by mutableStateOf<List<HistoryEntry>>(emptyList()); private set
    var feerates by mutableStateOf<Map<Int, Long>>(emptyMap()); private set
    var pending by mutableStateOf<PlanPreview?>(null); private set
    var lastSentTxid by mutableStateOf<String?>(null); private set

    /** Bumped by every payment sent: the Send form starts over. */
    var sentCount by mutableStateOf(0); private set
    var error by mutableStateOf<String?>(null)

    /** Payments sent from this phone the server hasn't listed yet (see [LocalSend]).
     *  Written from the main thread and the background balance scan. */
    private val localLock = Any()
    @Volatile private var localSends: List<LocalSend> = emptyList()

    /** The in-view wallet's balance and history as the server last gave them,
     *  before [localSends] are applied. */
    private var serverBalances: Balances? = null
    private var serverHistory: List<HistoryEntry> = emptyList()

    /** [id]'s local sends still unlisted — pruning those [listed] now holds. */
    private fun unseenSends(id: String, listed: List<HistoryEntry>? = null): List<LocalSend> = synchronized(localLock) {
        localSends = pruneLocalSends(localSends, id, listed?.mapTo(HashSet()) { it.txid }, System.currentTimeMillis())
        localSends.filter { it.walletId == id }
    }

    private fun noteLocalSend(s: LocalSend) = synchronized(localLock) {
        localSends = localSends.filterNot { it.walletId == s.walletId && it.txid == s.txid } + s
    }

    /** [id]'s coins spent by payments the server hasn't listed yet, as "txid:vout". */
    private fun spentLocally(id: String): Set<String> = unseenSends(id).flatMapTo(HashSet()) { it.spent }

    /** The local send for a payment [plan] just broadcast as [txid] to [to]. */
    private fun localSend(c: WalletConfig, s: WalletSession, plan: FundingPlan, txid: String, to: String): LocalSend {
        val inSat = plan.selected.sumOf { it.valueSat.toLong() }
        val change = plan.changeSat?.toLong() ?: 0L
        val out = inSat - plan.feeSat.toLong() - (plan.serviceFeeSat?.toLong() ?: 0L) - change
        return LocalSend(
            walletId = c.id, txid = txid,
            spent = plan.selected.mapTo(HashSet()) { "${it.txid}:${it.vout}" },
            inSat = inSat,
            backSat = change + if (ownsAddress(s, c, to)) out else 0L,
            feeSat = plan.feeSat.toLong(),
            at = System.currentTimeMillis(),
        )
    }

    /** Whether [addr] is one of this wallet's addresses, within the gap past its counters. */
    private fun ownsAddress(s: WalletSession, c: WalletConfig, addr: String): Boolean =
        (0..c.nextReceive + 20).any { runCatching { s.view.addressAt(0u, it.toUInt()).address }.getOrNull().equals(addr, ignoreCase = true) } ||
            (0..c.nextChange + 20).any { runCatching { s.view.addressAt(1u, it.toUInt()).address }.getOrNull().equals(addr, ignoreCase = true) }

    /** Show the in-view wallet's server balance and history with its local sends applied. */
    private fun showWithLocalSends(id: String, listed: List<HistoryEntry>? = null, toOverview: Boolean = true) {
        val unseen = unseenSends(id, listed)
        val raw = serverBalances
        if (raw == null) {
            history = withLocalSends(Balances(0, 0), serverHistory, unseen).second
            return
        }
        val (b, h) = withLocalSends(raw, serverHistory, unseen)
        history = h
        balances = b
        if (toOverview && !raw.serverDegraded) setWalletBalance(id, b)
    }

    /** The process's one swap engine and store live here (a service locator). */
    private val fortis = app as FortisApplication

    /** This ViewModel's funder, handed to the Application-level engine while it lives. */
    private val funder = swapFunder(fortis.swapStore)

    init {
        fortis.unlockedFunder = funder
        viewModelScope.launch {
            val s = store.load()
            wallets = s.wallets
            selectedId = s.selectedId
            lockMode = s.lockMode
            appWrapped = s.appWrapped
            resolvePhase()
        }
    }

    private fun resolvePhase() {
        phase = when {
            wallets.isEmpty() -> Phase.Onboard
            wallets.all { it.watchOnly } -> Phase.Shell // no lock exists yet to check
            appSecret == null -> Phase.AppLock
            else -> Phase.Shell
        }
        if (phase == Phase.Shell) enterTab()
    }

    /** Prepare state for the nav tab currently in view. The Wallet tab's
     *  composable drives the actual refresh loop. */
    private fun enterTab() {
        if (nav == NavTab.Wallet) {
            val id = selectedId
            if (id == null || ensureSession(id) == null) { nav = NavTab.Home; return }
            ensureBackend()
        }
    }

    /** Unseal a wallet with the in-memory app secret (no extra prompt).
     *  `quiet` suppresses the error banner (used by the bulk balance scan). */
    private fun ensureSession(id: String, quiet: Boolean = false): WalletSession? {
        sessions[id]?.let { return it }
        val c = wallets.firstOrNull { it.id == id } ?: return null
        return try {
            val session = if (c.watchOnly) {
                WalletSession.watchOnly(c.chain, c.network, c.xpub ?: return null)
            } else {
                val secret = appSecret ?: return null
                val (mnemonic, passphrase) = unsealSeed(c.sealed!!, c.salt!!, secret)
                WalletSession.signing(c.chain, c.network, mnemonic, passphrase)
            }
            session.also {
                it.setIndices(c.nextReceive, c.nextChange)
                sessions[id] = it
            }
        } catch (e: Exception) {
            if (!quiet) error = e.message ?: str(R.string.error_open_wallet)
            null
        }
    }

    /** Confirmed and pending balances (sat) per wallet id, for the Home / Settings
     *  overviews. Absent = not yet known. */
    private var overview by mutableStateOf(OverviewBalances())
    val walletBalances: Map<String, Long> get() = overview.confirmed
    val walletPending: Map<String, Long> get() = overview.pending
    private val overviewBackends = java.util.concurrent.ConcurrentHashMap<String, Backend>()
    private val balanceLock = Any()
    private var scanningAll = false

    /** Record one wallet's balance — serialised, since the bulk scan runs off the
     *  main thread while [refresh] writes from it. */
    private fun setWalletBalance(id: String, b: Balances) = synchronized(balanceLock) {
        overview = overview.with(id, b)
    }

    private fun setCoinPrices(chain: String, prices: Map<String, Double>) = synchronized(balanceLock) {
        coinPrices = coinPrices + (chain to prices)
    }

    /** Scan every wallet's balance in the background; failures leave that
     *  wallet's entry absent. Off the main thread — unsealing runs Argon2id. */
    fun refreshAllBalances() {
        if (appSecret == null || scanningAll) return
        scanningAll = true
        viewModelScope.launch(Dispatchers.Default) {
            try {
                val priced = HashSet<String>()
                for (w in wallets.toList()) {
                    val s = ensureSession(w.id, quiet = true) ?: continue
                    val id = w.id
                    val b = overviewBackend(w, s)
                    runCatching { b.prewarm() }
                    if (unseenSends(id).isEmpty()) {
                        runCatching { b.balances() }.getOrNull()?.let { setWalletBalance(id, it) }
                    } else {
                        // A payment sent from here the server may not list yet: its history decides.
                        runCatching { b.balancesAndHistory(50) }.getOrNull()?.let { (bal, h) ->
                            setWalletBalance(id, withLocalSends(bal, h, unseenSends(id, h)).first)
                        }
                    }
                    // one price lookup per chain, reused across its wallets
                    if (priced.add(w.chain)) {
                        runCatching { b.prices() }.getOrNull()?.let { setCoinPrices(w.chain, it) }
                    }
                }
            } finally {
                scanningAll = false
            }
        }
    }

    /** The hosted edge as an Esplora backend for any wallet (not just the one in
     *  view), shared by the balance scan and the swap funder. */
    private fun overviewBackend(w: WalletConfig, s: WalletSession): Backend {
        val id = w.id
        return overviewBackends.getOrPut(id) {
            EsploraBackend(
                http, "$HOSTED_EDGE/${w.chain}", s.view,
                { (wallets.firstOrNull { it.id == id } ?: w).let { c -> c.nextReceive to c.nextChange } },
                w.backendToken ?: "",
                "$HOSTED_EDGE/pricing",
                bulkPrewarm = w.chain == "btc",
                chain = w.chain,
                txCache = txCache,
                batchScan = true,
            ) {
                val fresh = edgeRegister(http, HOSTED_EDGE)
                updateConfig(id) { it.copy(backendToken = fresh) }
                fresh
            }
        }
    }

    /**
     * Funds a swap's HTLC from its from-wallet, over the ordinary send path:
     * `plan_payment` to the HTLC address plus the commission (0.5%, at least 400 sat)
     * (`ServiceFee`, to the `/pricing` address — which the engine already checked
     * is the terms' fee address), then `sign_funding_tx`, then broadcast. The
     * txid (computed from the signed hex) and the signed hex are saved as
     * `pendingFundingTxid` / `pendingFundingHex` before the broadcast, so a
     * process death or lost answer in between can't fund twice: the engine
     * re-sends that same tx. `null` while the app is locked (the engine then
     * notifies NEED_FUNDING).
     */
    fun swapFunder(swaps: SwapStore): com.fortis.wallet.swap.Funder = com.fortis.wallet.swap.Funder { r, htlc ->
        if (appSecret == null) return@Funder null
        val w = wallets.firstOrNull { it.id == r.fromWalletId } ?: error("the swap's wallet was removed")
        require(w.chain == r.sendChain) { "the swap's wallet is on the wrong chain" }
        require(!w.watchOnly) { "a watch-only wallet can't fund a swap" }
        val s = withContext(Dispatchers.Default) { ensureSession(w.id, quiet = true) } ?: return@Funder null
        val b = overviewBackend(w, s)
        val pricing = b.status().pricing ?: error("the edge advertises no commission address")
        val terms = org.json.JSONObject(r.termsJson ?: error("no terms yet"))
        require(pricing.address == terms.getString("fee_address")) { "the commission address changed" }
        val fee = uniffi.wallet_ffi.ServiceFee(pricing.address, 50u, com.fortis.wallet.ui.screens.SWAP_FEE_FLOOR_SAT.toULong(), 0uL)
        // Capped like every swap spend: a hostile edge's estimate can't make funding a gift to miners.
        val feerate = b.feerateSatVb(6).coerceIn(1uL, uniffi.wallet_ffi.maxSwapFeerateSatVb())
        val spent = spentLocally(w.id)
        val utxos = b.utxos(1u).filterNot { "${it.txid}:${it.vout}" in spent }
        require(utxos.isNotEmpty()) { str(R.string.error_no_coins) }
        s.setIndices(w.nextReceive, w.nextChange)
        // On BTC, an OP_RETURN the XBT chain can't mine: the funding can't be replayed there.
        val plan = s.view.planPayment(
            utxos, listOf(uniffi.wallet_ffi.PayTo(htlc.address, htlc.amountSat)), feerate, 1u,
            com.fortis.wallet.swap.swapFundingGuard(w.chain), fee, false,
        )
        val signed = s.sign(plan.txHex, plan.selected)
        val txid = uniffi.wallet_ffi.txidOf(signed)
        // Saved before the broadcast: the engine re-sends this exact tx until it is
        // seen (or definitively rejected) and never plans a second funding meanwhile.
        val rec = swaps.get(r.id) ?: error("the swap record is gone")
        swaps.put(rec.copy(pendingFundingTxid = txid, pendingFundingHex = signed))
        b.broadcast(signed)
        runCatching { noteLocalSend(localSend(w, s, plan, txid, htlc.address)) }
        val idx = s.view.nextIndices()
        updateConfig(w.id) { it.copy(nextChange = maxOf(it.nextChange, idx.nextChange.toInt())) }
        txid
    }

    // --- swaps (P2P BTC ⇄ XBT; the engine itself lives in FortisApplication) ---

    /** The edge's swap quote; `null` while unknown, or when the edge has swaps off
     *  (`/swap/quote` → 404). The Home Swap button hides then. */
    var swapQuote by mutableStateOf<Quote?>(null); private set

    /** Every swap on this phone, newest first (re-read from the store). */
    var swaps by mutableStateOf<List<SwapRecord>>(emptyList()); private set

    /** Which swap screen is open over the Shell, or `null` for the nav tabs. */
    var swapRoute by mutableStateOf<SwapRoute?>(null); private set

    /** A create / take / accept is in flight: the screens disable their buttons. */
    var swapBusy by mutableStateOf(false); private set

    /** A taken share code whose terms are showing, awaiting Accept or Decline. */
    var takePreview by mutableStateOf<TakePreview?>(null); private set

    /** Wallets that may swap: seed wallets only (no watch-only, no hardware). */
    val seedWallets: List<WalletConfig> get() = wallets.filter { !it.watchOnly && !it.hardware && it.sealed != null }

    /** Show Home's Swap button: the edge quotes swaps (and there is a wallet to
     *  swap from), or a swap is still running and must stay reachable. */
    /** The Swaps tab shows whenever the edge offers swaps — even with only
     *  watch-only / hardware wallets, where the page explains what's needed —
     *  or while a swap is running. */
    val showSwap: Boolean get() = swapQuote != null || swaps.any { it.step.active }
    /** Swaps are on offer, but no wallet here can sign one. */
    val swapNeedsPhrase: Boolean get() = swapQuote != null && seedWallets.isEmpty()

    fun openSwaps() { error = null; pushPlace(); swapRoute = SwapRoute.List; refreshSwaps(); refreshQuote() }
    fun openNewSwap() { error = null; pushPlace(); swapRoute = SwapRoute.New; refreshQuote() }
    fun openTakeCode() { error = null; takePreview = null; pushPlace(); swapRoute = SwapRoute.Take }
    fun openTakeListed(entry: com.fortis.wallet.data.BookEntry) {
        error = null
        pushPlace()
        swapRoute = SwapRoute.TakeListed(entry)
        seedWallets.forEach { loadSwapFeeReserve(it.id) }
    }

    /** The open public offers (Swaps page); `null` until the first load. */
    var book by mutableStateOf<com.fortis.wallet.data.Book?>(null); private set

    fun refreshBook() = viewModelScope.launch {
        runCatching { fortis.swapApi.book() }.onSuccess { book = it }
    }

    /** The take being attempted, kept for its retries: (offer id | amount) →
     *  client id and keys, so a retry after a lost reply takes once, and under
     *  the recovery tag the edge already holds for it. */
    private var listedTake: Triple<String, String, SwapKeys>? = null

    /** The keys drawn for the share code being taken (its tag is on the edge). */
    private var takeCodeKeys: SwapKeys? = null

    /**
     * Take [amountSat] (on [fromId]'s chain) of listed [entry] at its own price,
     * then check the match's terms ourselves (a problem declines it at once) and
     * start the swap from [fromId] into [toId].
     */
    fun takeListed(entry: com.fortis.wallet.data.BookEntry, fromId: String, toId: String, amountSat: Long) = swapWrap {
        val from = seedWallet(fromId)
        val to = seedWallet(toId)
        val send = com.fortis.wallet.ui.screens.takerSendChain(entry)
        if (from.chain != send) throw Exception(str(R.string.swap_error_wrong_chain, send.uppercase()))
        if (from.chain == to.chain) throw Exception(str(R.string.swap_error_same_chain))
        val key = "${entry.id}|$amountSat"
        val t = try {
            withFreshIndex(onUsed = { catchUpSwapIndex(from.id) }) {
                val (_, clientId, keys) = listedTake?.takeIf { it.first == key }
                    ?: Triple(key, newClientId(), drawKeys(from.id)).also { listedTake = it }
                try {
                    fortis.swapApi.takeOffer(entry.id, amountSat, entry.priceSatPerXbt, clientId, keys.recoveryTag)
                } catch (e: SwapHttpException) {
                    // Refused: nothing was taken, so the next try starts afresh. Its
                    // index goes back unless the edge holds it (used before).
                    if (e.code in 400..499) {
                        listedTake = null
                        if (!indexUsed(e)) fortis.swapStore.releaseSwapIndex(keys.seed, keys.index)
                    }
                    throw e
                }
            }
        } catch (e: Exception) {
            val plain = com.fortis.wallet.ui.screens.takeError(e) ?: throw e
            if (plain == R.string.swap_take_gone || plain == R.string.swap_take_price_changed) {
                listedTake = null
                refreshBook()
            }
            throw Exception(str(plain))
        }
        val keys = listedTake!!.third
        listedTake = null
        val p = preview(t, entry.priceSatPerXbt)
        if (p.problem != null) {
            takePreview = p
            declineTaken()
            throw Exception(p.problem)
        }
        startTaken(p, from, to, keys)
    }
    fun openSwap(id: String) { error = null; pushPlace(); swapRoute = SwapRoute.Detail(id) }

    /** Back from a swap screen: to the list, and from the list to the tabs. A
     *  taken code left without accepting is declined so the match ends at once. */
    fun swapBack() {
        if (back()) return
        error = null
        if (swapRoute == SwapRoute.Take && takePreview != null) declineTaken()
        swapRoute = when (swapRoute) {
            null, SwapRoute.List -> null
            else -> SwapRoute.List
        }
        // Back from the Swaps tab's own list leaves the tab for Home.
        if (swapRoute == null && nav == NavTab.Swaps) nav = NavTab.Home
    }

    /** Network-fee reserve (sat) for funding a swap from each wallet, for the
     *  New swap screen's "up to 100%" slider. Absent until [loadSwapFeeReserve]. */
    var swapFeeReserves by mutableStateOf<Map<String, Long>>(emptyMap()); private set

    /** Look up [walletId]'s confirmed coins and today's fee rate, and record what
     *  funding a swap from it would cost in network fees. Failures leave it unknown. */
    fun loadSwapFeeReserve(walletId: String) = viewModelScope.launch(Dispatchers.Default) {
        runCatching {
            val w = wallets.firstOrNull { it.id == walletId } ?: return@runCatching
            val s = ensureSession(w.id, quiet = true) ?: return@runCatching
            val b = overviewBackend(w, s)
            val feerate = b.feerateSatVb(6).coerceIn(1uL, uniffi.wallet_ffi.maxSwapFeerateSatVb()).toLong()
            val reserve = com.fortis.wallet.ui.screens.swapFeeReserve(b.utxos(1u).size, feerate, w.chain)
            synchronized(balanceLock) { swapFeeReserves = swapFeeReserves + (walletId to reserve) }
        }
    }

    /** This phone's own NeoxEX reading, shown as the reference on the new-swap screen (`null` when unavailable). */
    var swapReference by mutableStateOf<NeoxEx.Reference?>(null); private set

    fun refreshQuote() = viewModelScope.launch {
        swapQuote = runCatching { fortis.swapApi.quote() }.getOrNull()
        swapReference = runCatching { fortis.swapReference() }.getOrNull()
    }

    fun refreshSwaps() = viewModelScope.launch {
        runCatching { fortis.swapStore.all() }.getOrNull()?.let { all ->
            swaps = all.sortedByDescending { it.createdAt }
        }
    }

    private fun nowSecs() = System.currentTimeMillis() / 1000

    private fun seedWallet(id: String): WalletConfig =
        seedWallets.firstOrNull { it.id == id } ?: throw Exception(str(R.string.swap_error_seed_only))

    /**
     * The swap's two signing secrets (XBT and BTC, both from the from-wallet's
     * seed at [index]), wrapped by [SwapKeystore]; the plain copies are zeroed.
     * Needs the app unlocked.
     */
    private suspend fun wrappedSwapSecrets(from: WalletSession, index: Int): Pair<String, String> =
        withContext(Dispatchers.Default) {
            val w = from.wallet ?: error(str(R.string.swap_error_seed_only))
            fun wrapped(chain: String): String {
                val secret = w.swapSecret(chain, 0u, index.toUInt())
                return try { SwapKeystore.wrap(secret) } finally { secret.fill(0) }
            }
            wrapped("xbt") to wrapped("btc")
        }

    /**
     * A new swap's keys from seed wallet [walletId]'s seed, while unlocked: the
     * seed's next swap index (its counter, so a restore can find the swap again),
     * the index's recovery tag, and the wrapped secrets.
     */
    private suspend fun drawKeys(walletId: String): SwapKeys {
        if (appSecret == null) throw Exception(str(R.string.swap_error_locked))
        val fs = withContext(Dispatchers.Default) { ensureSession(walletId, quiet = true) }
            ?: throw Exception(str(R.string.error_open_wallet))
        val w = fs.wallet ?: throw Exception(str(R.string.swap_error_seed_only))
        val seed = fs.fingerprint ?: w.masterFingerprint()
        val index = fortis.swapStore.nextSwapIndex(seed)
        val tag = withContext(Dispatchers.Default) { w.swapRecoveryTag(0u, index.toUInt()) }
        val (xbt, btc) = wrappedSwapSecrets(fs, index)
        return SwapKeys(index, seed, tag, xbt, btc)
    }

    /**
     * A new swap's redeem address (the to-wallet's next receive, which is then
     * advanced since the redeem always pays it) and refund address (the
     * from-wallet's next receive — only paid if the swap fails, so it isn't
     * advanced), while unlocked.
     */
    private suspend fun swapAddresses(from: WalletConfig, to: WalletConfig): Pair<String, String> {
        if (appSecret == null) throw Exception(str(R.string.swap_error_locked))
        if (from.chain == to.chain) throw Exception(str(R.string.swap_error_same_chain))
        val (fs, ts) = withContext(Dispatchers.Default) {
            ensureSession(from.id, quiet = true) to ensureSession(to.id, quiet = true)
        }
        if (fs == null || ts == null) throw Exception(str(R.string.error_open_wallet))
        return ts.receiveAddress(to.nextReceive).address to fs.receiveAddress(from.nextReceive).address
    }

    /** A new swap's preimage nonce: 16 random bytes, hex (only the initiator's is used). */
    private fun newPreimageNonce(): String =
        ByteArray(16).also { java.security.SecureRandom().nextBytes(it) }.joinToString("") { "%02x".format(it) }

    /**
     * The edge says a drawn index was used before: this phone's counter for
     * [walletId]'s seed is behind (a restore whose search didn't run, another
     * phone on the seed). Move it past every index the edge knows, looking
     * only — nothing is re-keyed.
     */
    private suspend fun catchUpSwapIndex(walletId: String) {
        val fs = withContext(Dispatchers.Default) { ensureSession(walletId, quiet = true) } ?: return
        val w = fs.wallet ?: return
        val seed = fs.fingerprint ?: w.masterFingerprint()
        val (_, highest) = scanTags({ i -> w.swapRecoveryTag(0u, i.toUInt()) }, { fortis.swapApi.recover(it, claim = false) })
        if (highest >= 0) fortis.swapStore.bumpSwapIndex(seed, highest)
    }

    /** Persist first, then start the service (so its first pass sees the swap). */
    private suspend fun startSwap(rec: SwapRecord) {
        fortis.swapStore.put(rec)
        updateConfig(rec.toWalletId) { it.copy(nextReceive = it.nextReceive + 1) }
        showSwap(rec.id)
    }

    /**
     * Post an offer to swap [amountSat] (on the from-wallet's chain) for the other
     * chain: on the public book (**Find a match**), or [private] with a share code
     * (**Swap with a person**).
     *
     * A provisional record (keys, addresses, the request and a random client id)
     * is saved **before** the POST, so no offer can exist that no record follows:
     * the engine re-posts with the same client id after a lost response or a
     * crash, and the edge hands back the same offer. The POST then promotes it.
     */
    fun createOffer(fromId: String, toId: String, amountSat: Long, priceSatPerXbt: Long, private: Boolean) = swapWrap {
        val from = seedWallet(fromId)
        val to = seedWallet(toId)
        val q = swapQuote ?: throw Exception(str(R.string.swap_unavailable))
        // a doomed offer must not leave a provisional record behind
        if (priceSatPerXbt <= 0) throw Exception(str(R.string.swap_error_price))
        amountError(btcLeg(amountSat, from.chain, priceSatPerXbt), q.minBtcSat, q.maxBtcSat)?.let {
            throw Exception(str(it, fmtCoin(amountLimit(it, q.minBtcSat, q.maxBtcSat))))
        }
        val (redeem, refund) = swapAddresses(from, to)
        updateConfig(to.id) { it.copy(nextReceive = it.nextReceive + 1) }
        val posted = withFreshIndex(onUsed = { catchUpSwapIndex(from.id) }) {
            val keys = drawKeys(from.id)
            val clientId = newClientId()
            val provisional = SwapRecord(
                id = clientId, clientId = clientId, step = Step.PROVISIONAL, privateOffer = private,
                fromWalletId = from.id, toWalletId = to.id, sendChain = from.chain,
                amountSat = amountSat, limitPrice = priceSatPerXbt, ownerSecret = "",
                swapIndex = keys.index, wrappedXbt = keys.wrappedXbt, wrappedBtc = keys.wrappedBtc,
                redeemAddress = redeem, refundAddress = refund, createdAt = nowSecs(),
                seed = keys.seed, recoveryTag = keys.recoveryTag, preimageNonce = newPreimageNonce(),
            )
            fortis.swapStore.put(provisional)
            try {
                fortis.swapEngine.postOffer(clientId)
            } catch (e: SwapHttpException) {
                if (e.code != 400 && e.code != 429 && !indexUsed(e)) {
                    // no answer we can trust: the engine re-posts (and adopts) in the background
                    showSwap(clientId)
                } else {
                    // refused: the edge made no offer, so there is nothing to follow
                    // (a used index is tried again on the next one; any other
                    // refusal gives the index back)
                    fortis.swapStore.remove(clientId)
                    if (!indexUsed(e)) fortis.swapStore.releaseSwapIndex(keys.seed, keys.index)
                    refreshSwaps()
                }
                throw e
            } catch (e: Exception) {
                showSwap(clientId)
                throw e
            }
        }
        showSwap(posted.id)
    }

    /** A swap search is running (Settings → Find my swaps). */
    var recovering by mutableStateOf(false); private set

    /** What the last swap search found, shown under its button. */
    var recoverNote by mutableStateOf<String?>(null); private set

    /**
     * Find this phone's seeds' swaps that still have coins locked — after a
     * restore, the records are gone but the edge knows each swap by its
     * recovery tag — and rebuild them, so the engine refunds or completes them.
     * [quiet] (after a restore): note only what was found, never a failure.
     */
    fun recoverSwaps(quiet: Boolean = false) = viewModelScope.launch {
        if (recovering) return@launch
        recovering = true
        // a fresh result each time, so a search visibly ran
        if (!quiet) recoverNote = null
        try {
            val note = try {
                findSwaps()
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                if (quiet) null else str(R.string.swap_recover_failed, e.message ?: e.toString())
                // (quiet: the note says to search again from here)
            }
            if (!quiet || note != str(R.string.swap_recover_none)) recoverNote = note ?: str(R.string.swap_recover_retry)
        } finally {
            recovering = false
        }
    }

    /**
     * [recoverSwaps]' work, per seed: look up its tags (changing nothing),
     * move its counter past them, and pick the swaps to take over — our own
     * leg funded, not settled, and not one this phone already runs (taking
     * that over would re-key it under the running swap). Only those are then
     * claimed (the edge hands out new owner secrets) and rebuilt.
     */
    private suspend fun findSwaps(): String {
        if (appSecret == null) throw Exception(str(R.string.swap_error_locked))
        val bySeed = withContext(Dispatchers.Default) {
            seedWallets.mapNotNull { w ->
                val s = ensureSession(w.id, quiet = true) ?: return@mapNotNull null
                val seed = s.fingerprint ?: s.wallet?.masterFingerprint() ?: return@mapNotNull null
                seed to w
            }.groupBy({ it.first }, { it.second })
        }
        var found = 0
        var needWallets = 0
        for ((seed, ws) in bySeed) {
            val fs = withContext(Dispatchers.Default) { ensureSession(ws.first().id, quiet = true) } ?: continue
            val w = fs.wallet ?: continue
            val (entries, highest) = scanTags(
                { i -> w.swapRecoveryTag(0u, i.toUInt()) },
                { tags -> fortis.swapApi.recover(tags, claim = false) },
            )
            if (highest >= 0) fortis.swapStore.bumpSwapIndex(seed, highest)
            if (BuildConfig.DEBUG) runCatching {
                android.util.Log.i("SwapRecovery", "seed ${seed.take(4)}…: ${entries.size} tags known (highest $highest), ${entries.count { it.second.match != null }} matched")
            }
            val have = fortis.swapStore.all()
            val wanted = mutableMapOf<String, Triple<Int, WalletConfig, WalletConfig>>()
            for ((index, r) in entries) {
                val m = r.match ?: continue
                if (r.offerId == null) continue
                if (have.any { it.id == r.offerId || it.matchId == m.id || it.recoveryTag == r.tag }) continue
                val initiator = m.role == "initiator"
                val own = (if (initiator) m.xbtFunding else m.btcFunding) ?: continue // nothing of ours locked
                // The initiator's XBT already spent: refunded or redeemed, so settled. The
                // participant's BTC spent may be the initiator's redeem, which our XBT
                // redeem still needs: always taken over, the engine tells which.
                if (initiator && fortis.swapCheck.unspent("xbt", own.txid, own.vout) == null) continue
                val (from, to) = recoveryWallets(if (initiator) "xbt" else "btc", ws, seedWallets) ?: run { needWallets++; continue }
                if (BuildConfig.DEBUG) runCatching { android.util.Log.i("SwapRecovery", "taking over ${r.offerId?.take(8)}: ${m.role}, ${from.chain} → ${to.chain}") }
                wanted[r.tag] = Triple(index, from, to)
            }
            for (batch in wanted.keys.chunked(100)) {
                for (r in fortis.swapApi.recover(batch, claim = true)) {
                    val (index, from, to) = wanted[r.tag] ?: continue
                    if (r.ownerSecret == null) continue
                    val (redeem, refund) = swapAddresses(from, to)
                    val (xbt, btc) = wrappedSwapSecrets(fs, index)
                    val rec = recoveredRecord(index, r, seed, from, to, xbt, btc, redeem, refund, nowSecs()) ?: continue
                    fortis.swapStore.put(rec)
                    updateConfig(to.id) { it.copy(nextReceive = it.nextReceive + 1) }
                    found++
                }
            }
        }
        if (found > 0) {
            SwapService.start(getApplication())
            swaps = fortis.swapStore.all().sortedByDescending { it.createdAt }
        }
        return listOfNotNull(
            plural(R.plurals.swap_recovered, found).takeIf { found > 0 },
            plural(R.plurals.swap_recover_need_wallets, needWallets).takeIf { needWallets > 0 },
        ).joinToString("\n").ifEmpty { str(R.string.swap_recover_none) }
    }

    /** Start the service (it drives the saved record) and open the swap — in
     *  place of the New / Take screen it came from, so Back skips that form. */
    private suspend fun showSwap(id: String) {
        SwapService.start(getApplication())
        swaps = fortis.swapStore.all().sortedByDescending { it.createdAt }
        swapRoute = SwapRoute.Detail(id)
    }

    /** A fresh offer idempotency key: 16 random bytes, hex. */
    private fun newClientId(): String =
        ByteArray(16).also { java.security.SecureRandom().nextBytes(it) }.joinToString("") { "%02x".format(it) }

    /**
     * Take a share code: the edge matches us at once (we are the code's opposite
     * direction). The terms are checked (everything but the price, which is the
     * code's own) and shown before the user accepts; nothing is saved until then.
     */
    fun takeCode(code: String) = swapWrap {
        val clean = code.trim().uppercase().filter { it.isLetterOrDigit() }
        if (clean.isEmpty()) throw Exception(str(R.string.swap_error_enter_code))
        takePreview = null
        takeCodeKeys = null
        // The wallets are picked after the terms show, so the keys come from the
        // open seed wallet's seed (or the first one's): any of the user's seeds
        // holds them, and that seed's restore finds the swap by its tag.
        val keyWallet = seedWallets.firstOrNull { it.id == selectedId } ?: seedWallets.firstOrNull()
            ?: throw Exception(str(R.string.swap_error_seed_only))
        val t = withFreshIndex(onUsed = { catchUpSwapIndex(keyWallet.id) }) {
            val keys = drawKeys(keyWallet.id)
            try {
                fortis.swapApi.take(clean, keys.recoveryTag).also { takeCodeKeys = keys }
            } catch (e: SwapHttpException) {
                // refused (a mistyped code…): nothing taken, the index goes back
                if (e.code in 400..499 && !indexUsed(e)) fortis.swapStore.releaseSwapIndex(keys.seed, keys.index)
                throw e
            }
        }
        takePreview = preview(t, limitPrice = null)
    }

    /** The match [t] made, with its terms checked by us: against [limitPrice]
     *  (the price we were shown), or — for a share code, whose maker sets the
     *  price — everything but the price. A failed read cancels our offer. */
    private suspend fun preview(t: Taken, limitPrice: Long?): TakePreview {
        val info = try {
            fortis.swapApi.match(t.matchId, t.ownerSecret)
        } catch (e: Exception) {
            runCatching { fortis.swapApi.cancelOffer(t.offerId, t.ownerSecret) }
            throw e
        }
        val termsJson = info.terms.toString()
        val limit = limitPrice ?: info.terms.getLong("price_sat_per_xbt")
        val problem = runCatching {
            uniffi.wallet_ffi.validateSwapTerms(termsJson, limit.toULong(), info.role, nowSecs().toUInt(), com.fortis.wallet.swap.SWAP_NETWORK)
        }.exceptionOrNull()?.let { it.message ?: it.toString() }
        val reference = runCatching { fortis.swapReference() }.getOrNull()
        return TakePreview(t, termsJson, info.role, reference, problem)
    }

    /** Accept the shown terms: build and save the swap record, then start it. */
    fun acceptTaken(fromId: String, toId: String) = swapWrap {
        val p = takePreview ?: return@swapWrap
        val keys = takeCodeKeys ?: return@swapWrap
        startTaken(p, seedWallet(fromId), seedWallet(toId), keys)
    }

    /** Build and save the swap record for taken match [p] (taken under [keys]' tag), then start it. */
    private suspend fun startTaken(p: TakePreview, from: WalletConfig, to: WalletConfig, keys: SwapKeys) {
        if (from.chain != p.sendChain) throw Exception(str(R.string.swap_error_wrong_chain, p.sendChain.uppercase()))
        val (redeem, refund) = swapAddresses(from, to)
        val rec = SwapRecord(
            id = p.taken.offerId, matchId = p.taken.matchId, fromWalletId = from.id, toWalletId = to.id,
            sendChain = p.sendChain, amountSat = p.sendSat, limitPrice = p.priceSatPerXbt,
            ownerSecret = p.taken.ownerSecret, swapIndex = keys.index,
            wrappedXbt = keys.wrappedXbt, wrappedBtc = keys.wrappedBtc,
            redeemAddress = redeem, refundAddress = refund, createdAt = nowSecs(),
            seed = keys.seed, recoveryTag = keys.recoveryTag, preimageNonce = newPreimageNonce(),
        )
        startSwap(rec)
        takePreview = null
        takeCodeKeys = null
    }

    /** Turn the shown terms down: the match ends now and our taking offer comes off the book. */
    fun declineTaken() {
        val p = takePreview ?: return
        takePreview = null
        fortis.appScope.launch {
            runCatching {
                fortis.swapApi.post(p.taken.matchId, p.taken.ownerSecret, org.json.JSONObject().put("type", "decline"))
            }
            runCatching { fortis.swapApi.cancelOffer(p.taken.offerId, p.taken.ownerSecret) }
        }
    }

    /**
     * Take an unmatched offer off the book; the engine then marks the swap
     * cancelled. A record still provisional (its POST unanswered) keeps the
     * cancel, and the engine adopts the offer and takes it down.
     */
    fun cancelOffer(id: String) = swapWrap {
        if (fortis.swapEngine.cancelProvisional(id)) {
            runCatching { fortis.swapEngine.step(id) }
            refreshSwaps()
            return@swapWrap
        }
        val r = fortis.swapStore.find(id) ?: return@swapWrap
        if (r.step != Step.WAITING_MATCH) return@swapWrap
        try {
            fortis.swapApi.cancelOffer(r.id, r.ownerSecret)
        } catch (e: SwapHttpException) {
            if (e.code == 409) throw Exception(str(R.string.swap_error_already_matched))
            throw e
        }
        fortis.swapEngine.step(r.id)
        refreshSwaps()
    }

    /**
     * Confirmations of the counterparty's funding, as the edge announced it, or
     * `null` if it isn't announced yet (or can't be read now). For the detail
     * screen's "they funded n/6".
     */
    suspend fun theirConfirmations(r: SwapRecord): Long? = runCatching {
        val m = r.matchId ?: return null
        val info = fortis.swapApi.match(m, r.ownerSecret)
        val op = (if (r.recvChain == "btc") info.btcFunding else info.xbtFunding) ?: return null
        fortis.swapChain.txout(r.recvChain, op.txid, op.vout)?.confirmations
    }.getOrNull()

    private fun swapWrap(block: suspend () -> Unit) = viewModelScope.launch {
        if (swapBusy) return@launch
        swapBusy = true
        error = null
        try {
            runCatching { block() }.onFailure { error = it.message ?: it.toString() }
        } finally {
            swapBusy = false
        }
    }

    override fun onCleared() {
        // The engine outlives this ViewModel; with no unlocked wallet it gets
        // `null` from the funder (→ NEED_FUNDING) until the next unlock.
        if (fortis.unlockedFunder === funder) fortis.unlockedFunder = null
        super.onCleared()
    }

    /** Drop the in-view backend + derived state — call when the selected wallet changes. */
    private fun resetView() {
        backend = null; fallback = null; usingFallback = false
        status = null; history = emptyList(); feerates = emptyMap()
        pending = null; lastSentTxid = null
        serverHistory = emptyList(); refreshAgain = false
        // Seed with the last successful refresh's balance rather than a blank
        // 0, so opening a wallet shows *something* immediately instead of
        // always starting from scratch — refresh() overwrites this with a
        // fresh value shortly. `status` staying null keeps the "connecting…"
        // hint showing alongside it, so a stale number still reads as
        // "still checking", not as a confirmed-current one.
        serverBalances = wallets.firstOrNull { it.id == selectedId }?.lastKnownBalanceSat
            ?.let { Balances(confirmedSat = it, pendingSat = 0) }
        balances = serverBalances
        selectedId?.let { if (serverBalances != null) showWithLocalSends(it, toOverview = false) }
    }

    // --- navigation ---

    /** One place in the Shell: a tab, and the swap screen open over it (if any). */
    private data class Place(val nav: NavTab, val swapRoute: SwapRoute?)

    /** Where Back goes: the places visited before this one, newest last. */
    private val backStack = ArrayDeque<Place>()

    /** Remember the place in view before moving somewhere else. */
    private fun pushPlace() {
        val here = Place(nav, swapRoute)
        if (backStack.lastOrNull() != here) backStack.addLast(here)
        while (backStack.size > MAX_BACK) backStack.removeFirst()
    }

    /** A fresh start (locked, wallet added or removed): nothing to go back to. */
    private fun clearBack() = backStack.clear()

    /**
     * Back within the Shell: return to the place visited before this one, or
     * Home. False on Home with nothing behind it — the caller then asks before
     * leaving the app.
     */
    fun back(): Boolean {
        val prev = backStack.removeLastOrNull() ?: Place(NavTab.Home, null).takeIf {
            // Nothing remembered (e.g. after a restart): Home is where Back ends up.
            nav != NavTab.Home || swapRoute != null
        } ?: return false
        error = null
        if (swapRoute != null && prev.swapRoute != swapRoute) declineTaken()
        swapRoute = prev.swapRoute
        nav = prev.nav
        if (nav == NavTab.Swaps || swapRoute == SwapRoute.List) { refreshSwaps(); refreshQuote() }
        if (phase == Phase.Shell) enterTab()
        return true
    }

    /** Switch the Shell's top-nav tab. */
    fun go(tab: NavTab) {
        error = null
        if (nav == tab && swapRoute == null) return
        pushPlace()
        if (swapRoute != null) { declineTaken(); swapRoute = null }
        nav = tab
        if (tab == NavTab.Swaps) { refreshSwaps(); refreshQuote() }
        if (phase == Phase.Shell) enterTab()
    }
    fun goHome() = go(NavTab.Home)
    fun goSettings() = go(NavTab.Settings)
    /** Legacy call sites — the picker / manage screen are now the Home / Settings tabs. */
    fun goWalletList() = go(NavTab.Home)
    fun goManageWallets() = go(NavTab.Settings)

    val canAddWallet: Boolean get() = wallets.size < MAX_WALLETS
    fun addWallet() {
        if (!canAddWallet) { error = str(R.string.error_wallet_max, MAX_WALLETS); return }
        error = null; draftMnemonic = null; phase = Phase.Onboard
    }
    fun cancelOnboard() {
        draftMnemonic = null
        phase = if (wallets.isEmpty()) Phase.Onboard else Phase.Shell
    }

    /** Back from a step of adding a wallet (generate, restore, watch, hardware)
     *  to the screen where you chose how to add it. */
    fun backToOnboard() { error = null; phase = Phase.Onboard }

    /** Adding a wallet over existing ones (Back then returns to them). */
    val addingWallet: Boolean get() = wallets.isNotEmpty()

    fun goCreate() { phase = if (draftMnemonic != null) Phase.Create else Phase.Gen }
    fun generateSeed(extra: ByteArray, words: Int) = wrap {
        draftMnemonic = newMnemonic(extra, words)
        phase = Phase.Create
    }
    fun goRestore() { phase = Phase.Restore }
    fun goWatch() { phase = Phase.Watch }
    fun goHwConnect() { error = null; phase = Phase.HwConnect }

    /** Is the next wallet the first one requiring an app lock? True until a
     *  signing wallet exists — a device holding only watch-only wallets has
     *  never actually set one up. */
    val settingUp: Boolean get() = wallets.all { it.watchOnly }

    /** Pick a wallet from the Home list — opens it on the Wallet tab. */
    fun selectWallet(id: String) {
        if (id != selectedId) {
            selectedId = id
            viewModelScope.launch { store.setSelected(id) }
            resetView()
        }
        if (nav != NavTab.Wallet || swapRoute != null) pushPlace()
        if (swapRoute != null) { declineTaken(); swapRoute = null }
        nav = NavTab.Wallet
        resolvePhase()
    }

    // --- app unlock ---

    /** Biometric mode: the UI unwrapped the app secret via the Keystore. */
    fun appUnlockWithSecret(secret: String) = wrap {
        appSecret = secret
        resolvePhase()
        fortis.onAppUnlocked()
    }

    /** Password mode: open a wallet with the password off the main thread (it
     *  runs Argon2id), caching the session so the balance scan reuses it. */
    fun appUnlockWithPassword(pw: String) = wrap {
        val c = config?.takeIf { !it.watchOnly } ?: wallets.first { !it.watchOnly }
        withContext(Dispatchers.Default) {
            val (mnemonic, passphrase) = try {
                unsealSeed(c.sealed!!, c.salt!!, pw)
            } catch (e: com.fortis.wallet.wallet.WrongPassword) {
                throw Exception(str(R.string.error_wrong_password))
            }
            WalletSession.signing(c.chain, c.network, mnemonic, passphrase).also {
                it.setIndices(c.nextReceive, c.nextChange)
                sessions[c.id] = it
            }
        }
        appSecret = pw
        resolvePhase()
        fortis.onAppUnlocked()
    }

    /** Lock the whole app — clears every seed and the app secret from memory. */
    fun lock() {
        sessions.values.forEach { it.close() }
        sessions.clear()
        appSecret = null
        synchronized(balanceLock) { overview = OverviewBalances(); coinPrices = emptyMap() }
        overviewBackends.clear()
        revealingId = null
        declineTaken()
        swapRoute = null
        resetView()
        nav = NavTab.Home
        clearBack()
        phase = when {
            wallets.isEmpty() -> Phase.Onboard
            wallets.all { it.watchOnly } -> Phase.Shell // nothing was ever locked
            else -> Phase.AppLock
        }
    }

    // --- create / restore ---

    fun createWallet(name: String, chain: String, network: String, passphrase: String, lock: LockSetup? = null) = wrap {
        finishOnboard(name, chain, network, draftMnemonic!!, passphrase, lock)
    }

    fun restoreWallet(name: String, phrase: String, passphrase: String, chain: String, network: String, lock: LockSetup? = null) = wrap {
        val words = phrase.trim().split(Regex("\\s+")).joinToString(" ")
        finishOnboard(name, chain, network, words, passphrase, lock)
        recoverSwaps(quiet = true)
    }

    private suspend fun finishOnboard(
        name: String, chain: String, network: String,
        mnemonic: String, passphrase: String, lock: LockSetup?,
    ) {
        if (settingUp) {
            requireNotNull(lock) { "the first wallet sets up the app lock" }
            appSecret = lock.secret
            appWrapped = lock.appWrapped
            lockMode = lock.mode
            store.setLock(lock.mode, lock.appWrapped)
        }
        requireFreeName(name, chain)
        val secret = appSecret ?: error("the app is locked")
        val s = WalletSession.signing(chain, network, mnemonic, passphrase)
        val sealed = sealSeed(mnemonic, passphrase, secret)
        val id = UUID.randomUUID().toString()
        val c = WalletConfig(id, cleanWalletName(name), chain, network, sealed.blobHex, sealed.saltHex)
        store.save(c); store.setSelected(id)
        wallets = wallets + c
        selectedId = id
        resetView()
        sessions[id] = s
        draftMnemonic = null
        nav = NavTab.Wallet
        clearBack()
        resolvePhase()
    }

    /** Import an account-level xpub with no seed at all — nothing to sign
     *  with, nothing to seal, no app-lock interaction even as a device's
     *  very first wallet. */
    fun watchWallet(name: String, xpub: String, chain: String) = wrap {
        val trimmed = xpub.trim()
        if (trimmed.isEmpty()) throw Exception(str(R.string.error_enter_xpub))
        if (xpubDepth(trimmed) == 0) throw Exception(str(R.string.error_xpub_is_master_key))
        try {
            WalletSession.watchOnly(chain, "mainnet", trimmed).close() // throws on a malformed xpub
        } catch (e: Exception) {
            val invalid = e.message?.contains("invalid account xpub", ignoreCase = true) == true
            throw Exception(if (invalid) str(R.string.error_invalid_xpub) else (e.message ?: str(R.string.error_invalid_xpub)))
        }
        finishWatchOnly(name, chain, trimmed)
    }

    // --- hardware wallet ---

    /** The BTC hardware wallet just connected, while its "open the matching XBT
     *  wallet?" offer is showing. */
    var hwOfferXbtFor by mutableStateOf<String?>(null); private set

    /** Parse pasted / single-frame-scanned export text (zpub, `[fp/84h/0h/0h]xpub`,
     *  single-part UR) as a BTC BIP-84 account. Throws with a user-facing message. */
    fun importAccountText(text: String): ImportedAccount =
        friendlyImportError { uniffi.wallet_ffi.importAccount(text, "btc", "mainnet") }

    /** The finished result of an animated-QR scan, as [importAccountText]. */
    fun importAccountScan(scanner: uniffi.wallet_ffi.UrAccountScanner): ImportedAccount =
        friendlyImportError { scanner.result("btc", "mainnet") }

    private fun <T> friendlyImportError(block: () -> T): T = try {
        block()
    } catch (e: Exception) {
        val m = e.message.orEmpty()
        throw Exception(when {
            m.contains("Native SegWit") -> str(R.string.error_not_bip84)
            m.contains("invalid account xpub", ignoreCase = true) -> str(R.string.error_invalid_xpub)
            else -> m.ifBlank { str(R.string.error_invalid_xpub) }
        })
    }

    /** Add the device's BTC account as a hardware watch-only wallet, then offer
     *  the same addresses on XBT. */
    fun connectHardware(name: String, account: ImportedAccount) = wrap {
        if (!canAddWallet) throw Exception(str(R.string.error_wallet_max, MAX_WALLETS))
        WalletSession.watchOnly("btc", "mainnet", account.xpub).close() // same check watchWallet makes
        hwOfferXbtFor = finishWatchOnly(name, "btc", account.xpub, hardware = true, masterFingerprint = account.masterFingerprint)
    }

    fun acceptXbtOffer() {
        val id = hwOfferXbtFor ?: return
        hwOfferXbtFor = null
        cloneToOtherChain(id)
    }

    fun dismissXbtOffer() { hwOfferXbtFor = null }

    private suspend fun finishWatchOnly(
        name: String,
        chain: String,
        xpub: String,
        hardware: Boolean = false,
        masterFingerprint: String? = null,
    ): String {
        requireFreeName(name, chain)
        val id = UUID.randomUUID().toString()
        val c = WalletConfig(
            id, cleanWalletName(name), chain, "mainnet", watchOnly = true, xpub = xpub,
            hardware = hardware, masterFingerprint = masterFingerprint,
        )
        store.save(c); store.setSelected(id)
        wallets = wallets + c
        selectedId = id
        resetView()
        draftMnemonic = null
        nav = NavTab.Wallet
        clearBack()
        resolvePhase()
        return id
    }

    // --- manage ---

    fun renameWallet(id: String, name: String) = wrap {
        val clean = name.trim().take(MAX_WALLET_NAME)
        if (clean.isEmpty()) return@wrap
        val chain = wallets.firstOrNull { it.id == id }?.chain ?: return@wrap
        requireFreeName(clean, chain, exceptId = id)
        updateConfig(id) { it.copy(name = clean) }
    }

    /** Throws (shown as the screen's error) when [name] is taken on [chain]. */
    private fun requireFreeName(name: String, chain: String, exceptId: String? = null) {
        if (walletNameTaken(wallets, name, chain, exceptId)) {
            throw Exception(str(R.string.error_name_taken, chain.uppercase(), cleanWalletName(name)))
        }
    }

    // --- reveal recovery phrase ---

    var revealingId by mutableStateOf<String?>(null); private set

    /** Go to the (screenshot-blocked) recovery-phrase screen for a wallet. */
    /** A fresh "it's you" (fingerprint or password) for a reveal or a send. */
    private val reauth = com.fortis.wallet.security.Reauth(com.fortis.wallet.security.REAUTH_FRESH_MS)

    /** The UI proved it's the user: [given] is the app secret their fingerprint
     *  unwrapped, or their password. True if it matches the unlocked app's. */
    fun confirmIdentity(given: String): Boolean = reauth.confirm(given, appSecret)

    /** Reveal a recovery phrase — only right after [confirmIdentity]. */
    fun startReveal(id: String) {
        if (!reauth.consume()) { error = str(R.string.reauth_needed); return }
        error = null; revealingId = id; phase = Phase.RevealSeed
    }
    fun closeReveal() { revealingId = null; resolvePhase() }

    /** Decrypt a wallet's seed for display. `(words, passphrase)`; passphrase is
     *  "" when none was set. Runs Argon2id — call from a coroutine. */
    suspend fun seedFor(id: String): Pair<List<String>, String>? {
        val c = wallets.firstOrNull { it.id == id } ?: return null
        if (c.watchOnly) return null
        val secret = appSecret ?: return null
        return withContext(Dispatchers.Default) {
            val (mnemonic, passphrase) = unsealSeed(c.sealed!!, c.salt!!, secret)
            mnemonic.trim().split(Regex("\\s+")) to passphrase
        }
    }

    /** Add a wallet to the other chain under the same name (same seed, same
     *  addresses). Doesn't change the selection. */
    fun cloneToOtherChain(id: String) = wrap {
        val c = wallets.firstOrNull { it.id == id } ?: return@wrap
        if (!canAddWallet) { error = str(R.string.error_wallet_max, MAX_WALLETS); return@wrap }
        if (walletNameTaken(wallets, c.name, c.otherChain)) {
            error = str(R.string.error_clone_exists, c.name, c.otherChain.uppercase())
            return@wrap
        }
        // Fresh chain: start address discovery at 0 rather than inheriting the
        // source wallet's counters (it's never received on this chain yet).
        val clone = c.copy(
            id = UUID.randomUUID().toString(), chain = c.otherChain, backendToken = null,
            nextReceive = 0, nextChange = 0,
        )
        store.save(clone)
        wallets = wallets + clone
        // the seed may have swaps that needed this chain's wallet to recover
        if (!c.watchOnly && !c.hardware) recoverSwaps(quiet = true)
    }

    /** Remove a wallet from the app. The seed is not destroyed — its recovery
     *  phrase still restores it. Stays on the current screen unless the wallet
     *  in view was the one removed. */
    fun removeWallet(id: String) = viewModelScope.launch {
        val wasCurrent = id == selectedId
        sessions.remove(id)?.close()
        overviewBackends.remove(id)
        synchronized(balanceLock) { overview = overview.without(id) }
        val remaining = store.remove(id)
        wallets = remaining.wallets
        selectedId = remaining.selectedId
        lockMode = remaining.lockMode
        appWrapped = remaining.appWrapped
        if (wallets.isEmpty()) {
            appSecret = null
            com.fortis.wallet.data.SeedKeystore.deleteKey()
            nav = NavTab.Home
            clearBack()
            resetView()
            resolvePhase()
        } else if (wasCurrent) {
            resetView()
            nav = NavTab.Home
            clearBack()
            resolvePhase()
        }
    }

    // --- backend ---

    /** Drop the stored token and mint a fresh one (Settings → Reconnect).
     *  Found live, 2026-09-28: this used to reset connection state and call
     *  `backend!!.status()` only to discard its result, then rely on
     *  whatever ambient poll loop the Wallet screen happens to be running to
     *  eventually notice — which meant tapping this while already on that
     *  screen could look like a complete no-op until the next scheduled
     *  tick. Now calls [refresh] itself directly, so the button visibly does
     *  something every time regardless of any poll timing. */
    fun reconnect() = wrap {
        val id = selectedId ?: return@wrap
        if (ensureSession(id) == null) return@wrap // needs the wallet open
        val token = edgeRegister(http, HOSTED_EDGE)
        updateConfig(id) { it.copy(backendToken = token) }
        backend = edgeBackend(token)
        usingFallback = false
        status = null; history = emptyList()
        resolvePhase()
        refresh()
    }

    /** The hosted fortis-edge as an Esplora backend at `{HOSTED_EDGE}/{chain}`. */
    private fun edgeBackend(token: String): EsploraBackend {
        val c = config!!
        val id = c.id
        return EsploraBackend(
            http, "$HOSTED_EDGE/${c.chain}", session!!.view,
            { (wallets.firstOrNull { it.id == id } ?: c).let { w -> w.nextReceive to w.nextChange } },
            token,
            "$HOSTED_EDGE/pricing",
            bulkPrewarm = c.chain == "btc",
            chain = c.chain,
            txCache = txCache,
            batchScan = true,
        ) {
            val fresh = edgeRegister(http, HOSTED_EDGE)
            updateConfig(id) { it.copy(backendToken = fresh) }
            fresh
        }
    }

    private suspend fun updateConfig(id: String, f: (WalletConfig) -> WalletConfig) {
        val cur = wallets.firstOrNull { it.id == id } ?: return
        val next = f(cur)
        store.save(next)
        wallets = wallets.map { if (it.id == id) next else it }
    }

    private fun ensureBackend() {
        val c = config ?: return
        val view = session?.view ?: return
        if (backend == null) backend = edgeBackend(c.backendToken ?: "")
        if (fallback == null) {
            val esplora = if (c.chain == "btc") PUBLIC_BTC_ESPLORA else PUBLIC_XBT_ESPLORA
            fallback = EsploraBackend(
                http, esplora, view, { config!!.nextReceive to config!!.nextChange },
                chain = c.chain, txCache = txCache,
            )
        }
    }

    fun refresh(): kotlinx.coroutines.Job = viewModelScope.launch {
        // The wallet-detail screen's poll loop calls this every 20s with no
        // memory of whether the last call ever finished — without this guard,
        // a scan that runs longer than 20s (any real watch-only import with
        // more than a handful of used addresses; a deep one can take minutes)
        // gets a *second*, fully overlapping refresh() stacked on top of it
        // every single tick, then a third, then a fourth. Found live,
        // 2026-09-15: this is what turned "a slow scan" into "an ever-growing
        // pile of concurrent scans all sharing one OkHttpClient and one
        // EsploraBackend's mutable cache fields", which is a completely
        // different and far worse problem — the request volume alone was
        // enough to make an otherwise-healthy edge look unreliable.
        if (refreshing) return@launch
        refreshing = true
        refreshAgain = false
        lastRefreshError = null
        val edge = backend ?: run { refreshing = false; return@launch }
        // The wallet this refresh() call is for — captured now because
        // `selectedId`/`config` can change while the suspend calls below are
        // in flight (a wallet switch runs freely at any suspension point).
        val refreshingFor = selectedId
        val refreshingChain = config?.chain
        suspend fun load(b: Backend, degraded: Boolean) {
            // Fire-and-forget: prewarm() only ever speeds up a *live* gap-limit
            // walk (it batch-primes the edge's own cache before the per-address
            // fetches that follow). Awaiting it here used to block everything
            // else in load() behind a full network POST — pointless, and
            // actively defeating TxCache's whole point, on a reopen/wallet
            // switch that already has a warm local cache: `EsploraBackend` is
            // recreated on every such switch (see resetView()), so prewarm()'s
            // own 45s throttle resets too and it fired on every single reopen
            // regardless of whether a live walk was even about to happen.
            viewModelScope.launch { runCatching { b.prewarm() } }
            val newStatus = b.status().copy(degraded = degraded)
            // balancesAndHistory(), not two separate calls: a batch-capable
            // backend answers both from the exact same underlying fetch (see
            // EsploraBackend.balancesAndHistory's doc) — calling them
            // separately used to risk one succeeding and the other hitting a
            // network hiccup in the gap right after, showing history with no
            // balance to go with it even though the server had already
            // answered both together. Found live, 2026-09-28, with a flaky
            // DNS resolver in the mix. Committed together right below, for
            // the same reason: there's nothing left to stagger once they
            // come from one call.
            val (newBalances, newHistory) = b.balancesAndHistory(50)
            if (backend !== b && fallback !== b) return
            status = newStatus
            usingFallback = degraded
            // History is always safe to show as-is, even from a degraded
            // answer — every entry in it is a real, permanently-confirmed
            // transaction (see fortis-edge's btc_history doc), just possibly
            // an incomplete list. Balance is not: a degraded answer's
            // `utxos` is deliberately empty (see scan_chain's fallback), and
            // committing that to the screen — even for one refresh cycle —
            // means a wallet whose real balance we already know flashes to
            // 0 before the next successful poll corrects it. Found live,
            // 2026-09-28: fixed the *persistence* of a degraded 0 as "last
            // known" earlier the same day, but missed that the live display
            // had the identical problem one line up from it — keep
            // whatever's already showing (a real balance from this session,
            // or the seeded last-known one) instead of overwriting it with
            // data the server itself said not to trust.
            serverHistory = newHistory
            if (!newBalances.serverDegraded) serverBalances = newBalances
            // Payments this phone sent that the server hasn't listed yet still show.
            if (refreshingFor != null) showWithLocalSends(refreshingFor, newHistory)
            else {
                history = newHistory
                if (!newBalances.serverDegraded) balances = newBalances
            }
            // Only when this landed via the primary backend (not the old
            // public-explorer fallback, `degraded`) *and* the edge itself
            // didn't flag the data as its own degraded answer — same
            // reasoning as the display guard just above, applied to what
            // gets remembered as "last known" for next time.
            if (!degraded && !newBalances.serverDegraded) {
                refreshingFor?.let { id ->
                    // Skip the write entirely when unchanged — this runs on
                    // every successful poll (every 20s while a wallet's
                    // open), and the common case is an unchanged balance.
                    val prev = wallets.firstOrNull { it.id == id }?.lastKnownBalanceSat
                    if (prev != newBalances.confirmedSat) {
                        updateConfig(id) { it.copy(lastKnownBalanceSat = newBalances.confirmedSat) }
                    }
                }
            }

            val newPrices = refreshingChain?.let { ch -> runCatching { b.prices() }.getOrNull()?.let { ch to it } }
            val newFeerates =
                if (feerates.isEmpty()) listOf(1, 6, 144).associateWith { b.feerateSatVb(it).toLong() } else null
            val cur = wallets.firstOrNull { it.id == refreshingFor }?.nextReceive ?: 0
            val freshReceive = runCatching { b.firstUnusedReceive(cur) }.getOrDefault(cur)

            // The user may have switched wallets while the suspend calls above
            // were awaiting a network response — `resetView()`/`ensureBackend()`
            // would have replaced `backend`/`fallback` with a new wallet's by
            // now. Committing this call's results to the shared fields at that
            // point would show the *previous* wallet's data under the *new*
            // one: found live, a slow BTC scan finishing after switching to an
            // XBT wallet displayed the BTC balance/history there until the
            // next XBT poll tick corrected it. Discard silently instead — the
            // new selection's own refresh() owns the UI now. (balances/history
            // already passed this same check right after they were fetched;
            // this second check guards newPrices/newFeerates/freshReceive, which
            // were fetched afterward and could race a switch on their own.)
            if (backend !== b && fallback !== b) return

            newPrices?.let { (ch, p) -> setCoinPrices(ch, p) }
            newFeerates?.let { feerates = it }
            if (refreshingFor != null && freshReceive > cur) {
                updateConfig(refreshingFor) { it.copy(nextReceive = freshReceive) }
            }
        }
        try {
            runCatching { load(edge, false) }
                .recoverCatching { e -> fallback?.let { load(it, true) } ?: throw e }
                .onFailure { e ->
                    // Found live, 2026-09-28: this used to swallow every
                    // exception with zero trace anywhere — not logcat, not
                    // the UI, nothing — which turned a real, fixable bug (a
                    // malformed server response the client threw parsing)
                    // into hours of blind guessing. Now always logged, and
                    // surfaced to the UI via lastRefreshError so a real
                    // failure reads as a real failure, not an indefinite
                    // "Connecting…".
                    // Debug builds only: the message can carry request paths with addresses.
                    if (BuildConfig.DEBUG) android.util.Log.e("WalletRefresh", "refresh failed for ${refreshingChain ?: "?"}", e)
                    lastRefreshError = e.message ?: e::class.simpleName ?: "unknown error"
                    status = null
                }
        } finally {
            refreshing = false
            if (refreshAgain) refresh()
        }
    }

    /** Set when [refreshSoon] found a refresh already running: one more follows it. */
    private var refreshAgain = false

    /** Like [refresh], but never skipped: when one is already running (and may have
     *  fetched before a payment just sent), another runs right after it. */
    fun refreshSoon() {
        if (refreshing) refreshAgain = true else refresh()
    }

    fun newReceiveAddress() = viewModelScope.launch {
        val id = selectedId ?: return@launch
        updateConfig(id) { it.copy(nextReceive = it.nextReceive + 1) }
    }

    /** Review is building the payment (fee rate, coins, plan): the button shows it. */
    var reviewing by mutableStateOf(false); private set

    /** Sign & send is signing and broadcasting: the button shows it. */
    var sending by mutableStateOf(false); private set

    fun buildPayment(
        to: String, amountSat: Long, sweep: Boolean,
        feerateOverride: Long?, confTarget: Int, replayProtect: Boolean,
        feeFromAmount: Boolean = false,
    ) {
        if (reviewing) return
        reviewing = true
        wrap {
            try {
                planPayment(to, amountSat, sweep, feerateOverride, confTarget, replayProtect, feeFromAmount)
            } finally {
                reviewing = false
            }
        }
    }

    private suspend fun planPayment(
        to: String, amountSat: Long, sweep: Boolean,
        feerateOverride: Long?, confTarget: Int, replayProtect: Boolean,
        feeFromAmount: Boolean,
    ) {
        require(sweep || amountSat > 0L) { str(R.string.error_enter_amount) }
        val to = to.trim()
        val b = active(); val s = session!!; val c = config!!
        s.checkAddress(to) // clear "… is not a valid address" before any network I/O
        val feerate = (feerateOverride ?: b.feerateSatVb(confTarget).toLong()).coerceAtLeast(1)
        // Not the coins a payment just sent spent, while the server may still list them.
        val spent = spentLocally(c.id)
        val utxos = b.utxos(1u).filterNot { "${it.txid}:${it.vout}" in spent }
        require(utxos.isNotEmpty()) { str(R.string.error_no_coins) }
        s.setIndices(c.nextReceive, c.nextChange)
        val opReturn = if (replayProtect && c.chain == "btc" && !sweep)
            com.fortis.wallet.wallet.randomBytes(100) else null
        val serviceFee = status?.pricing?.let {
            uniffi.wallet_ffi.ServiceFee(it.address, it.bps.toUInt(), it.floorSat.toULong(), it.capSat.toULong())
        }
        val plan = if (sweep) s.view.planSweep(utxos, to, feerate.toULong(), 1u, serviceFee)
        else s.view.planPayment(
            utxos, listOf(uniffi.wallet_ffi.PayTo(to, amountSat.toULong())), feerate.toULong(), 1u, opReturn, serviceFee, feeFromAmount,
        )
        pending = PlanPreview(plan, feerate.toULong(), to, sweep, replayProtected = opReturn != null)
    }

    fun cancelPending() { pending = null }

    /** Sign and send the pending payment — only right after [confirmIdentity]. */
    fun confirmSend() {
        if (sending) return
        sending = true
        wrap {
            try {
                signAndSend()
            } finally {
                sending = false
            }
        }
    }

    private suspend fun signAndSend() {
        if (!reauth.consume()) throw Exception(str(R.string.reauth_needed))
        val p = pending!!; val b = active(); val s = session!!; val c = config!!
        val signed = s.sign(p.plan.txHex, p.plan.selected)
        val txid = b.broadcast(signed)
        // Sent: nothing below may turn that into an error on screen, or leave the
        // confirm sheet up inviting a second tap.
        val sent = runCatching { uniffi.wallet_ffi.txidOf(signed) }.getOrNull() ?: txid.trim()
        pending = null
        lastSentTxid = sent
        sentCount++
        runCatching {
            val idx = s.view.nextIndices()
            updateConfig(c.id) { it.copy(nextChange = maxOf(it.nextChange, idx.nextChange.toInt())) }
        }.onFailure { if (it is CancellationException) throw it }
        runCatching { noteLocalSend(localSend(c, s, p.plan, sent, p.to)) }
        if (selectedId == c.id) showWithLocalSends(c.id)
        refreshSoon()
        followSend(c.id)
    }

    /** Refresh again a few times after a payment, until the server lists it. */
    private fun followSend(id: String) = viewModelScope.launch {
        for (wait in longArrayOf(3_000, 5_000, 10_000, 15_000, 30_000)) {
            kotlinx.coroutines.delay(wait)
            if (selectedId != id || unseenSends(id).isEmpty()) return@launch
            refreshSoon()
        }
    }

    fun dismissLastSent() { lastSentTxid = null }

    private fun wrap(block: suspend () -> Unit) = viewModelScope.launch {
        error = null
        runCatching { block() }.onFailure { error = it.message ?: it.toString() }
    }
}

/** Base58Check-decode just far enough to read a BIP-32 extended key's depth
 *  byte — for a friendlier check than wasm/wallet-ffi's generic "invalid xpub"
 *  error in the one case worth calling out by name: a *master* key (depth 0)
 *  is structurally valid, so it parses fine, but it silently derives
 *  addresses from a non-standard path that no real wallet ever used, leaving
 *  a wallet that looks connected but has permanently empty history. Ported
 *  from the identical helper in web/src/app.js. */
private fun xpubDepth(s: String): Int? {
    val alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
    val fiftyEight = java.math.BigInteger.valueOf(58)
    var num = java.math.BigInteger.ZERO
    for (ch in s) {
        val idx = alphabet.indexOf(ch)
        if (idx == -1) return null
        num = num * fiftyEight + java.math.BigInteger.valueOf(idx.toLong())
    }
    var hex = num.toString(16)
    if (hex.length % 2 != 0) hex = "0$hex"
    val bytes = hex.chunked(2)
    return if (bytes.size > 4) bytes[4].toInt(16) else null
}

/** Per-wallet confirmed and pending balances for the overviews. A degraded scan
 *  (the edge flagged it incomplete; its UTXO list is empty) is ignored, as the
 *  wallet screen does, so a known balance never flashes to 0. */
data class OverviewBalances(
    val confirmed: Map<String, Long> = emptyMap(),
    val pending: Map<String, Long> = emptyMap(),
) {
    fun with(id: String, b: Balances): OverviewBalances =
        if (b.serverDegraded) this
        else OverviewBalances(confirmed + (id to b.confirmedSat), pending + (id to b.pendingSat))

    fun without(id: String) = OverviewBalances(confirmed - id, pending - id)
}

/**
 * A payment this phone broadcast that the server hasn't listed yet. Until it
 * does, the wallet shows it (a pending history row, the new balance) and the
 * next payment leaves its coins alone — otherwise a second send planned from
 * the server's stale coin list would spend the same coins again.
 */
data class LocalSend(
    val walletId: String,
    val txid: String,
    /** The coins it spent, as "txid:vout". */
    val spent: Set<String>,
    /** Their total. Payments spend confirmed coins only. */
    val inSat: Long,
    /** What comes back to this wallet: the change, plus the payment itself when it went to one of its own addresses. */
    val backSat: Long,
    val feeSat: Long,
    val at: Long,
) {
    /** The row the server will list for it (see EsploraBackend.historyFrom). */
    fun entry() = HistoryEntry(
        txid = txid, send = true, amountSat = backSat - inSat + feeSat,
        confirmations = 0L, time = at / 1000, feeSat = feeSat,
    )
}

/** How long a local send is still trusted without the server listing it — past
 *  this it was most likely dropped, and its coins are spendable again. */
const val LOCAL_SEND_MS = 30 * 60_000L

/** The server's balance and history with the [unseen] local sends applied. */
fun withLocalSends(b: Balances, h: List<HistoryEntry>, unseen: List<LocalSend>): Pair<Balances, List<HistoryEntry>> {
    if (unseen.isEmpty()) return b to h
    val listed = h.mapTo(HashSet()) { it.txid }
    val add = unseen.filter { it.txid !in listed }
    if (add.isEmpty()) return b to h
    val shown = b.copy(
        confirmedSat = (b.confirmedSat - add.sumOf { it.inSat }).coerceAtLeast(0L),
        pendingSat = b.pendingSat + add.sumOf { it.backSat },
    )
    return shown to (add.sortedByDescending { it.at }.map { it.entry() } + h)
}

/** [sends] without those [listed] for [walletId] (the server has them now) and the expired. */
fun pruneLocalSends(sends: List<LocalSend>, walletId: String, listed: Set<String>?, now: Long): List<LocalSend> =
    sends.filter { now - it.at < LOCAL_SEND_MS && (listed == null || it.walletId != walletId || it.txid !in listed) }
