package com.fortis.wallet.lightning

import com.fortis.wallet.HOSTED_EDGE
import com.fortis.wallet.data.WalletConfig
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

interface LightningStore {
    suspend fun walletConfig(walletId: String): WalletConfig? = null
    suspend fun esploraUrl(walletId: String): String = "$HOSTED_EDGE/btc"
    suspend fun vssUrl(walletId: String): String = "$HOSTED_EDGE/ln/vss"
    suspend fun vssAuthHeader(walletId: String): String = ""
    fun dataDir(walletId: String): String = "/tmp/fortis-ln/$walletId"

    /** A transfer to Lightning whose deposit was broadcast and is not channel funds yet. Survives a process death. */
    suspend fun pendingTransfer(walletId: String): PendingTransfer? = null
    suspend fun setPendingTransfer(walletId: String, t: PendingTransfer?) {}
}

class InMemoryLightningStore(
    private val wallets: Map<String, WalletConfig> = emptyMap(),
    private val esplora: String = "$HOSTED_EDGE/btc",
    private val vss: String = "$HOSTED_EDGE/ln/vss",
) : LightningStore {

    override suspend fun walletConfig(walletId: String): WalletConfig? = wallets[walletId]

    override suspend fun esploraUrl(walletId: String): String = esplora

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
    override fun close() = inner.close()
}

/** An event poll that returns sooner than this did not wait for an event. */
private const val IDLE_POLL_MIN_NANOS = 50_000_000L
private const val IDLE_POLL_BACKOFF_MS = 250L
private const val START_RETRY_MS = 2_000L

/** Logcat, where there is one (not in JVM unit tests). */
private fun logWarn(msg: String, e: Throwable?) {
    runCatching { android.util.Log.w("FortisLightning", msg, e) }
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
    private val onSweepBroadcast: (suspend (walletId: String) -> Unit)? = null,
    private val ensureAuthHeader: (suspend (walletId: String) -> String)? = null,
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

    /** Msat that can be sent now, as of the last balance refresh (the channels' outbound HTLC limit, which is below the balance). */
    @Volatile
    var sendableMsat: ULong = 0uL
        private set

    private var unlockedWalletId: String? = null
    private var unlockedSeed: ByteArray? = null
    private var lastKnownBalances: LnBalances = ZERO_LN_BALANCES
    private var eventJob: Job? = null
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
                if (w != null && !isLightningWallet(w)) {
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
        _state.value = LnState.Ready(bal, ch)
    }

    /**
     * Sends the transit balance back to the BTC wallet when it is an orphan: a closed channel's
     * funds, or a deposit nobody is waiting for. Returns the new balances and channel state.
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
        val addr = runCatching { provider(walletId) }.getOrNull()
        if (addr.isNullOrBlank()) return bal to ch
        val swept = withContext(Dispatchers.IO) { runCatching { node.sweepTransit(addr) }.getOrNull() }
        if (swept != null) runCatching { onSweepBroadcast?.invoke(walletId) }
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
        val done = when (transitAction(spendable, ch, t.feerateSatVb)) {
            TransitAction.Wait -> false
            TransitAction.Open -> runCatching { node.openChannel(amount.toULong()) }.isSuccess
            TransitAction.SpliceIn -> runCatching { node.spliceIn(amount.toULong()) }.isSuccess
        }
        if (!done) return bal to ch
        runCatching { store.setPendingTransfer(walletId, null) }
        _transferPending.value = false
        return withContext(Dispatchers.IO) {
            runCatching { node.balances() }.getOrDefault(bal) to runCatching { node.channelState() }.getOrDefault(ch)
        }
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
    }

    private suspend fun refreshBalancesLocked() {
        val node = currentNode ?: return
        if (_state.value !is LnState.Ready) return
        var bal = runCatching { node.balances() }.getOrNull() ?: return
        var ch = runCatching { node.channelState() }.getOrNull() ?: return
        unlockedWalletId?.let { id ->
            val (b2, c2) = fundFromTransit(node, id, bal, ch)
            bal = b2
            ch = c2
        }
        if (currentNode === node) publish(node, bal, ch)
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
            val id = node.pay(target, amountMsat)
            drainImmediateEvents(node, unlockedWalletId ?: "")
            if (!node.blocksOnNextEvent) {
                val p = runCatching { node.payments().firstOrNull { it.id == id } }.getOrNull()
                refreshBalances()
                if (p?.status == LnPaymentStatus.FAILED) {
                    throw LnFfiException.Other("Payment failed")
                }
                return@withContext id
            }
            val deadline = System.currentTimeMillis() + 60_000L
            while (System.currentTimeMillis() < deadline && currentNode === node) {
                val p = runCatching { node.payments().firstOrNull { it.id == id } }.getOrNull()
                if (p?.status == LnPaymentStatus.SUCCEEDED) {
                    refreshBalances()
                    return@withContext id
                }
                if (p?.status == LnPaymentStatus.FAILED) {
                    refreshBalances()
                    throw LnFfiException.Other("Payment failed")
                }
                kotlinx.coroutines.delay(250)
            }
            // Neither succeeded nor failed within the deadline: the payment may
            // still be in flight, so don't report it as sent.
            refreshBalances()
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
    suspend fun closeAndSweep(toAddress: String, force: Boolean = false): String? = mutex.withLock {
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
                node.sweepTransit(toAddress)
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
        val esploraUrl = store.esploraUrl(walletId)
        val cfg = LnNodeConfig(
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
                    if (lnInfo.enabled) n.takeover() else n.takeoverWith(LnStartMode.EXIT)
                } else {
                    if (lnInfo.enabled) n.start() else n.startWith(LnStartMode.EXIT)
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
            createdNode?.let { withContext(Dispatchers.IO) { runCatching { it.close() } } }
            currentNode = null
            _state.value = LnState.Error(e, lastKnownBalances)
        } catch (e: Exception) {
            logWarn("start failed", e)
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
