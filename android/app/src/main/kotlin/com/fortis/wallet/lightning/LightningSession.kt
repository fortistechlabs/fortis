package com.fortis.wallet.lightning

import com.fortis.wallet.HOSTED_EDGE
import com.fortis.wallet.data.WalletConfig
import com.fortis.wallet.data.lnChainOf
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import uniffi.wallet_ffi.LnBalances
import uniffi.wallet_ffi.LnChain
import uniffi.wallet_ffi.LnChannelState
import uniffi.wallet_ffi.LnDirection
import uniffi.wallet_ffi.LnEventRecord
import uniffi.wallet_ffi.LnFfiException
import uniffi.wallet_ffi.LnLspLimits
import uniffi.wallet_ffi.LnNode
import uniffi.wallet_ffi.LnNodeConfig
import uniffi.wallet_ffi.LnPayment
import uniffi.wallet_ffi.LnPaymentStatus
import uniffi.wallet_ffi.LnReceiveQuote
import uniffi.wallet_ffi.LnStartMode
import uniffi.wallet_ffi.LnTarget
import uniffi.wallet_ffi.LnVssConfig

/** The Esplora a Lightning node on [chain] reads. BTC uses the user's own BTC
 *  server when one is on ([ownBtcServer]); XBT Lightning always reads the hosted edge. */
fun lightningEsploraUrl(chain: LnChain, ownBtcServer: String?): String = when (chain) {
    LnChain.BTC -> ownBtcServer ?: "$HOSTED_EDGE/btc"
    LnChain.XBT -> "$HOSTED_EDGE/xbt"
}

interface LightningStore {
    suspend fun walletConfig(walletId: String): WalletConfig? = null
    suspend fun esploraUrl(walletId: String, chain: LnChain): String = lightningEsploraUrl(chain, ownBtcServer = null)
    suspend fun vssUrl(walletId: String): String = "$HOSTED_EDGE/ln/vss"
    suspend fun vssAuthHeader(walletId: String): String = ""
    fun dataDir(walletId: String): String = "/tmp/fortis-ln/$walletId"

    /** A transfer to Lightning whose deposit was broadcast and is not channel funds yet. Survives a process death. */
    suspend fun pendingTransfer(walletId: String): PendingTransfer? = null
    suspend fun setPendingTransfer(walletId: String, t: PendingTransfer?) {}
}

class InMemoryLightningStore(
    private val wallets: Map<String, WalletConfig> = emptyMap(),
    /** The BTC node's Esplora (the XBT node's is always the hosted edge's). */
    private val esplora: String = "$HOSTED_EDGE/btc",
    private val vss: String = "$HOSTED_EDGE/ln/vss",
) : LightningStore {

    override suspend fun walletConfig(walletId: String): WalletConfig? = wallets[walletId]

    override suspend fun esploraUrl(walletId: String, chain: LnChain): String =
        lightningEsploraUrl(chain, ownBtcServer = esplora)

    override suspend fun vssUrl(walletId: String): String = vss

    override suspend fun vssAuthHeader(walletId: String): String =
        wallets[walletId]?.backendToken?.takeIf { it.isNotBlank() }?.let { "Bearer $it" } ?: ""

    override fun dataDir(walletId: String): String = "/tmp/fortis-ln-test/$walletId"

    private val transfers = java.util.concurrent.ConcurrentHashMap<String, PendingTransfer>()
    override suspend fun pendingTransfer(walletId: String): PendingTransfer? = transfers[walletId]
    override suspend fun setPendingTransfer(walletId: String, t: PendingTransfer?) {
        if (t == null) transfers.remove(walletId) else transfers[walletId] = t
    }
}

interface LnNodeApi {
    val blocksOnNextEvent: Boolean get() = false

    fun nodeId(): String = "02" + "11".repeat(32)
    fun limits(): LnLspLimits = LnLspLimits(
        minPaymentMsat = 10_000_000uL,
        maxPaymentMsat = 500_000_000uL,
        channelMinSat = 100_000uL,
        channelMaxSat = 500_000uL,
    )

    fun start()
    fun startWith(mode: LnStartMode) = start()
    fun startMode(): LnStartMode? = LnStartMode.NORMAL
    fun takeover() = start()
    fun takeoverWith(mode: LnStartMode) = takeover()
    fun stop()
    fun isQuiescent(): Boolean = true
    fun balances(): LnBalances
    fun receive(amountMsat: ULong, description: String): LnReceiveQuote =
        throw UnsupportedOperationException("receive not implemented in fake")

    fun pay(target: LnTarget, amountMsat: ULong?): String =
        throw UnsupportedOperationException("pay not implemented in fake")

    fun estimateFeeMsat(target: LnTarget, amountMsat: ULong): ULong =
        uniffi.wallet_ffi.lnEstimateFeeMsat(target, amountMsat)

    fun payments(): List<LnPayment> = emptyList()

    /** Sats a move to Lightning must deposit on top of the channel size (the anchor reserve left in transit). */
    fun anchorReserveSat(): ULong = 0uL
    /** The smallest channel the user can fund. */
    fun userChannelMinSat(): ULong = 5_000uL

