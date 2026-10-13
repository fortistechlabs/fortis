package com.fortis.wallet.data

import com.fortis.wallet.HOSTED_EDGE
import okhttp3.MediaType.Companion.toMediaTypeOrNull
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray
import org.json.JSONObject

/** One unspent output. `confirmations` is 0 while it is in the mempool. */
data class TxOutInfo(val valueSat: Long, val scriptPubkeyHex: String, val confirmations: Long)

/** What the swap engine reads from (and sends to) the two chains. `chain` is
 *  "btc" or "xbt". */
interface ChainWatch {
    /** The output if it exists and is unspent (a mempool spend counts as spent). */
    suspend fun txout(chain: String, txid: String, vout: Int): TxOutInfo?

    /** The raw hex of the transaction spending the output, if any. BTC only:
     *  XBT has no spender index, so it is always `null` there. */
    suspend fun spendingTxHex(chain: String, txid: String, vout: Int): String?

    /** True once the output's spend is confirmed. (Added beyond the brief: how
     *  the engine knows a redeem or refund has settled.) */
    suspend fun spendConfirmed(chain: String, txid: String, vout: Int): Boolean

    /** A `(txid, vout)` paying exactly `amountSat` to `address`. BTC only (via
     *  Esplora `/address/{a}/txs`); XBT isn't address-indexed for P2WSH, so it is
     *  always `null` there and the engine relies on `SwapRecord.pendingFundingTxid`. */
    suspend fun findFunding(chain: String, address: String, amountSat: Long): Pair<String, Int>?

    /** `[target6, max(target1, 2 × target6)]`, sat/vB. */
    suspend fun feerates(chain: String): List<Long>

    /** Broadcast; returns the txid. */
    suspend fun broadcast(chain: String, hex: String): String
}

/**
 * The edge's fee estimates, kept between 1 sat/vB and [max] (the core's swap
 * ceiling), duplicates dropped, order kept: a hostile or broken edge's absurd
 * estimate can't reach a signed swap spend (the core caps those too).
 */
internal fun clampFeerates(rates: List<Long>, max: Long): List<Long> =
    rates.map { it.coerceIn(1L, max) }.distinct()

/**
 * [ChainWatch] over the hosted edge: `{edge}/xbt/…` (fortis-index) and
 * `{edge}/btc/…` (Esplora). HTLC spends pass the edge's fee check once both
 * parties' keys and the hashlock have been relayed.
 */
class EdgeChainWatch(
    private val http: OkHttpClient,
    private val token: suspend () -> String,
    base: String = HOSTED_EDGE,
) : ChainWatch {
    private val base = base.trimEnd('/')

    private suspend fun fetch(path: String): Pair<Int, String> {
        val auth = token()
        val req = Request.Builder().url(base + path).header("Authorization", "Bearer $auth").build()
        return http.send(req) { r -> r.code to r.body?.string().orEmpty() }
    }

    /** The body of a 2xx, `null` for a 404, an exception otherwise. */
    private suspend fun get(path: String): String? {
        val (code, text) = fetch(path)
        return when {
            code in 200..299 -> text
            code == 404 -> null
            else -> throw IllegalStateException("$code on $path: ${text.take(200)}")
        }
    }

    private suspend fun btcTip(): Long = get("/btc/blocks/tip/height")!!.trim().toLong()

    private suspend fun btcOutspend(txid: String, vout: Int): JSONObject? =
        get("/btc/tx/$txid/outspend/$vout")?.let(::JSONObject)

    override suspend fun txout(chain: String, txid: String, vout: Int): TxOutInfo? = when (chain) {
        "xbt" -> get("/xbt/txout/$txid/$vout")?.let(::JSONObject)?.let {
            TxOutInfo(it.getLong("value_sat"), it.getString("scriptpubkey_hex"), it.getLong("confirmations"))
        }
        "btc" -> {
            val tx = get("/btc/tx/$txid")?.let(::JSONObject)
            val out = tx?.optJSONArray("vout")?.optJSONObject(vout)
            if (tx == null || out == null || btcOutspend(txid, vout)?.optBoolean("spent") != false) {
                null
            } else {
                val status = tx.optJSONObject("status")
                val confs = if (status?.optBoolean("confirmed") == true) {
                    maxOf(0L, btcTip() - status.getLong("block_height") + 1)
                } else 0L
                TxOutInfo(out.getLong("value"), out.getString("scriptpubkey"), confs)
            }
        }
        else -> error("unknown chain $chain")
    }

    override suspend fun spendingTxHex(chain: String, txid: String, vout: Int): String? {
        if (chain != "btc") return null
        val o = btcOutspend(txid, vout) ?: return null
        if (!o.optBoolean("spent")) return null
        val spender = o.optString("txid").ifBlank { return null }
        return get("/btc/tx/$spender/hex")?.trim()
    }

    override suspend fun spendConfirmed(chain: String, txid: String, vout: Int): Boolean = when (chain) {
        // The confirmed-UTXO view: a 200 means not (yet) spent in a block. Only
        // fortis-index's own 404 means spent; any other 404 is an error.
        "xbt" -> fetch("/xbt/txout/$txid/$vout?mempool=false").let { (code, text) ->
            when {
                code in 200..299 -> false
                code == 404 && text.contains("spent or unknown") -> true
                else -> throw IllegalStateException("$code on /xbt/txout: ${text.take(200)}")
            }
        }
        "btc" -> btcOutspend(txid, vout)?.let {
            it.optBoolean("spent") && it.optJSONObject("status")?.optBoolean("confirmed") == true
        } ?: false
        else -> error("unknown chain $chain")
    }

    override suspend fun findFunding(chain: String, address: String, amountSat: Long): Pair<String, Int>? {
        if (chain != "btc") return null
        val txs = get("/btc/address/$address/txs")?.let(::JSONArray) ?: return null
        for (i in 0 until txs.length()) {
            val tx = txs.getJSONObject(i)
            val outs = tx.optJSONArray("vout") ?: continue
            for (v in 0 until outs.length()) {
                val o = outs.getJSONObject(v)
                if (o.optString("scriptpubkey_address") == address && o.optLong("value") == amountSat) {
                    return tx.getString("txid") to v
                }
            }
        }
        return null
    }

    override suspend fun feerates(chain: String): List<Long> {
        val f = JSONObject(get("/$chain/v1/fees/recommended") ?: error("no fee estimates for $chain"))
        // hourFee is the 6-block target (as the watchtower reads it), fastestFee the next block
        val t6 = maxOf(1L, Math.ceil(f.optDouble("hourFee", 1.0)).toLong())
        val t1 = maxOf(1L, Math.ceil(f.optDouble("fastestFee", 1.0)).toLong())
        return clampFeerates(listOf(t6, maxOf(t1, 2 * t6)), uniffi.wallet_ffi.maxSwapFeerateSatVb().toLong())
    }

    override suspend fun broadcast(chain: String, hex: String): String {
        val auth = token()
        val req = Request.Builder().url("$base/$chain/tx")
            .header("Authorization", "Bearer $auth")
            .post(hex.toRequestBody("text/plain".toMediaTypeOrNull()))
            .build()
        return http.send(req) { r ->
            val body = r.body?.string()?.trim().orEmpty()
            check(r.isSuccessful) { body.ifBlank { "the $chain node rejected the transaction (${r.code})" } }
            body
        }
    }
}
