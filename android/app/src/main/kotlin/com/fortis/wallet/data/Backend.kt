package com.fortis.wallet.data

import uniffi.wallet_ffi.WalletUtxo

/** A hosted backend's service fee, from its status endpoint. Self-hosted backends
 *  don't report one, and no fee is charged. */
data class ServicePricing(val address: String, val bps: Int, val floorSat: Long, val capSat: Long)

data class ChainStatus(
    val blocks: ULong,
    val synced: Boolean,
    val via: String,
    val chain: String = "",
    val subversion: String = "",
    val scanningPct: Int? = null,
    val pricing: ServicePricing? = null,
    // true when serving from the public-explorer fallback, not the hosted service
    val degraded: Boolean = false,
)

/** `serverDegraded`: the edge said this balance came from its own Haskoin-
 *  failure fallback (confirmed history only, not a real live balance) —
 *  distinct from [ChainStatus.degraded] above, which means *this client*
 *  fell back to the public-explorer backend. Two different, independently
 *  true-or-false things that happen to share the word "degraded"; found
 *  live, 2026-09-28, that not distinguishing the server's own signal caused
 *  a stale/partial balance to get remembered as "last known" and shown on
 *  the next cold open. */
data class Balances(val confirmedSat: Long, val pendingSat: Long, val serverDegraded: Boolean = false)

data class HistoryEntry(
    val txid: String,
    val send: Boolean,
    val amountSat: Long,
    val confirmations: Long,
    val time: Long,
    /** Fee this wallet paid, sat. 0 for a receive (the sender paid it). */
    val feeSat: Long = 0L,
) {
    /** A send where nothing actually left the wallet — a self-transfer or a
     *  consolidation; only the fee was spent. */
    val internal: Boolean get() = send && amountSat == 0L
}

/** Same surface for both chain backends. Coin selection and signing happen in
 *  wallet-ffi; a backend only ever handles public data + finished transactions. */
interface Backend {
    val label: String
    suspend fun status(): ChainStatus
    suspend fun balances(): Balances
    suspend fun utxos(minConf: UInt): List<WalletUtxo>
    suspend fun feerateSatVb(confTarget: Int): ULong
    suspend fun history(count: Int): List<HistoryEntry>
    suspend fun broadcast(rawHex: String): String

    /** [balances] and [history] together, from one underlying fetch where the
     *  backend can manage it. Matters because a backend's "check anything new"
     *  step can be shared between the two (see `EsploraBackend.batchSnapshot`,
     *  where both are derived from the very same server response) — calling
     *  them as two separate suspend functions risks one succeeding and the
     *  other failing independently if the network hiccups in the gap between
     *  them, even though the data for both already arrived together. Default
     *  here is the old separate-calls behavior, for a backend with no shared
     *  fetch to exploit. */
    suspend fun balancesAndHistory(count: Int): Pair<Balances, List<HistoryEntry>> = balances() to history(count)

    /** USD per whole coin for this chain, or null if the backend has no price
     *  feed. Used only to show an approximate fiat value. */
    suspend fun price(): Double? = null

    /** Bulk-load everything for the wallet's addresses ahead of a scan, if the
     *  backend can (a no-op otherwise). Called once at the top of a refresh so
     *  the per-address loop that follows is served from cache. */
    suspend fun prewarm() {}

    /** The first receive-branch index that has never appeared on-chain, at or
     *  after [floor] — so the wallet can auto-advance past addresses it has
     *  already handed out. Backends that don't track address use return [floor]
     *  (the node-backed one advances its own descriptor). */
    suspend fun firstUnusedReceive(floor: Int): Int = floor
}