    /** Msat that can be sent now (the channels' outbound HTLC limit). */
    fun sendableMsat(): ULong = balances().lightningSat * 1_000uL
    /** The most one incoming payment can be over a channel now. */
    fun receivableMsat(): ULong = 0uL
    /** The node key's signature over a Fortis account request body. */
    fun signAccountRequest(body: String): String = ""
    fun lnurlInvoice(target: LnTarget, amountMsat: ULong): String = uniffi.wallet_ffi.lnLnurlInvoice(target, amountMsat)
    fun nextEvent(timeoutMs: ULong): LnEventRecord? = null
    fun transitAddress(): String = ""
    fun openChannel(amountSat: ULong) {}
    fun closeChannel() {}
    fun spliceIn(amountSat: ULong) {}
    fun spliceOut(amountSat: ULong, toAddress: String) {}
    fun forceCloseChannel() {}
    fun sweepTransit(toAddress: String): String? = null
    fun channelState(): LnChannelState
    /** The funding txid of the channel this node funds while it is unconfirmed. */
    /** What a cooperative close pays, in sat per 1000 weight; null when not known. */
    fun closingFeeRateSatPerKw(): ULong? = null
    fun pendingFundingTxid(): String? = null
    /** Confirmations so far and needed of the channel being opened. */
    fun openingProgress(): uniffi.wallet_ffi.LnOpeningProgress? = null
    fun close() {}
}

class FfiLnNode(private val inner: LnNode) : LnNodeApi {
    override val blocksOnNextEvent: Boolean = true

    override fun nodeId(): String = inner.nodeId()
    override fun limits(): LnLspLimits = inner.limits()
    override fun start() = inner.start()
    override fun startWith(mode: LnStartMode) = inner.startWith(mode)
    override fun startMode(): LnStartMode? = inner.startMode()
    override fun takeover() = inner.takeover()
    override fun takeoverWith(mode: LnStartMode) = inner.takeoverWith(mode)
    override fun stop() = inner.stop()
    override fun isQuiescent(): Boolean = inner.isQuiescent()
    override fun balances(): LnBalances = inner.balances()
    override fun receive(amountMsat: ULong, description: String): LnReceiveQuote =
        inner.receive(amountMsat, description)

    override fun pay(target: LnTarget, amountMsat: ULong?): String =
        inner.pay(target, amountMsat)

    override fun estimateFeeMsat(target: LnTarget, amountMsat: ULong): ULong =
        inner.estimateFeeMsat(target, amountMsat)

    override fun payments(): List<LnPayment> = inner.payments()
    override fun anchorReserveSat(): ULong = inner.anchorReserveSat()
    override fun sendableMsat(): ULong = inner.sendableMsat()
    override fun receivableMsat(): ULong = inner.receivableMsat()
    override fun signAccountRequest(body: String): String = inner.signAccountRequest(body)
    override fun nextEvent(timeoutMs: ULong): LnEventRecord? = inner.nextEvent(timeoutMs)
    override fun transitAddress(): String = inner.transitAddress()
    override fun openChannel(amountSat: ULong) = inner.openChannel(amountSat)
    override fun closeChannel() = inner.closeChannel()
    override fun spliceIn(amountSat: ULong) = inner.spliceIn(amountSat)
    override fun spliceOut(amountSat: ULong, toAddress: String) = inner.spliceOut(amountSat, toAddress)
    override fun userChannelMinSat(): ULong = inner.userChannelMinSat()
    override fun forceCloseChannel() = inner.forceCloseChannel()
    override fun sweepTransit(toAddress: String): String? = inner.sweepTransit(toAddress)
    override fun channelState(): LnChannelState = inner.channelState()
    override fun closingFeeRateSatPerKw(): ULong? = inner.closingFeeRateSatPerKw()
    override fun pendingFundingTxid(): String? = inner.pendingFundingTxid()
    override fun openingProgress(): uniffi.wallet_ffi.LnOpeningProgress? = inner.openingProgress()
    override fun close() = inner.close()
}

/** An event poll that returns sooner than this did not wait for an event. */
private const val IDLE_POLL_MIN_NANOS = 50_000_000L
private const val IDLE_POLL_BACKOFF_MS = 250L
private const val START_RETRY_MS = 2_000L
private const val POLL_INTERVAL_MS = 5_000L
private const val AUTO_CANCEL_RETRY_MS = 30_000L

/** Logcat, where there is one (not in JVM unit tests). */
private fun logWarn(msg: String, e: Throwable?) {
    runCatching { android.util.Log.w("FortisLightning", msg, e) }
}

/** A node error as a line for the screen and the report. */
internal fun describeLnError(e: Throwable): String = when (e) {
    is LnFfiException.Other -> e.reason
    else -> e.message?.takeIf { it.isNotBlank() } ?: e.javaClass.simpleName
}

/** The `reason` of the error `close_channel` gives for a channel that is already being closed. */
private const val CLOSE_IN_PROGRESS = "close_in_progress"

