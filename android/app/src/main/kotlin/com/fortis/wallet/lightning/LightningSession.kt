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
}

class InMemoryLightningStore(
    private val wallets: Map<String, WalletConfig> = emptyMap(),
    private val esplora: String = "$HOSTED_EDGE/btc",
    private val vss: String = "$HOSTED_EDGE/ln/vss",
) : LightningStore {
    private val enabledMap = ConcurrentHashMap<String, Boolean>()

    override suspend fun lightningEnabled(walletId: String): Boolean {
        val w = wallets[walletId]
        if (w != null && !showLightningToggle(w)) return false
        return enabledMap[walletId] ?: false
    }

    override suspend fun setLightningEnabled(walletId: String, on: Boolean) {
        val w = wallets[walletId]
        if (w != null && !showLightningToggle(w)) {
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
        (amountMsat / 100uL) + 50_000uL

    fun payments(): List<LnPayment> = emptyList()
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
    override fun nextEvent(timeoutMs: ULong): LnEventRecord? = inner.nextEvent(timeoutMs)
    override fun transitAddress(): String = inner.transitAddress()
    override fun openChannel(amountSat: ULong) = inner.openChannel(amountSat)
    override fun closeChannel() = inner.closeChannel()
    override fun forceCloseChannel() = inner.forceCloseChannel()
    override fun sweepTransit(toAddress: String): String? = inner.sweepTransit(toAddress)
    override fun channelState(): LnChannelState = inner.channelState()
    override fun close() = inner.close()
}

class LightningSession(
    private val scope: CoroutineScope,
    private val store: LightningStore,
    private val nodeFactory: (LnNodeConfig) -> LnNodeApi = { cfg -> FfiLnNode(LnNode(cfg)) },
    private val info: LnInfoApi,
    private val sweepAddressProvider: (suspend (walletId: String) -> String?)? = null,
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

    private var unlockedWalletId: String? = null
    private var unlockedSeed: ByteArray? = null
    private var lastKnownBalances: LnBalances = ZERO_LN_BALANCES
    private var eventJob: Job? = null

    fun enable(walletId: String): Job = scope.launch {
        mutex.withLock {
            val w = store.walletConfig(walletId)
            if (w != null && !showLightningToggle(w)) {
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
                if (w != null && !showLightningToggle(w)) {
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

    fun requestMoveToChannel(amountSat: ULong) {
        pendingMoveSat = amountSat
    }

    fun payments(): List<LnPayment> =
        currentNode?.let { runCatching { it.payments() }.getOrNull() }.orEmpty()

    fun refreshBalances() {
        val node = currentNode ?: return
        val cur = _state.value
        if (cur is LnState.Ready) {
            var bal = runCatching { node.balances() }.getOrNull() ?: return
            var ch = runCatching { node.channelState() }.getOrNull() ?: return
            val moveAmt = pendingMoveSat
            if (moveAmt != null && bal.onchainTransitSat >= moveAmt) {
                pendingMoveSat = null
                runCatching { node.openChannel(moveAmt) }
                bal = runCatching { node.balances() }.getOrDefault(bal)
                ch = runCatching { node.channelState() }.getOrDefault(ch)
            }
            lastKnownBalances = bal
            _state.value = LnState.Ready(bal, ch)
        }
    }

    /** Pay [target] and wait until `PaymentSent` or `PaymentFailed` (or terminal payment status). */
    suspend fun pay(target: LnTarget, amountMsat: ULong?): String {
        val node = currentNode ?: throw LnFfiException.Other("lightning not ready")
        return withContext(Dispatchers.IO) {
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
            refreshBalances()
            id
        }
    }

    /** Close the active channel (if any) and sweep settled transit funds to [toAddress]. */
    suspend fun closeAndSweep(toAddress: String): String? = mutex.withLock {
        val node = currentNode ?: throw LnFfiException.Other("lightning not ready")
        withContext(Dispatchers.IO) {
            if (node.channelState() != LnChannelState.NONE) {
                runCatching { node.closeChannel() }
            }
            val txid = if (node.balances().onchainTransitSat > 0uL) {
                node.sweepTransit(toAddress)
            } else {
                null
            }
            val bal = node.balances()
            val ch = node.channelState()
            lastKnownBalances = bal
            _state.value = LnState.Ready(bal, ch)
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
        val cfg = LnNodeConfig(
            bip39Seed = seed.copyOf(),
            network = w?.network ?: lnInfo.network,
            dataDir = store.dataDir(walletId),
            esploraUrl = store.esploraUrl(walletId),
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
                val n = nodeFactory(cfg)
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

            val moveAmt = pendingMoveSat
            if (moveAmt != null && bal.onchainTransitSat >= moveAmt) {
                pendingMoveSat = null
                withContext(Dispatchers.IO) {
                    runCatching { node.openChannel(moveAmt) }
                }
                bal = node.balances()
                ch = node.channelState()
            } else if (moveAmt == null && bal.onchainTransitSat > 0uL && ch == LnChannelState.NONE && sweepAddressProvider != null) {
                val addr = runCatching { sweepAddressProvider?.invoke(walletId) }.getOrNull()
                if (!addr.isNullOrBlank()) {
                    withContext(Dispatchers.IO) {
                        runCatching { node.sweepTransit(addr) }
                    }
                    bal = node.balances()
                    ch = node.channelState()
                }
            }

            lastKnownBalances = bal
            _state.value = LnState.Ready(bal, ch)
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
                val ev = runCatching { node.nextEvent(500uL) }.getOrNull() ?: continue
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
            if (ev is LnEventRecord.ChannelClosed && bal.onchainTransitSat > 0uL && ch == LnChannelState.NONE && sweepAddressProvider != null) {
                val addr = runCatching { sweepAddressProvider?.invoke(walletId) }.getOrNull()
                if (!addr.isNullOrBlank()) {
                    runCatching { node.sweepTransit(addr) }
                    bal = runCatching { node.balances() }.getOrDefault(bal)
                    ch = runCatching { node.channelState() }.getOrDefault(ch)
                }
            }
            lastKnownBalances = bal
            _state.value = LnState.Ready(bal, ch)
        }
    }
}
