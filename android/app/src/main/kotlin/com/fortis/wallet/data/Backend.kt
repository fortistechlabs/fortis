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

data class Balances(val confirmedSat: Long, val pendingSat: Long)

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