class LightningSession(
    private val scope: CoroutineScope,
    private val store: LightningStore,
    private val nodeFactory: (LnNodeConfig) -> LnNodeApi = { cfg -> FfiLnNode(LnNode(cfg)) },
    private val info: LnInfoApi,
    private val sweepAddressProvider: (suspend (walletId: String) -> String?)? = null,
    /** Called after a transit sweep to the address [sweepAddressProvider] gave
     *  was broadcast, so the caller can move on to a fresh receive address. */
    /** Called after a sweep to on-chain went out: the wallet, and how much it swept (0: not known). */
    private val onSweepBroadcast: (suspend (walletId: String, sweptSat: Long) -> Unit)? = null,
    private val ensureAuthHeader: (suspend (walletId: String) -> String)? = null,
    /** The chain this session's node runs on; it starts only that chain's Lightning wallet and
     *  pays only that chain's invoices. No default: a session must say which chain it is. */
    val chain: LnChain,
    /** True only in debug builds (the app passes BuildConfig.DEBUG): the XBT canary override of
     *  [effectiveLnEnabled]. Off by default so tests and release builds see `/ln/info` as is. */
    private val debugCanary: Boolean = false,
    /** Whether the network knows a transaction (`null`: can't tell). Used to notice a channel
     *  funding that was never broadcast; none: never checked. */
    private val fundingOnNetwork: (suspend (txid: String) -> Boolean?)? = null,
    private val nowMs: () -> Long = System::currentTimeMillis,
    /** How often a running node's balances, pending channel and transfer are re-checked on the
     *  session's own clock (0: never — tests that drive [refreshBalances] by hand). Nothing else
     *  polls while the Lightning wallet is on screen: found live 2026-11-16, a stuck channel was
     *  never noticed and a failed open never retried. */
    private val pollIntervalMs: Long = POLL_INTERVAL_MS,
    /** A stuck channel cancels itself and returns the funds (after [cancelStuckChannel]'s own
     *  re-check of the network), without asking: the user's rule 2026-11-16. */
    private val autoCancelStuck: Boolean = true,
    /** Called after a stuck channel was cancelled by the session (to tidy what the app shows). */
    private val onStuckCancelled: (suspend (walletId: String) -> Unit)? = null,
    /** Called once per funding transaction of a channel this node opens (for the history). */
    private val onFundingSeen: (suspend (walletId: String, txid: String) -> Unit)? = null,
) {
    private val mutex = Mutex()

    private val _state = MutableStateFlow<LnState>(LnState.Off)
    val state: StateFlow<LnState> = _state.asStateFlow()

    private val _events = MutableSharedFlow<LnEventRecord>(extraBufferCapacity = 64)
    val events: SharedFlow<LnEventRecord> = _events.asSharedFlow()

    @Volatile
    var currentNode: LnNodeApi? = null
        private set

    @Volatile
    var lastInfo: LnInfo? = null
        private set

    /** Whether this session treats the LSP as enabled: `/ln/info`'s `enabled`, or in a debug
     *  build the XBT canary override. True while the info has not been read yet. */
    val remoteEnabled: Boolean
        get() = lastInfo?.let { effectiveLnEnabled(chain, it.enabled, debugCanary) } != false

    /** Msat that can be sent now, as of the last balance refresh (the channels' outbound HTLC limit, which is below the balance). */
    @Volatile
    var sendableMsat: ULong = 0uL
        private set

    /** What closing the channel will cost in network fee: the node's own close feerate on a close
     *  transaction of [CLOSE_TX_WEIGHT] weight units (the one seen on XBT was 672); null when not known. */
    fun closingFeeSat(): Long? {
        val node = currentNode ?: return null
        val rate = runCatching { node.closingFeeRateSatPerKw() }.getOrNull() ?: return null
        return (rate.toLong() * CLOSE_TX_WEIGHT + 999L) / 1_000L
    }

    /** The same figure as a flow, so a screen that judges a payment by it is redrawn when it changes
     *  (it changes without the balances changing, e.g. once the channel becomes usable after a start). */
    private val _sendableFlow = kotlinx.coroutines.flow.MutableStateFlow(0uL)
    val sendableFlow: kotlinx.coroutines.flow.StateFlow<ULong> = _sendableFlow

    private var unlockedWalletId: String? = null
    private var unlockedSeed: ByteArray? = null
    private var lastKnownBalances: LnBalances = ZERO_LN_BALANCES
    private var eventJob: Job? = null
    private var refreshJob: Job? = null
    /** Where errors are reported (the app sends them to the server); set by the view model. */
    @Volatile
    var errorReporter: ((where: String, error: Throwable) -> Unit)? = null

    /** Restarts a failed start every [START_RETRY_MS] until it works (or the app locks). */
    private var retryJob: Job? = null

    /** The Lightning wallet was deleted: stops its node, wipes the node's local files and goes Off. */
    fun forget(walletId: String): Job = scope.launch {
        retryJob?.cancel()
        mutex.withLock {
            if (unlockedWalletId == walletId) {
                unlockedSeed?.fill(0)
                unlockedSeed = null
                unlockedWalletId = null
            }
            stopCurrentNodeLocked()
            lastKnownBalances = ZERO_LN_BALANCES
            withContext(Dispatchers.IO) { runCatching { java.io.File(store.dataDir(walletId)).deleteRecursively() } }
            _state.value = LnState.Off
        }
    }

    fun onUnlocked(walletId: String, bip39Seed: ByteArray): Job {
        val seedCopy = bip39Seed.copyOf()
        return scope.launch {
            mutex.withLock {
                val w = store.walletConfig(walletId)
                if (w != null && lnChainOf(w.chain) != chain) {
                    seedCopy.fill(0)
                    stopCurrentNodeLocked()
                    unlockedSeed?.fill(0)
                    unlockedSeed = null
                    unlockedWalletId = null
                    _state.value = LnState.Off
                    return@withLock
                }

                // Already running for this wallet (e.g. the app was unlocked again, or another
                // wallet was opened): keep the node. An Error state retries the start below.
                if (unlockedWalletId == walletId && currentNode != null && _state.value is LnState.Ready) {
                    seedCopy.fill(0)
                    return@withLock
                }

                if (unlockedWalletId != null && unlockedWalletId != walletId) {
                    stopCurrentNodeLocked()
                    lastKnownBalances = ZERO_LN_BALANCES
                }
                unlockedSeed?.fill(0)
                unlockedSeed = seedCopy
                unlockedWalletId = walletId

                startLocked(walletId, seedCopy, takeover = false)
            }
        }
    }

    fun onRelocked(): Job = scope.launch {
        retryJob?.cancel()
        mutex.withLock {
            unlockedSeed?.fill(0)
            unlockedSeed = null
            unlockedWalletId = null
            stopCurrentNodeLocked()
            _state.value = LnState.Off
        }
    }

    fun takeover(): Job = scope.launch {
        mutex.withLock {
            val id = unlockedWalletId ?: return@withLock
            val seed = unlockedSeed ?: return@withLock
            startLocked(id, seed, takeover = true)
        }
    }

    fun onConnectivityRestored(): Job = scope.launch {
        mutex.withLock {
            val id = unlockedWalletId ?: return@withLock
            val seed = unlockedSeed ?: return@withLock
            val cur = _state.value
            if (cur is LnState.Error && cur.error !is LnFfiException.ActiveOnAnotherDevice) {
                startLocked(id, seed, takeover = false)
            }
        }
    }

    /** Publishes [bal] and [ch] as the node's state, with the amount that can be sent now. */
    private fun publish(node: LnNodeApi, bal: LnBalances, ch: LnChannelState) {
        lastKnownBalances = bal
        sendableMsat = runCatching { node.sendableMsat() }.getOrDefault(bal.lightningSat * 1_000uL)
        _sendableFlow.value = sendableMsat
        _state.value = LnState.Ready(bal, ch)
    }

    /**
     * Sends the transit balance back to an on-chain wallet of this chain (the one
     * [sweepAddressProvider] gives) when it is an orphan: a closed channel's funds, or a
     * deposit nobody is waiting for. Returns the new balances and channel state.
     */
    private suspend fun sweepOrphanTransit(
        node: LnNodeApi,
        walletId: String,
        bal: LnBalances,
        ch: LnChannelState,
    ): Pair<LnBalances, LnChannelState> {
        val provider = sweepAddressProvider
        if (provider == null || bal.onchainTransitSat == 0uL || ch != LnChannelState.NONE) {
            return bal to ch
        }
        // A deposit on its way to becoming a channel is not an orphan.
        if (activeTransfer(walletId) != null) return bal to ch
        // Wait until nothing more is coming (the close output and any change are all spendable), so ONE sweep takes
        // them together and the network fee is paid once, not once per piece.
        if (bal.pendingCloseSat > 0uL || bal.pendingTransitSat > 0uL) return bal to ch
        val addr = runCatching { provider(walletId) }.getOrNull()
        if (addr.isNullOrBlank()) return bal to ch
        val swept = withContext(Dispatchers.IO) { runCatching { node.sweepTransit(addr) }.getOrNull() }
        if (swept != null) runCatching { onSweepBroadcast?.invoke(walletId, bal.onchainTransitSat.toLong()) }
        return withContext(Dispatchers.IO) {
            runCatching { node.balances() }.getOrDefault(bal) to runCatching { node.channelState() }.getOrDefault(ch)
        }
    }

    /** The pending transfer to Lightning, unless it is stale (then it is dropped). */
    private suspend fun activeTransfer(walletId: String): PendingTransfer? {
        val t = runCatching { store.pendingTransfer(walletId) }.getOrNull() ?: return null
        if (System.currentTimeMillis() - t.atMs > PENDING_TRANSFER_MAX_AGE_MS) {
            runCatching { store.setPendingTransfer(walletId, null) }
            return null
        }
        return t
    }

    private val _transferPending = kotlinx.coroutines.flow.MutableStateFlow(false)
    /** A transfer to Lightning is under way (its deposit not yet in the channel). */
    val transferPending: kotlinx.coroutines.flow.StateFlow<Boolean> = _transferPending

    private val _transferError = kotlinx.coroutines.flow.MutableStateFlow<String?>(null)
    /** Why opening the channel (or splicing in) failed on the last try; the try repeats. */
    val transferError: StateFlow<String?> = _transferError
    private var reportedTransferError: String? = null

    private val _cancelling = kotlinx.coroutines.flow.MutableStateFlow(false)
    /** A stuck channel was cancelled and its funds are on their way back to the on-chain wallet. */
    val cancelling: StateFlow<Boolean> = _cancelling

    private val _opening = kotlinx.coroutines.flow.MutableStateFlow<uniffi.wallet_ffi.LnOpeningProgress?>(null)
    /** Confirmations so far and needed of the channel being opened, for "waiting for x of y". */
    val openingProgress: StateFlow<uniffi.wallet_ffi.LnOpeningProgress?> = _opening

    private val _fundingTxid = kotlinx.coroutines.flow.MutableStateFlow<String?>(null)
    /** The funding transaction of the channel being opened, while it is unconfirmed. */
    val fundingTxid: StateFlow<String?> = _fundingTxid
    private var reportedFunding: String? = null

    private val _stuck = kotlinx.coroutines.flow.MutableStateFlow(false)
    /** A pending channel's funding transaction has not reached the network for [STUCK_AFTER_MS]. */
    val stuckChannel: StateFlow<Boolean> = _stuck
    private var missingSince: Long? = null

    /** The deposit of a transfer to Lightning was broadcast: once it confirms, the transit
     *  balance becomes channel funds (an open, or a splice-in). */
    suspend fun commitTransfer(feerateSatVb: Long) {
        val id = unlockedWalletId ?: return
        runCatching { store.setPendingTransfer(id, PendingTransfer(System.currentTimeMillis(), feerateSatVb)) }
        _transferPending.value = true
    }

    /** Opens or splices with the confirmed transit balance while a transfer is pending.
     *  Errors leave the transfer pending, so the next refresh retries. */
    private suspend fun fundFromTransit(
        node: LnNodeApi, walletId: String, bal: LnBalances, ch: LnChannelState,
    ): Pair<LnBalances, LnChannelState> {
        val t = activeTransfer(walletId)
        _transferPending.value = t != null
        if (t == null) return bal to ch
        val spendable = bal.onchainTransitSat.toLong()
        val amount = spendable - channelFeeMarginSat(t.feerateSatVb)
        val action = transitAction(spendable, ch, t.feerateSatVb)
        if (action == TransitAction.Wait) return bal to ch
        // A failure is kept and reported, not swallowed: found live 2026-11-16, a failed open
        // left "Moving…" on screen for ever with nothing logged.
        val failure = runCatching {
            when (action) {
                TransitAction.Open -> node.openChannel(amount.toULong())
                TransitAction.SpliceIn -> node.spliceIn(amount.toULong())
                TransitAction.Wait -> Unit
            }
        }.exceptionOrNull()
        if (failure != null) {
            if (failure is CancellationException) throw failure
            val text = describeLnError(failure)
            _transferError.value = text
            logWarn("channel $action failed: $text", failure)
            if (text != reportedTransferError) {
                reportedTransferError = text
                runCatching { errorReporter?.invoke("channel_open", failure) }
            }
            return bal to ch
        }
        _transferError.value = null
        reportedTransferError = null
        runCatching { store.setPendingTransfer(walletId, null) }
        _transferPending.value = false
        return withContext(Dispatchers.IO) {
            runCatching { node.balances() }.getOrDefault(bal) to runCatching { node.channelState() }.getOrDefault(ch)
        }
    }

    /** Asks the network whether it has the pending channel's funding transaction and keeps the
     *  stuck clock ([nextMissingSince]). */
    private suspend fun watchFunding(node: LnNodeApi) {
        _opening.value = runCatching { node.openingProgress() }.getOrNull()
        val txid = runCatching { node.pendingFundingTxid() }.getOrNull()
        _fundingTxid.value = txid
        if (txid != null && txid != reportedFunding) {
            reportedFunding = txid
            unlockedWalletId?.let { id -> runCatching { onFundingSeen?.invoke(id, txid) } }
        }
        val check = fundingOnNetwork
        val onNetwork = if (txid != null && check != null) runCatching { check(txid) }.getOrNull() else null
        val now = nowMs()
        missingSince = nextMissingSince(missingSince, txid, onNetwork, now)
        _stuck.value = isChannelStuck(missingSince, now)
        if (txid != null) logWarn("funding ${txid.take(12)} onNetwork=$onNetwork missingSince=$missingSince stuck=${_stuck.value}", null)
    }

    /** Gives up a stuck channel: drops the pending transfer (so the open is not retried), force
     *  closes the channel (its funding never reached the network, so nothing is at stake in it)
     *  and sweeps the coins the node still holds to [toAddress]. */
    suspend fun cancelStuckChannel(toAddress: String): String? {
        // The user's last chance to be wrong is ours: never close a channel whose funding the
        // network does have (the index may only have been slow when the notice came up).
        val node = currentNode
        val txid = node?.let { runCatching { it.pendingFundingTxid() }.getOrNull() }
        val check = fundingOnNetwork
        if (txid != null && check != null && runCatching { check(txid) }.getOrNull() != false) {
            missingSince = null
            _stuck.value = false
            throw LnFfiException.Other("channel_funding_is_on_the_network")
        }
        unlockedWalletId?.let { id -> runCatching { store.setPendingTransfer(id, null) } }
        _transferPending.value = false
        _transferError.value = null
        reportedTransferError = null
        // Right after the force close the node refuses the sweep ("closing") until it has proved the
        // dead funding void: expected, not a failure. The orphan sweep finishes it.
        _cancelling.value = true
        val swept = closeAndSweep(toAddress, force = true, sweepFailureIsFatal = false)
        missingSince = null
        _stuck.value = false
        return swept
    }

    /** Moves [payoutSat] out of the channel to [toAddress] (a splice-out). */
    suspend fun spliceOut(payoutSat: Long, toAddress: String) = mutex.withLock {
        val node = currentNode ?: throw LnFfiException.Other("lightning not ready")
        withContext(Dispatchers.IO) {
            node.spliceOut(payoutSat.toULong(), toAddress)
            publish(node, node.balances(), node.channelState())
        }
    }

    /** True when [walletId]'s seed is held here, i.e. [onUnlocked] already ran for it. */
    fun isUnlockedFor(walletId: String): Boolean = unlockedWalletId == walletId && unlockedSeed != null

    /** The payment history. Reads node files, so it runs on the IO dispatcher. */
    suspend fun payments(): List<LnPayment> = withContext(Dispatchers.IO) {
        currentNode?.let { runCatching { it.payments() }.getOrNull() }.orEmpty()
    }

    /** Re-reads balances and channel state. Blocking node calls run on the IO dispatcher. A start, stop or close in progress
     *  publishes its own state, and these calls must not overlap it, so then this does nothing. */
    suspend fun refreshBalances() {
        if (!mutex.tryLock()) return
        try {
            withContext(Dispatchers.IO) { refreshBalancesLocked() }
        } finally {
            mutex.unlock()
        }
        maybeAutoCancel()
    }

    private val autoCancelBusy = java.util.concurrent.atomic.AtomicBoolean(false)
    private var lastAutoCancelAttemptMs = 0L

    /** Outside the lock (cancelling takes it): when the channel is stuck, cancels it and returns the
     *  funds to the chain's on-chain wallet. Retried every [AUTO_CANCEL_RETRY_MS] if it fails; with no
     *  wallet to pay, it does nothing and the notice stays for the user. */
    private suspend fun maybeAutoCancel() {
        if (!autoCancelStuck || !_stuck.value) return
        val id = unlockedWalletId ?: return
        val now = nowMs()
        if (now - lastAutoCancelAttemptMs < AUTO_CANCEL_RETRY_MS && lastAutoCancelAttemptMs != 0L) return
        if (!autoCancelBusy.compareAndSet(false, true)) return
        try {
            lastAutoCancelAttemptMs = now
            val addr = runCatching { sweepAddressProvider?.invoke(id) }.getOrNull()
            if (addr.isNullOrBlank()) return
            val txid = runCatching { cancelStuckChannel(addr) }
                .onFailure { if (it is CancellationException) throw it; logWarn("auto-cancel refused: ${describeLnError(it)}", null) }
                .getOrNull()
            if (_stuck.value) return
            logWarn("stuck channel cancelled automatically, swept=$txid", null)
            if (txid != null) runCatching { onSweepBroadcast?.invoke(id, 0L) }
            runCatching { onStuckCancelled?.invoke(id) }
        } finally {
            autoCancelBusy.set(false)
        }
    }

    private suspend fun refreshBalancesLocked() {
        val node = currentNode ?: return
        if (_state.value !is LnState.Ready) return
        var bal = runCatching { node.balances() }.getOrNull() ?: return
        var ch = runCatching { node.channelState() }.getOrNull() ?: return
        unlockedWalletId?.let { id ->
            val (b2, c2) = fundFromTransit(node, id, bal, ch)
            // A closed channel's money that has confirmed since: on to the BTC wallet now,
            // not only at the next start.
            val (b3, c3) = sweepOrphanTransit(node, id, b2, c2)
            bal = b3
            ch = c3
        }
        watchFunding(node)
        if (_cancelling.value && ch == LnChannelState.NONE && bal.onchainTransitSat == 0uL && bal.pendingCloseSat == 0uL) {
            _cancelling.value = false
        }
        if (currentNode === node) publish(node, bal, ch)
    }

    private val _lateOutcome = kotlinx.coroutines.flow.MutableSharedFlow<LateOutcome>(extraBufferCapacity = 4)

    /** How a payment ended that we had stopped waiting for ("status still being confirmed"). */
    val lateOutcome: kotlinx.coroutines.flow.SharedFlow<LateOutcome> = _lateOutcome

    /** Watches [id] until it succeeds or fails (up to [LATE_WATCH_MS]) and reports it on [lateOutcome]. */
    private fun watchLate(node: LnNodeApi, id: String) {
        scope.launch(Dispatchers.IO) {
            val end = System.currentTimeMillis() + LATE_WATCH_MS
            while (System.currentTimeMillis() < end && currentNode === node) {
                val p = runCatching { node.payments().firstOrNull { it.id == id } }.getOrNull()
                if (p?.status == LnPaymentStatus.SUCCEEDED || p?.status == LnPaymentStatus.FAILED) {
                    runCatching { refreshBalances() }
                    _lateOutcome.emit(LateOutcome(id, p.status == LnPaymentStatus.SUCCEEDED, p.amountMsat))
                    return@launch
                }
                kotlinx.coroutines.delay(1_000)
            }
        }
    }

    /** Pay [target] and wait until `PaymentSent` or `PaymentFailed` (or terminal payment status). */
    suspend fun pay(target: LnTarget, amountMsat: ULong?): String {
        val node = currentNode ?: throw LnFfiException.Other("lightning not ready")
        return withContext(Dispatchers.IO) {
            // A BOLT11 invoice is guarded against being paid twice by its payment
            // hash (in Rust). An LNURL, a Lightning address or a BOLT12 offer
            // yields a fresh invoice or id every time, so nothing there can tell
            // that "the same" payment is still in flight (e.g. after the 60 s wait
            // below ended with the outcome unknown). Until the outbound payments
            // that are still pending have settled, do not start one of those.
            val freshEachTime = target is LnTarget.LnurlPay ||
                target is LnTarget.LightningAddress ||
                target is LnTarget.Bolt12Offer
            if (freshEachTime) {
                val pending = runCatching { node.payments() }.getOrNull()
                    ?.any { it.direction == LnDirection.OUT && it.status == LnPaymentStatus.PENDING }
                    ?: false
                if (pending) throw LnFfiException.PaymentStateUnknown()
            }
            // Only this chain's invoice or offer is paid (BTC and XBT invoices look alike;
            // UNKNOWN is refused). An LNURL / Lightning address is checked by the node, on the
            // invoice it fetches, before it pays (Rust `Node::prepare`).
            if (target !is LnTarget.LnurlPay && target !is LnTarget.LightningAddress) requireTargetChain(chain, target)
            val id = node.pay(target, amountMsat)
            drainImmediateEvents(node, unlockedWalletId ?: "")
            // A BOLT12 offer asks the recipient for an invoice first, and the node waits up to ~90 s for the
            // answer before it reports failure: wait longer than that, or the result arrives after we gave up.
            val isOffer = target is LnTarget.Bolt12Offer
            val failure = if (isOffer) OFFER_FAILED else "Payment failed"
            if (!node.blocksOnNextEvent) {
                val p = runCatching { node.payments().firstOrNull { it.id == id } }.getOrNull()
                refreshBalances()
                if (p?.status == LnPaymentStatus.FAILED) {
                    throw LnFfiException.Other(failure)
                }
                return@withContext id
            }
            val deadline = System.currentTimeMillis() + (if (isOffer) OFFER_WAIT_MS else 60_000L)
            while (System.currentTimeMillis() < deadline && currentNode === node) {
                val p = runCatching { node.payments().firstOrNull { it.id == id } }.getOrNull()
                if (p?.status == LnPaymentStatus.SUCCEEDED) {
                    refreshBalances()
                    return@withContext id
                }
                if (p?.status == LnPaymentStatus.FAILED) {
                    refreshBalances()
                    throw LnFfiException.Other(failure)
                }
                kotlinx.coroutines.delay(250)
            }
            // Neither succeeded nor failed within the deadline: the payment may
            // still be in flight, so don't report it as sent. Keep watching it, and say how it ended.
            refreshBalances()
            watchLate(node, id)
            throw LnFfiException.PaymentStateUnknown()
        }
    }

    /** Close the active channel (if any) and sweep settled transit funds to [toAddress].
     *
     *  The close is cooperative. If the LSP cannot be reached, or LDK refuses, the error is thrown
     *  and nothing is force-closed: a force close broadcasts the latest commitment, locks the funds
     *  behind the LSP's `to_self_delay` and costs more in fees, so it is only done with
     *  `force = true`, which the app passes after the user has chosen it. A close that is already
     *  under way is not an error: the sweep below proceeds. */
    suspend fun closeAndSweep(toAddress: String, force: Boolean = false, sweepFailureIsFatal: Boolean = true): String? = mutex.withLock {
        val node = currentNode ?: throw LnFfiException.Other("lightning not ready")
        withContext(Dispatchers.IO) {
            val state = node.channelState()
            if (state != LnChannelState.NONE && state != LnChannelState.CLOSING) {
                if (force) {
                    node.forceCloseChannel()
                } else {
                    try {
                        node.closeChannel()
                    } catch (e: LnFfiException.Other) {
                        if (e.reason != CLOSE_IN_PROGRESS) throw e
                    }
                }
            }
            val txid = if (node.balances().onchainTransitSat > 0uL) {
                if (sweepFailureIsFatal) node.sweepTransit(toAddress)
                else runCatching { node.sweepTransit(toAddress) }
                    .onFailure { logWarn("sweep after cancel deferred: ${describeLnError(it)}", null) }
                    .getOrNull()
            } else {
                null
            }
            publish(node, node.balances(), node.channelState())
            txid
        }
    }

    private suspend fun stopCurrentNodeLocked() {
        eventJob?.cancel()
        eventJob = null
        refreshJob?.cancel()
        refreshJob = null
        val node = currentNode
        currentNode = null
        if (node != null) {
            withContext(Dispatchers.IO) {
                runCatching { node.stop() }
                runCatching { node.close() }
            }
        }
    }

    private suspend fun startLocked(walletId: String, seed: ByteArray, takeover: Boolean) {
        startOnceLocked(walletId, seed, takeover)
        scheduleRetryIfFailed(walletId, kotlin.coroutines.coroutineContext[Job])
    }

    /** After a failed start, tries again in [START_RETRY_MS], for as long as this wallet stays
     *  unlocked. Not when another device holds Lightning: that waits for the user's choice. */
    private fun scheduleRetryIfFailed(walletId: String, self: Job?) {
        val s = _state.value
        if (s !is LnState.Error || s.error is LnFfiException.ActiveOnAnotherDevice) return
        logWarn("start failed, retrying in ${START_RETRY_MS}ms: ${s.error}", null)
        retryJob?.takeIf { it !== self }?.cancel()
        retryJob = scope.launch {
            kotlinx.coroutines.delay(START_RETRY_MS)
            mutex.withLock {
                if (unlockedWalletId != walletId) return@withLock
                val seed = unlockedSeed ?: return@withLock
                val cur = _state.value
                if (cur is LnState.Error && cur.error !is LnFfiException.ActiveOnAnotherDevice) {
                    startLocked(walletId, seed, takeover = false)
                }
            }
        }
    }

    private suspend fun startOnceLocked(walletId: String, seed: ByteArray, takeover: Boolean) {
        stopCurrentNodeLocked()
        _state.value = LnState.Starting
        val t0 = System.nanoTime()
        fun lap(what: String) = logWarn("start +${(System.nanoTime() - t0) / 1_000_000}ms $what", null)

        val lnInfo = try {
            withContext(Dispatchers.IO) { info.fetch() }
        } catch (e: CancellationException) {
            throw e
        } catch (e: LnFfiException) {
            _state.value = LnState.Error(e, lastKnownBalances)
            return
        } catch (e: Exception) {
            _state.value = LnState.Error(LnFfiException.LspUnreachable(), lastKnownBalances)
            return
        }
        lastInfo = lnInfo
        lap("lsp info")

        val w = store.walletConfig(walletId)
        val authHeader = ensureAuthHeader?.invoke(walletId)
            ?.takeIf { it.isNotBlank() }
            ?: store.vssAuthHeader(walletId)
        val esploraUrl = store.esploraUrl(walletId, chain)
        val cfg = LnNodeConfig(
            chain = chain,
            bip39Seed = seed.copyOf(),
            network = w?.network ?: lnInfo.network,
            dataDir = store.dataDir(walletId),
            esploraUrl = esploraUrl,
            // The hosted edge requires the install token on chain reads; a server of the user's own must not get it.
            esploraAuthHeader = if (esploraUrl.startsWith(HOSTED_EDGE)) authHeader else "",
            lsp = lnInfo.toLspInfo(),
            vss = LnVssConfig(
                url = store.vssUrl(walletId),
                authHeader = authHeader,
            ),
            limits = lnInfo.toLspLimits(),
        )

        var createdNode: LnNodeApi? = null
        try {
            val node = withContext(Dispatchers.IO) {
                // The factory copies the seed into the node; ours is not needed
                // after that, so it does not sit in the heap until the GC runs.
                val n = try {
                    nodeFactory(cfg)
                } finally {
                    cfg.bip39Seed.fill(0)
                }
                createdNode = n
                if (takeover) {
                    if (effectiveLnEnabled(chain, lnInfo.enabled, debugCanary)) n.takeover() else n.takeoverWith(LnStartMode.EXIT)
                } else {
                    if (effectiveLnEnabled(chain, lnInfo.enabled, debugCanary)) n.start() else n.startWith(LnStartMode.EXIT)
                }
                n
            }
            currentNode = node
            lap("node running")
            var bal = node.balances()
            var ch = node.channelState()

            val (b1, c1) = fundFromTransit(node, walletId, bal, ch)
            val (b2, c2) = sweepOrphanTransit(node, walletId, b1, c1)
            bal = b2
            ch = c2

            publish(node, bal, ch)
            lap("balance shown")
            drainImmediateEvents(node, walletId)
            if (node.blocksOnNextEvent && currentNode === node) {
                startEventPoller(node, walletId)
            }
            if (currentNode === node) startRefreshPoller(node)
        } catch (e: CancellationException) {
            createdNode?.let {
                withContext(Dispatchers.IO) {
                    runCatching { it.stop() }
                    runCatching { it.close() }
                }
            }
            throw e
        } catch (e: LnFfiException) {
            logWarn("start failed", e)
            errorReporter?.invoke("lightning_start", e)
            createdNode?.let { withContext(Dispatchers.IO) { runCatching { it.close() } } }
            currentNode = null
            _state.value = LnState.Error(e, lastKnownBalances)
        } catch (e: Exception) {
            logWarn("start failed", e)
            errorReporter?.invoke("lightning_start", e)
            createdNode?.let { withContext(Dispatchers.IO) { runCatching { it.close() } } }
            currentNode = null
            _state.value = LnState.Error(
                LnFfiException.Other(e.message ?: e.toString()),
                lastKnownBalances,
            )
        } finally {
            // Also when the start was cancelled before the factory ran.
            cfg.bip39Seed.fill(0)
        }
    }

    private suspend fun drainImmediateEvents(node: LnNodeApi, walletId: String) {
        while (currentNode === node) {
            val ev = node.nextEvent(0uL) ?: break
            handleEvent(node, walletId, ev)
        }
    }

    /** Re-checks the running node every [pollIntervalMs] (balances, a pending channel's funding, a
     *  pending transfer's open): see the parameter. */
    private fun startRefreshPoller(node: LnNodeApi) {
        refreshJob?.cancel()
        refreshJob = if (pollIntervalMs <= 0L) null else scope.launch {
            while (isActive && currentNode === node) {
                kotlinx.coroutines.delay(pollIntervalMs)
                runCatching { refreshBalances() }.onFailure { if (it is CancellationException) throw it }
            }
        }
    }

    private fun startEventPoller(node: LnNodeApi, walletId: String) {
        eventJob?.cancel()
        eventJob = scope.launch(Dispatchers.IO) {
            while (isActive && currentNode === node) {
                val startedAt = System.nanoTime()
                val ev = runCatching { node.nextEvent(500uL) }.getOrNull()
                if (ev == null) {
                    // A node that waits returns after its timeout; one that
                    // returns at once (stopped underneath us) must not make
                    // this a busy loop.
                    if (System.nanoTime() - startedAt < IDLE_POLL_MIN_NANOS) {
                        kotlinx.coroutines.delay(IDLE_POLL_BACKOFF_MS)
                    }
                    continue
                }
                handleEvent(node, walletId, ev)
            }
        }
    }

    private suspend fun handleEvent(node: LnNodeApi, walletId: String, ev: LnEventRecord) {
        _events.emit(ev)
        if (ev is LnEventRecord.DeviceLockLost) {
            if (currentNode === node) {
                currentNode = null
                runCatching { node.close() }
                _state.value = LnState.Error(LnFfiException.ActiveOnAnotherDevice(), lastKnownBalances)
            }
            return
        }
        if (currentNode === node) {
            var bal = runCatching { node.balances() }.getOrNull() ?: return
            var ch = runCatching { node.channelState() }.getOrNull() ?: return
            if (ev is LnEventRecord.ChannelClosed) {
                val (b2, c2) = sweepOrphanTransit(node, walletId, bal, ch)
                bal = b2
                ch = c2
            }
            publish(node, bal, ch)
        }
    }
}

