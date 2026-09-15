package com.fortis.wallet

import android.app.Application
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.fortis.wallet.data.*
import com.fortis.wallet.wallet.WalletSession
import com.fortis.wallet.wallet.newMnemonic
import com.fortis.wallet.wallet.sealSeed
import com.fortis.wallet.wallet.unsealSeed
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import okhttp3.OkHttpClient
import uniffi.wallet_ffi.FundingPlan
import java.net.Proxy
import java.util.UUID
import java.util.concurrent.TimeUnit

enum class Phase { Loading, AppLock, Onboard, Gen, Create, Restore, Watch, Shell, RevealSeed }

/** The three top-level destinations inside [Phase.Shell]'s nav bar. */
enum class NavTab { Home, Wallet, Settings }

/** How many wallets one install can hold. */
const val MAX_WALLETS = 10

/** Longest a wallet name may be. */
const val MAX_WALLET_NAME = 30

/** The one backend the mobile app talks to. Not user-configurable, not shown. */
const val HOSTED_EDGE = "https://api.fortistechlabs.com"

/** Public Esplora fallbacks, used when [HOSTED_EDGE] is unreachable. */
const val PUBLIC_BTC_ESPLORA = "https://mempool.space/api"
const val PUBLIC_XBT_ESPLORA = "https://mempool.guide/api"

data class PlanPreview(
    val plan: FundingPlan, val feerate: ULong, val to: String,
    val sweep: Boolean, val replayProtected: Boolean = false,
)

/** The result of setting up the app lock, produced by the UI for the first wallet. */
data class LockSetup(val mode: String, val secret: String, val appWrapped: String?)

class WalletViewModel(app: Application) : AndroidViewModel(app) {
    private val store = Store(app)
    private fun str(id: Int, vararg args: Any) = getApplication<Application>().getString(id, *args)
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

    /** USD per whole coin, keyed by chain ("btc" / "xbt"). Shared by every
     *  wallet on that chain; absent = no fiat value shown. */
    var coinUsd by mutableStateOf<Map<String, Double>>(emptyMap()); private set

    /** Approximate USD value of `sat` on `chain`, or null if that chain has no
     *  price yet. */
    fun usdValue(sat: Long, chain: String): Double? =
        coinUsd[chain]?.let { it * (sat / 100_000_000.0) }
    var history by mutableStateOf<List<HistoryEntry>>(emptyList()); private set
    var feerates by mutableStateOf<Map<Int, Long>>(emptyMap()); private set
    var pending by mutableStateOf<PlanPreview?>(null); private set
    var lastSentTxid by mutableStateOf<String?>(null); private set
    var error by mutableStateOf<String?>(null)

