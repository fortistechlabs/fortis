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
import java.util.concurrent.ConcurrentHashMap

interface LightningStore {
    suspend fun lightningEnabled(walletId: String): Boolean
    suspend fun setLightningEnabled(walletId: String, on: Boolean)
    suspend fun walletConfig(walletId: String): WalletConfig? = null
    suspend fun esploraUrl(walletId: String): String = "$HOSTED_EDGE/btc"
    suspend fun vssUrl(walletId: String): String = "$HOSTED_EDGE/ln/vss"
    suspend fun vssAuthHeader(walletId: String): String = ""
    fun dataDir(walletId: String): String = "/tmp/fortis-ln/$walletId"

    /** A "Move to Lightning" deposit that was broadcast and has not been turned into a channel yet. Survives a process death. */
    suspend fun pendingMove(walletId: String): PendingMove? = null
    suspend fun setPendingMove(walletId: String, move: PendingMove?) {}
}

/** [amountSat] was sent to the node's transit address at [atMs] (epoch ms), to become a channel. */
data class PendingMove(val amountSat: Long, val atMs: Long)

/** How long a broadcast deposit may take to arrive before the intent is dropped. */
const val PENDING_MOVE_MAX_AGE_MS = 48L * 60 * 60 * 1000

class InMemoryLightningStore(
    private val wallets: Map<String, WalletConfig> = emptyMap(),
    private val esplora: String = "$HOSTED_EDGE/btc",
    private val vss: String = "$HOSTED_EDGE/ln/vss",
) : LightningStore {
    private val enabledMap = ConcurrentHashMap<String, Boolean>()
    private val moves = ConcurrentHashMap<String, PendingMove>()

    override suspend fun pendingMove(walletId: String): PendingMove? = moves[walletId]

    override suspend fun setPendingMove(walletId: String, move: PendingMove?) {
        if (move == null) moves.remove(walletId) else moves[walletId] = move
    }

    override suspend fun lightningEnabled(walletId: String): Boolean {
        val w = wallets[walletId]
        if (w != null && !isLightningWallet(w)) return false
        return enabledMap[walletId] ?: false
    }

    override suspend fun setLightningEnabled(walletId: String, on: Boolean) {
        val w = wallets[walletId]
        if (w != null && !isLightningWallet(w)) {
            enabledMap.remove(walletId)
            return
        }
        if (on) enabledMap[walletId] = true else enabledMap.remove(walletId)
    }

    override suspend fun walletConfig(walletId: String): WalletConfig? = wallets[walletId]

    override suspend fun esploraUrl(walletId: String): String = esplora

    override suspend fun vssUrl(walletId: String): String = vss

    override suspend fun vssAuthHeader(walletId: String): String =
        wallets[walletId]?.backendToken?.takeIf { it.isNotBlank() }?.let { "Bearer $it" } ?: ""

    override fun dataDir(walletId: String): String = "/tmp/fortis-ln-test/$walletId"
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
    fun anchorReserveSat(): ULong = 25_000uL

    /** Msat that can be sent now (the channels' outbound HTLC limit). */
    fun sendableMsat(): ULong = balances().lightningSat * 1_000uL
    fun nextEvent(timeoutMs: ULong): LnEventRecord? = null
    fun transitAddress(): String = ""
    fun openChannel(amountSat: ULong) {}
    fun closeChannel() {}
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
    override fun nextEvent(timeoutMs: ULong): LnEventRecord? = inner.nextEvent(timeoutMs)
    override fun transitAddress(): String = inner.transitAddress()
    override fun openChannel(amountSat: ULong) = inner.openChannel(amountSat)
    override fun closeChannel() = inner.closeChannel()
    override fun forceCloseChannel() = inner.forceCloseChannel()
    override fun sweepTransit(toAddress: String): String? = inner.sweepTransit(toAddress)
    override fun channelState(): LnChannelState = inner.channelState()
    override fun close() = inner.close()
}