/**
 * Whether a node on [chain] runs normally given `/ln/info`'s [infoEnabled]. A debug build
 * ([debug], the `.dev`/`.dev2` apps) runs XBT Lightning even while it is disabled, so the
 * real-phone canary (deploy/lightning.md §12) can run before the operator enables it. Release
 * builds and the BTC chain follow `/ln/info` exactly.
 */
fun effectiveLnEnabled(chain: LnChain, infoEnabled: Boolean, debug: Boolean): Boolean =
    infoEnabled || (debug && chain == LnChain.XBT)

/** Weight units of a cooperative close with one input and two outputs (a real close was 168 vB). */
private const val CLOSE_TX_WEIGHT = 672L

/** A payment whose result came after the app had stopped waiting for it. */
data class LateOutcome(val paymentId: String, val succeeded: Boolean, val amountMsat: ULong)

/** Reason string of the error for a failed BOLT12 offer payment (no answer from the recipient, or no route). */
const val OFFER_FAILED = "Offer payment failed"

/** How long a BOLT12 offer payment is waited for: longer than the node's own invoice-request wait (observed ~130 s). */
private const val OFFER_WAIT_MS = 150_000L

/** How long a payment we stopped waiting for is watched. */
private const val LATE_WATCH_MS = 10 * 60_000L
