package com.fortis.wallet

import android.app.Application
import android.content.Context
import com.fortis.wallet.data.ChainWatch
import com.fortis.wallet.data.EdgeChainWatch
import com.fortis.wallet.data.EdgeSwapApi
import com.fortis.wallet.data.NeoxEx
import com.fortis.wallet.data.SwapApi
import com.fortis.wallet.data.SwapStore
import com.fortis.wallet.swap.FfiSwapCrypto
import com.fortis.wallet.swap.Funder
import com.fortis.wallet.swap.SwapEngine
import com.fortis.wallet.swap.SwapNotifications
import com.fortis.wallet.swap.SwapRunner
import com.fortis.wallet.swap.SwapService
import com.fortis.wallet.swap.SwapWorker
import com.fortis.wallet.swap.due
import com.fortis.wallet.ui.localeWrap
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient
import java.net.Proxy
import java.util.concurrent.TimeUnit

class FortisApplication : Application() {
    override fun attachBaseContext(base: Context) = super.attachBaseContext(localeWrap(base))

    override fun onCreate() {
        super.onCreate()
        CrashReporter.install()
        SwapNotifications.createChannels(this)
        SwapWorker.schedule(this)
    }

    // --- swaps: one engine per process (a service locator), shared by the
    //     foreground service, the worker, the boot receiver and the UI ---

    /** Process-wide work that outlives any screen (unlock-time swap steps). */
    val appScope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    private val swapHttp: OkHttpClient by lazy {
        OkHttpClient.Builder()
            .proxy(Proxy.NO_PROXY)
            .connectTimeout(10, TimeUnit.SECONDS)
            .readTimeout(15, TimeUnit.SECONDS) // EdgeSwapApi raises it for the relay long-poll
            // A hard ceiling on any one call (above the long-poll's 35 s read):
            // a swap step can never wait on the network indefinitely.
            .callTimeout(60, TimeUnit.SECONDS)
            .build()
    }

    /** The only `SwapStore` over the `fortis_swaps` DataStore: never build a second one. */
    val swapStore: SwapStore by lazy { SwapStore(this) }

    /**
     * Who funds our HTLCs. The `WalletViewModel` sets it while it exists; its
     * funder answers `null` (→ NEED_FUNDING) while the app is locked. With no
     * ViewModel (the service or worker after process death) there is no
     * unlocked wallet, so the engine gets `null` too.
     */
    @Volatile
    var unlockedFunder: Funder? = null

    private val swapToken: suspend () -> String = { swapStore.edgeToken(swapHttp, HOSTED_EDGE) }

    /** The edge's `/swap/…` routes — the engine's, and the UI's (quote, offers, take). */
    val swapApi: SwapApi by lazy { EdgeSwapApi(swapHttp, HOSTED_EDGE, swapToken) }

    /** Chain reads for the swaps — the engine's, and the detail screen's confirmations. */
    val swapChain: ChainWatch by lazy { EdgeChainWatch(swapHttp, swapToken, HOSTED_EDGE) }

    /** Independent explorers (not the edge), for chain facts that cost coins. */
    val swapCheck: com.fortis.wallet.data.ChainCheck by lazy { com.fortis.wallet.data.ExplorerChainCheck(swapHttp) }

    /** This phone's own NeoxEX reference (`null` when NeoxEX has no data; throws when unreachable). */
    suspend fun swapReference(): NeoxEx.Reference? = NeoxEx.reference(swapHttp)

    val swapEngine: SwapEngine by lazy {
        SwapEngine(
            api = swapApi,
            store = swapStore,
            chain = swapChain,
            independent = swapCheck,
            crypto = { FfiSwapCrypto(it) },
            funder = Funder { r, htlc -> unlockedFunder?.fund(r, htlc) },
            now = { System.currentTimeMillis() / 1000 },
            notify = { r, e -> SwapNotifications.event(this, r, e) },
        )
    }

    /** Steps the active swaps concurrently, never one swap twice at once. */
    val swapRunner: SwapRunner by lazy {
        SwapRunner(
            active = { swapStore.due(System.currentTimeMillis() / 1000).map { it.id } },
            onTimeout = { id -> swapEngine.note(id, "step abandoned at the ${SwapRunner.STEP_TIMEOUT_MS / 1000} s limit; retried next pass") },
        ) { swapEngine.step(it) }
    }

    /**
     * The app was just unlocked: a swap waiting on NEED_FUNDING can fund now, so
     * step every active swap at once and (re)start the foreground service.
     */
    fun onAppUnlocked() {
        appScope.launch {
            if (runCatching { swapStore.due(System.currentTimeMillis() / 1000) }.getOrDefault(emptyList()).isEmpty()) return@launch
            swapRunner.launchActive(appScope)
            SwapService.start(this@FortisApplication)
        }
    }
}