/** An event poll that returns sooner than this did not wait for an event. */
private const val IDLE_POLL_MIN_NANOS = 50_000_000L
private const val IDLE_POLL_BACKOFF_MS = 250L

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

    @Volatile
    var pendingMoveSat: ULong? = null
        private set

    /** Msat that can be sent now, as of the last balance refresh (the channels' outbound HTLC limit, which is below the balance). */
    @Volatile
    var sendableMsat: ULong = 0uL
        private set

    private var unlockedWalletId: String? = null
    private var unlockedSeed: ByteArray? = null
    private var lastKnownBalances: LnBalances = ZERO_LN_BALANCES
    private var eventJob: Job? = null

    fun enable(walletId: String): Job = scope.launch {
        mutex.withLock {
            val w = store.walletConfig(walletId)
            if (w != null && !isLightningWallet(w)) {
                _state.value = LnState.Off
                return@withLock
            }
            store.setLightningEnabled(walletId, true)
            val seed = unlockedSeed
            if (unlockedWalletId == walletId && seed != null) {
                startLocked(walletId, seed, takeover = false)
            }
        }
    }

    fun disable(walletId: String): Job = scope.launch {
        mutex.withLock {
            store.setLightningEnabled(walletId, false)
            if (unlockedWalletId == walletId || currentNode != null) {
                stopCurrentNodeLocked()
                lastKnownBalances = ZERO_LN_BALANCES
                _state.value = LnState.Off
            }
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

                if (unlockedWalletId != null && unlockedWalletId != walletId) {
                    stopCurrentNodeLocked()
                    lastKnownBalances = ZERO_LN_BALANCES
                }
                unlockedSeed?.fill(0)
                unlockedSeed = seedCopy
                unlockedWalletId = walletId

                if (!store.lightningEnabled(walletId)) {
                    stopCurrentNodeLocked()
                    _state.value = LnState.Off
                    return@withLock
                }

                startLocked(walletId, seedCopy, takeover = false)
            }
        }
    }

    fun onRelocked(): Job = scope.launch {
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
            if (!store.lightningEnabled(id)) return@withLock
            startLocked(id, seed, takeover = true)
        }
    }

    fun onConnectivityRestored(): Job = scope.launch {
        mutex.withLock {
            val id = unlockedWalletId ?: return@withLock
            val seed = unlockedSeed ?: return@withLock
            if (!store.lightningEnabled(id)) return@withLock
            val cur = _state.value
            if (cur is LnState.Error && cur.error !is LnFfiException.ActiveOnAnotherDevice) {
                startLocked(id, seed, takeover = false)
            }
        }
    }

    /** Arms an in-memory move of [amountSat]. Nothing is persisted: if the process dies, or the
     *  deposit is never sent, the intent is gone. The app calls [commitMove] once the deposit has
     *  actually been broadcast, which also survives a restart. */
    fun requestMoveToChannel(amountSat: ULong) {
        pendingMoveSat = amountSat
    }

    /** The deposit for a move to Lightning of [amountSat] was broadcast: remember it (also on
     *  disk), so the funds arriving in transit become a channel rather than being swept back,
     *  whether or not the app is restarted before they do. */
    suspend fun commitMove(amountSat: ULong) {
        pendingMoveSat = amountSat
        val id = unlockedWalletId ?: return
        runCatching { store.setPendingMove(id, PendingMove(amountSat.toLong(), System.currentTimeMillis())) }
    }

    /** The move intent: the in-memory one, else the persisted one if it is not stale. */
    private suspend fun activeMove(walletId: String): ULong? {
        pendingMoveSat?.let { return it }
        val persisted = runCatching { store.pendingMove(walletId) }.getOrNull() ?: return null
        if (System.currentTimeMillis() - persisted.atMs > PENDING_MOVE_MAX_AGE_MS || persisted.amountSat <= 0) {
            runCatching { store.setPendingMove(walletId, null) }
            return null
        }
        return persisted.amountSat.toULong()
    }

    /** Whether the transit balance covers opening a channel of [moveAmt]: the amount plus the anchor reserve ldk-node keeps back. */
    private fun coversMove(node: LnNodeApi, bal: LnBalances, moveAmt: ULong): Boolean =
        bal.onchainTransitSat >= moveAmt + runCatching { node.anchorReserveSat() }.getOrDefault(25_000uL)

    /** Publishes [bal] and [ch] as the node's state, with the amount that can be sent now. */
    private fun publish(node: LnNodeApi, bal: LnBalances, ch: LnChannelState) {
        lastKnownBalances = bal
        sendableMsat = runCatching { node.sendableMsat() }.getOrDefault(bal.lightningSat * 1_000uL)
        _state.value = LnState.Ready(bal, ch)
    }

    /**
     * Sends the transit balance back to the BTC wallet when it is an orphan: a closed channel's
     * funds, or a deposit nobody is waiting for. Never while a move to Lightning is pending: that
     * deposit is about to become a channel. Returns the new balances and channel state.
     */
    private suspend fun sweepOrphanTransit(
        node: LnNodeApi,
        walletId: String,
        bal: LnBalances,
        ch: LnChannelState,
    ): Pair<LnBalances, LnChannelState> {
        val provider = sweepAddressProvider
        if (provider == null || bal.onchainTransitSat == 0uL || ch != LnChannelState.NONE || activeMove(walletId) != null) {
            return bal to ch
        }
        val addr = runCatching { provider(walletId) }.getOrNull()
        if (addr.isNullOrBlank()) return bal to ch
        val swept = withContext(Dispatchers.IO) { runCatching { node.sweepTransit(addr) }.getOrNull() }
        if (swept != null) runCatching { onSweepBroadcast?.invoke(walletId) }
        return withContext(Dispatchers.IO) {
            runCatching { node.balances() }.getOrDefault(bal) to runCatching { node.channelState() }.getOrDefault(ch)
        }
    }

    private suspend fun consumeMove(walletId: String) {
        pendingMoveSat = null
        runCatching { store.setPendingMove(walletId, null) }
    }

    /** True when [walletId]'s seed is held here, i.e. [onUnlocked] already ran for it. */
    fun isUnlockedFor(walletId: String): Boolean = unlockedWalletId == walletId && unlockedSeed != null

    /** The payment history. Reads node files, so it runs on the IO dispatcher. */
    suspend fun payments(): List<LnPayment> = withContext(Dispatchers.IO) {
        currentNode?.let { runCatching { it.payments() }.getOrNull() }.orEmpty()
    }

    /** Re-reads balances and channel state, and opens the channel for a move whose deposit has
     *  arrived. Blocking node calls run on the IO dispatcher. A start, stop or close in progress
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
        val walletId = unlockedWalletId
        val moveAmt = walletId?.let { activeMove(it) }
        if (walletId != null && moveAmt != null && coversMove(node, bal, moveAmt) && lastInfo?.enabled != false) {
            // The intent is spent only once LDK accepted the open; after a failure (LSP briefly
            // unreachable, ...) it stays, so the next refresh retries instead of the deposit
            // being swept back. It expires on its own (PENDING_MOVE_MAX_AGE_MS).
            if (runCatching { node.openChannel(moveAmt) }.isSuccess) consumeMove(walletId)
            bal = runCatching { node.balances() }.getOrDefault(bal)
            ch = runCatching { node.channelState() }.getOrDefault(ch)
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
        stopCurrentNodeLocked()
        _state.value = LnState.Starting

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
            var bal = node.balances()
            var ch = node.channelState()

            val moveAmt = activeMove(walletId)
            if (moveAmt != null && coversMove(node, bal, moveAmt)) {
                val opened = withContext(Dispatchers.IO) {
                    runCatching { node.openChannel(moveAmt) }.isSuccess
                }
                if (opened) consumeMove(walletId)
                bal = node.balances()
                ch = node.channelState()
            } else {
                val (b2, c2) = sweepOrphanTransit(node, walletId, bal, ch)
                bal = b2
                ch = c2
            }

            publish(node, bal, ch)
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
            createdNode?.let { withContext(Dispatchers.IO) { runCatching { it.close() } } }
            currentNode = null
            _state.value = LnState.Error(e, lastKnownBalances)
        } catch (e: Exception) {
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