    init {
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

    /** Confirmed balance (sat) per wallet id, for the Home / Settings overviews.
     *  Absent = not yet known. */
    var walletBalances by mutableStateOf<Map<String, Long>>(emptyMap()); private set
    private val overviewBackends = java.util.concurrent.ConcurrentHashMap<String, Backend>()
    private val balanceLock = Any()
    private var scanningAll = false

    /** Record one wallet's balance — serialised, since the bulk scan runs off the
     *  main thread while [refresh] writes from it. */
    private fun setWalletBalance(id: String, sat: Long) = synchronized(balanceLock) {
        walletBalances = walletBalances + (id to sat)
    }

    private fun setCoinUsd(chain: String, usd: Double) = synchronized(balanceLock) {
        coinUsd = coinUsd + (chain to usd)
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
                    val b = overviewBackends.getOrPut(id) {
                        EsploraBackend(
                            http, "$HOSTED_EDGE/${w.chain}", s.view,
                            { (wallets.firstOrNull { it.id == id } ?: w).let { c -> c.nextReceive to c.nextChange } },
                            w.backendToken ?: "",
                            "$HOSTED_EDGE/pricing",
                            bulkPrewarm = w.chain == "btc",
                        ) {
                            val fresh = edgeRegister(http, HOSTED_EDGE)
                            updateConfig(id) { it.copy(backendToken = fresh) }
                            fresh
                        }
                    }
                    runCatching { b.prewarm() }
                    runCatching { b.balances() }.getOrNull()?.let { setWalletBalance(id, it.confirmedSat) }
                    // one price lookup per chain, reused across its wallets
                    if (priced.add(w.chain)) {
                        runCatching { b.price() }.getOrNull()?.let { setCoinUsd(w.chain, it) }
                    }
                }
            } finally {
                scanningAll = false
            }
        }
    }

    /** Drop the in-view backend + derived state — call when the selected wallet changes. */
    private fun resetView() {
        backend = null; fallback = null; usingFallback = false
        status = null; balances = null; history = emptyList(); feerates = emptyMap()
        pending = null; lastSentTxid = null
    }

    // --- navigation ---

    /** Switch the Shell's top-nav tab. */
    fun go(tab: NavTab) {
        error = null
        nav = tab
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

    fun goCreate() { phase = if (draftMnemonic != null) Phase.Create else Phase.Gen }
    fun generateSeed(extra: ByteArray, words: Int) = wrap {
        draftMnemonic = newMnemonic(extra, words)
        phase = Phase.Create
    }
    fun goRestore() { phase = Phase.Restore }
    fun goWatch() { phase = Phase.Watch }

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
        nav = NavTab.Wallet
        resolvePhase()
    }

    // --- app unlock ---

    /** Biometric mode: the UI unwrapped the app secret via the Keystore. */
    fun appUnlockWithSecret(secret: String) = wrap {
        appSecret = secret
        resolvePhase()
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
    }

    /** Lock the whole app — clears every seed and the app secret from memory. */
    fun lock() {
        sessions.values.forEach { it.close() }
        sessions.clear()
        appSecret = null
        synchronized(balanceLock) { walletBalances = emptyMap(); coinUsd = emptyMap() }
        overviewBackends.clear()
        revealingId = null
        resetView()
        nav = NavTab.Home
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
        val secret = appSecret ?: error("the app is locked")
        val s = WalletSession.signing(chain, network, mnemonic, passphrase)
        val sealed = sealSeed(mnemonic, passphrase, secret)
        val id = UUID.randomUUID().toString()
        val cleanName = name.trim().take(MAX_WALLET_NAME).ifBlank { "Wallet" }
        val c = WalletConfig(id, cleanName, chain, network, sealed.blobHex, sealed.saltHex)
        store.save(c); store.setSelected(id)
        wallets = wallets + c
        selectedId = id
        resetView()
        sessions[id] = s
        draftMnemonic = null
        nav = NavTab.Wallet
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

    private suspend fun finishWatchOnly(name: String, chain: String, xpub: String) {
        val id = UUID.randomUUID().toString()
        val cleanName = name.trim().take(MAX_WALLET_NAME).ifBlank { "Wallet" }
        val c = WalletConfig(id, cleanName, chain, "mainnet", watchOnly = true, xpub = xpub)
        store.save(c); store.setSelected(id)
        wallets = wallets + c
        selectedId = id
        resetView()
        draftMnemonic = null
        nav = NavTab.Wallet
        resolvePhase()
    }

    // --- manage ---

    fun renameWallet(id: String, name: String) = wrap {
        val clean = name.trim().take(MAX_WALLET_NAME)
        if (clean.isNotEmpty()) updateConfig(id) { it.copy(name = clean) }
    }

    // --- reveal recovery phrase ---

    var revealingId by mutableStateOf<String?>(null); private set

    /** Go to the (screenshot-blocked) recovery-phrase screen for a wallet. */
    fun startReveal(id: String) { error = null; revealingId = id; phase = Phase.RevealSeed }
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
        if (wallets.any { it.name == c.name && it.chain == c.otherChain }) {
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
    }

    /** Remove a wallet from the app. The seed is not destroyed — its recovery
     *  phrase still restores it. Stays on the current screen unless the wallet
     *  in view was the one removed. */
    fun removeWallet(id: String) = viewModelScope.launch {
        val wasCurrent = id == selectedId
        sessions.remove(id)?.close()
        overviewBackends.remove(id)
        walletBalances = walletBalances - id
        val remaining = store.remove(id)
        wallets = remaining.wallets
        selectedId = remaining.selectedId
        lockMode = remaining.lockMode
        appWrapped = remaining.appWrapped
        if (wallets.isEmpty()) {
            appSecret = null
            com.fortis.wallet.data.SeedKeystore.deleteKey()
            nav = NavTab.Home
            resetView()
            resolvePhase()
        } else if (wasCurrent) {
            resetView()
            nav = NavTab.Home
            resolvePhase()
        }
    }

    // --- backend ---

    /** Drop the stored token and mint a fresh one (Settings → Reconnect). */
    fun reconnect() = wrap {
        val id = selectedId ?: return@wrap
        if (ensureSession(id) == null) return@wrap // needs the wallet open
        val token = edgeRegister(http, HOSTED_EDGE)
        updateConfig(id) { it.copy(backendToken = token) }
        backend = edgeBackend(token)
        usingFallback = false
        backend!!.status()
        status = null; balances = null; history = emptyList()
        resolvePhase()
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
            fallback = EsploraBackend(http, esplora, view, { config!!.nextReceive to config!!.nextChange })
        }
    }

    private var refreshing = false

    fun refresh() = viewModelScope.launch {
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
        val edge = backend ?: run { refreshing = false; return@launch }
        suspend fun load(b: Backend, degraded: Boolean) {
            runCatching { b.prewarm() }
            status = b.status().copy(degraded = degraded)
            usingFallback = degraded
            balances = b.balances()
            selectedId?.let { id -> balances?.let { setWalletBalance(id, it.confirmedSat) } }
            config?.chain?.let { ch -> runCatching { b.price() }.getOrNull()?.let { setCoinUsd(ch, it) } }
            history = b.history(50)
            if (feerates.isEmpty()) feerates = listOf(1, 6, 144).associateWith { b.feerateSatVb(it).toLong() }
            // Gap-limit auto-advance: skip the receive address past any that the
            // scan just found already used, so "Receive" always shows a fresh one.
            selectedId?.let { id ->
                val cur = wallets.firstOrNull { it.id == id }?.nextReceive ?: 0
                val fresh = runCatching { b.firstUnusedReceive(cur) }.getOrDefault(cur)
                if (fresh > cur) updateConfig(id) { it.copy(nextReceive = fresh) }
            }
        }
        try {
            runCatching { load(edge, false) }
                .recoverCatching { e -> fallback?.let { load(it, true) } ?: throw e }
                .onFailure { status = null }
        } finally {
            refreshing = false
        }
    }

    fun newReceiveAddress() = viewModelScope.launch {
        val id = selectedId ?: return@launch
        updateConfig(id) { it.copy(nextReceive = it.nextReceive + 1) }
    }

    fun buildPayment(
        to: String, amountSat: Long, sweep: Boolean,
        feerateOverride: Long?, confTarget: Int, replayProtect: Boolean,
        feeFromAmount: Boolean = false,
    ) = wrap {
        require(sweep || amountSat > 0L) { str(R.string.error_enter_amount) }
        val to = to.trim()
        val b = active(); val s = session!!; val c = config!!
        s.checkAddress(to) // clear "… is not a valid address" before any network I/O
        val feerate = (feerateOverride ?: b.feerateSatVb(confTarget).toLong()).coerceAtLeast(1)
        val utxos = b.utxos(1u)
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

    fun confirmSend() = wrap {
        val p = pending!!; val b = active(); val s = session!!; val c = config!!
        val signed = s.sign(p.plan.txHex, p.plan.selected)
        val txid = b.broadcast(signed)
        val idx = s.view.nextIndices()
        updateConfig(c.id) { it.copy(nextChange = maxOf(it.nextChange, idx.nextChange.toInt())) }
        pending = null
        lastSentTxid = txid.trim().ifBlank { null }
        refresh()
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
