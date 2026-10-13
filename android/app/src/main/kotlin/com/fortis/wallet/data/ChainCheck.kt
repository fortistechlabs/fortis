package com.fortis.wallet.data

import okhttp3.MediaType.Companion.toMediaTypeOrNull
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONObject
import java.io.IOException

/**
 * Chain facts from sources the edge doesn't control, for the moments where a
 * lie would cost coins: before the participant funds against the initiator's
 * HTLC, before the initiator reveals the preimage against the participant's,
 * and when the participant looks for that preimage. A hostile or compromised
 * edge (or anything between the phone and it) could otherwise fake a funding,
 * or hide a redeem. `chain` is "btc" or "xbt".
 */
interface ChainCheck {
    /** `txid:vout` if it exists and is unspent, with confirmations counted
     *  from the explorer's own tip; `null` if it is unknown or spent. Throws
     *  when no explorer answers. */
    suspend fun unspent(chain: String, txid: String, vout: Int): TxOutInfo?

    /** The raw hex of the transaction spending `txid:vout`, or `null` if it is
     *  unspent. Throws when no explorer answers. */
    suspend fun spendingTxHex(chain: String, txid: String, vout: Int): String?

    /** True once `txid:vout` is spent by a confirmed transaction. Throws when
     *  no explorer answers. */
    suspend fun spendConfirmed(chain: String, txid: String, vout: Int): Boolean

    /** Broadcast; returns the txid. Throws when no explorer accepts it. */
    suspend fun broadcast(chain: String, hex: String): String

    /** `txid:vout`'s value and script whether or not it is spent (no
     *  confirmations: 0); `null` if the explorer doesn't know it. Throws when
     *  no explorer answers. */
    suspend fun output(chain: String, txid: String, vout: Int): TxOutInfo?
}

/** Public Esplora APIs, first choice first: none is run by fortis. */
val EXPLORERS: Map<String, List<String>> = mapOf(
    "btc" to listOf("https://mempool.space/api", "https://blockstream.info/api"),
    "xbt" to listOf("https://mempool.kilombino.com/api", "https://mempool.guide/api"),
)

/**
 * [ChainCheck] over public Esplora explorers. Each question is answered by one
 * explorer, start to finish: the first that answers. A 404 is an answer ("not
 * there"); a connection failure, a server error or an answer about some other
 * transaction moves on to the next explorer.
 */
class ExplorerChainCheck(
    private val http: OkHttpClient,
    private val explorers: Map<String, List<String>> = EXPLORERS,
) : ChainCheck {
    private class Unanswered(msg: String) : IOException(msg)

    /** The body of a 2xx, `null` for a 404; otherwise [Unanswered]. */
    private suspend fun get(url: String): String? {
        val req = Request.Builder().url(url).build()
        val (code, text) = http.send(req) { r -> r.code to r.body?.string().orEmpty() }
        return when {
            code in 200..299 -> text
            code == 404 -> null
            else -> throw Unanswered("$code on $url: ${text.take(200)}")
        }
    }

    /** [ask] of the first explorer for [chain] that answers. */
    private suspend fun <T> first(chain: String, ask: suspend (String) -> T): T {
        val bases = explorers[chain].orEmpty()
        check(bases.isNotEmpty()) { "no independent explorer for $chain" }
        var last: Exception? = null
        for (base in bases) {
            try {
                return ask(base)
            } catch (e: kotlinx.coroutines.CancellationException) {
                throw e
            } catch (e: IOException) {
                last = e
            } catch (e: org.json.JSONException) {
                last = Unanswered("$base: bad answer: ${e.message}")
            }
        }
        throw last ?: Unanswered("no explorer answered")
    }

    /** `/tx/{txid}` — only if it really is about [txid]. */
    private suspend fun tx(base: String, txid: String): JSONObject? {
        val tx = get("$base/tx/$txid")?.let(::JSONObject) ?: return null
        if (!tx.optString("txid").equals(txid, ignoreCase = true)) throw Unanswered("$base answered about another transaction")
        return tx
    }

    override suspend fun unspent(chain: String, txid: String, vout: Int): TxOutInfo? = first(chain) { base ->
        val tx = tx(base, txid) ?: return@first null
        val out = tx.optJSONArray("vout")?.optJSONObject(vout) ?: return@first null
        val spend = get("$base/tx/$txid/outspend/$vout")?.let(::JSONObject) ?: throw Unanswered("$base: no outspend")
        if (spend.optBoolean("spent", true)) return@first null
        val status = tx.optJSONObject("status")
        val confs = if (status?.optBoolean("confirmed") == true) {
            val tip = get("$base/blocks/tip/height")?.trim()?.toLongOrNull() ?: throw Unanswered("$base: no tip")
            maxOf(0L, tip - status.getLong("block_height") + 1)
        } else 0L
        TxOutInfo(out.getLong("value"), out.getString("scriptpubkey"), confs)
    }

    override suspend fun output(chain: String, txid: String, vout: Int): TxOutInfo? = first(chain) { base ->
        val out = tx(base, txid)?.optJSONArray("vout")?.optJSONObject(vout) ?: return@first null
        TxOutInfo(out.getLong("value"), out.getString("scriptpubkey"), 0)
    }

    override suspend fun spendingTxHex(chain: String, txid: String, vout: Int): String? = first(chain) { base ->
        val spend = get("$base/tx/$txid/outspend/$vout")?.let(::JSONObject) ?: return@first null
        if (!spend.optBoolean("spent")) return@first null
        val by = spend.optString("txid").ifBlank { throw Unanswered("$base: a spend without a txid") }
        get("$base/tx/$by/hex")?.trim() ?: throw Unanswered("$base: no hex for $by")
    }

    override suspend fun spendConfirmed(chain: String, txid: String, vout: Int): Boolean = first(chain) { base ->
        val spend = get("$base/tx/$txid/outspend/$vout")?.let(::JSONObject) ?: return@first false
        spend.optBoolean("spent") && spend.optJSONObject("status")?.optBoolean("confirmed") == true
    }

    override suspend fun broadcast(chain: String, hex: String): String = first(chain) { base ->
        val req = Request.Builder().url("$base/tx")
            .post(hex.toRequestBody("text/plain".toMediaTypeOrNull()))
            .build()
        val (code, text) = http.send(req) { r -> r.code to r.body?.string().orEmpty() }
        if (code !in 200..299) throw Unanswered("$base refused the transaction ($code): ${text.take(200)}")
        text.trim()
    }
}
