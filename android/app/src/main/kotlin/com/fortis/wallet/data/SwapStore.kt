package com.fortis.wallet.data

import android.content.Context
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import okhttp3.OkHttpClient
import org.json.JSONArray
import org.json.JSONObject
import java.security.SecureRandom

/**
 * Where a swap is. Everything before [DONE] is still being driven.
 * [PROVISIONAL]: saved before its offer was posted; its id is the client id
 * until the edge's offer id replaces it (`SwapEngine.postOffer`).
 */
enum class Step {
    PROVISIONAL, WAITING_MATCH, MATCHED, NEED_FUNDING, FUNDED, THEY_FUNDED, REDEEMED,
    DONE, REFUNDING, REFUNDED, CANCELLED, FAILED;

    val active: Boolean get() = this !in setOf(DONE, REFUNDED, CANCELLED, FAILED)
}

/**
 * One swap on this phone, keyed by its offer id. The swap secrets are kept only
 * wrapped by [SwapKeystore] ([wrappedXbt], [wrappedBtc]; blanked once settled);
 * [sessionJson] is `SwapSession.to_json`, which holds no secrets.
 *
 * Beyond the brief's fields:
 * - [redeemAddress] / [refundAddress]: where our redeem (receive chain) and refund
 *   (send chain) pay — the session needs them when it is built.
 * - [termsJson]: the match's terms, once matched.
 * - [fundingVout], [theirTxid]/[theirVout]: our and their HTLC outpoints.
 * - [theirXbtKey]/[theirBtcKey]/[hashlock]: relayed counterparty data, kept until
 *   the session takes it.
 * - [redeemTxid]/[refundTxid]: what we broadcast.
 * - [marks]: one-shot milestones already done (see `SwapEngine`), so a re-run
 *   never repeats them.
 * - [pendingFundingHex], [rejectedFundings], [shareCode], [createdAt],
 *   [clientId], [privateOffer]: see below.
 */
data class SwapRecord(
    val id: String,
    val matchId: String? = null,
    val role: String? = null,
    val fromWalletId: String,
    val toWalletId: String,
    val sendChain: String,
    val amountSat: Long,
    val slippageBps: Int,
    val ownerSecret: String,
    val swapIndex: Int,
    val account: Int = 0,
    val sessionJson: String? = null,
    val wrappedXbt: String,
    val wrappedBtc: String,
    val relaySeq: Long = 0,
    val pendingFundingTxid: String? = null,
    val fundingTxid: String? = null,
    /** Our signed refund variants (low fee first). */
    val presigned: List<String> = emptyList(),
    val step: Step = Step.WAITING_MATCH,
    /** Unix seconds of the deadline the user is up against, if any. */
    val deadline: Long? = null,
    val error: String? = null,
    val redeemAddress: String,
    val refundAddress: String,
    val termsJson: String? = null,
    val fundingVout: Int? = null,
    val theirXbtKey: String? = null,
    val theirBtcKey: String? = null,
    val hashlock: String? = null,
    val theirTxid: String? = null,
    val theirVout: Int? = null,
    val redeemTxid: String? = null,
    val refundTxid: String? = null,
    val marks: Set<String> = emptySet(),
    /** The signed funding tx behind [pendingFundingTxid], saved with it before the
     *  broadcast, so an unconfirmed funding is re-sent as is — never re-planned. */
    val pendingFundingHex: String? = null,
    /** Fundings the node definitively rejected (inputs spent); still checked on
     *  chain in case one lands after all. */
    val rejectedFundings: List<String> = emptyList(),
    /** A private offer's share code, shown (with its QR) until it is taken. */
    val shareCode: String? = null,
    /** Unix seconds the swap was created on this phone; orders the swap list. */
    val createdAt: Long = 0,
    /** The offer POST's idempotency key; also the record's id while [Step.PROVISIONAL]. */
    val clientId: String? = null,
    /** The offer was posted (or is to be posted) private, with a share code. */
    val privateOffer: Boolean = false,
) {
    val recvChain: String get() = if (sendChain == "btc") "xbt" else "btc"

    /** Never prints the owner secret, the wrapped swap secrets or the signed
     *  funding tx (records end up in logs and crash reports). */
    override fun toString(): String = "SwapRecord" + toJson().apply {
        for (k in listOf("owner_secret", "wrapped_xbt", "wrapped_btc", "pending_funding_hex")) {
            if (has(k)) put(k, "<redacted>")
        }
    }

    fun toJson(): JSONObject = JSONObject().apply {
        put("id", id); put("from_wallet_id", fromWalletId); put("to_wallet_id", toWalletId)
        put("send_chain", sendChain); put("amount_sat", amountSat); put("slippage_bps", slippageBps)
        put("owner_secret", ownerSecret); put("swap_index", swapIndex); put("account", account)
        put("wrapped_xbt", wrappedXbt); put("wrapped_btc", wrappedBtc); put("relay_seq", relaySeq)
        put("presigned", JSONArray(presigned)); put("step", step.name)
        put("redeem_address", redeemAddress); put("refund_address", refundAddress)
        put("marks", JSONArray(marks.toList()))
        matchId?.let { put("match_id", it) }; role?.let { put("role", it) }
        sessionJson?.let { put("session", it) }; termsJson?.let { put("terms", it) }
        pendingFundingTxid?.let { put("pending_funding_txid", it) }
        pendingFundingHex?.let { put("pending_funding_hex", it) }
        if (rejectedFundings.isNotEmpty()) put("rejected_fundings", JSONArray(rejectedFundings))
        fundingTxid?.let { put("funding_txid", it) }; fundingVout?.let { put("funding_vout", it) }
        deadline?.let { put("deadline", it) }; error?.let { put("error", it) }
        theirXbtKey?.let { put("their_xbt", it) }; theirBtcKey?.let { put("their_btc", it) }
        hashlock?.let { put("hashlock", it) }
        shareCode?.let { put("share_code", it) }
        if (createdAt != 0L) put("created_at", createdAt)
        clientId?.let { put("client_id", it) }
        if (privateOffer) put("private", true)
        theirTxid?.let { put("their_txid", it) }; theirVout?.let { put("their_vout", it) }
        redeemTxid?.let { put("redeem_txid", it) }; refundTxid?.let { put("refund_txid", it) }
    }

    companion object {
        private fun JSONObject.str(k: String): String? = if (has(k) && !isNull(k)) getString(k) else null
        private fun JSONObject.long(k: String): Long? = if (has(k) && !isNull(k)) getLong(k) else null
        private fun JSONObject.int(k: String): Int? = if (has(k) && !isNull(k)) getInt(k) else null
        private fun JSONArray?.strings(): List<String> =
            if (this == null) emptyList() else (0 until length()).map { getString(it) }

        fun fromJson(o: JSONObject) = SwapRecord(
            id = o.getString("id"),
            matchId = o.str("match_id"),
            role = o.str("role"),
            fromWalletId = o.getString("from_wallet_id"),
            toWalletId = o.getString("to_wallet_id"),
            sendChain = o.getString("send_chain"),
            amountSat = o.getLong("amount_sat"),
            slippageBps = o.getInt("slippage_bps"),
            ownerSecret = o.getString("owner_secret"),
            swapIndex = o.getInt("swap_index"),
            account = o.optInt("account", 0),
            sessionJson = o.str("session"),
            wrappedXbt = o.optString("wrapped_xbt"),
            wrappedBtc = o.optString("wrapped_btc"),
            relaySeq = o.optLong("relay_seq", 0),
            pendingFundingTxid = o.str("pending_funding_txid"),
            fundingTxid = o.str("funding_txid"),
            presigned = o.optJSONArray("presigned").strings(),
            step = runCatching { Step.valueOf(o.getString("step")) }.getOrDefault(Step.FAILED),
            deadline = o.long("deadline"),
            error = o.str("error"),
            redeemAddress = o.getString("redeem_address"),
            refundAddress = o.getString("refund_address"),
            termsJson = o.str("terms"),
            fundingVout = o.int("funding_vout"),
            theirXbtKey = o.str("their_xbt"),
            theirBtcKey = o.str("their_btc"),
            hashlock = o.str("hashlock"),
            theirTxid = o.str("their_txid"),
            theirVout = o.int("their_vout"),
            redeemTxid = o.str("redeem_txid"),
            refundTxid = o.str("refund_txid"),
            marks = o.optJSONArray("marks").strings().toSet(),
            pendingFundingHex = o.str("pending_funding_hex"),
            rejectedFundings = o.optJSONArray("rejected_fundings").strings(),
            shareCode = o.str("share_code"),
            createdAt = o.optLong("created_at", 0),
            clientId = o.str("client_id"),
            privateOffer = o.optBoolean("private", false),
        )
    }
}

/** One JSON string, read and atomically rewritten. DataStore on the phone; an
 *  in-memory string in JVM tests. */
interface SwapBlob {
    suspend fun read(): String?
    suspend fun update(f: (String?) -> String)
}

private val Context.swapData by preferencesDataStore("fortis_swaps")

private class DataStoreBlob(private val ctx: Context) : SwapBlob {
    private val key = stringPreferencesKey("swaps")
    override suspend fun read(): String? = ctx.swapData.data.first()[key]
    override suspend fun update(f: (String?) -> String) {
        ctx.swapData.edit { it[key] = f(it[key]) }
    }
}

private val secureRandom = SecureRandom()

/**
 * Every swap on this phone, as one JSON document like [Store]'s wallet list:
 * `{"records":[…], "swap_indices":[…], "token": "…"}`.
 */
class SwapStore(
    private val blob: SwapBlob,
    /** A uniformly random swap index in `[0, 2^31)`; a parameter only so tests can script it. */
    private val draw: () -> Int = { secureRandom.nextInt() and Int.MAX_VALUE },
) {
    constructor(ctx: Context) : this(DataStoreBlob(ctx.applicationContext))

    private val lock = Mutex()

    private fun parse(raw: String?): JSONObject = raw?.let { runCatching { JSONObject(it) }.getOrNull() } ?: JSONObject()

    private fun records(doc: JSONObject): List<SwapRecord> {
        val arr = doc.optJSONArray("records") ?: return emptyList()
        return (0 until arr.length()).map { SwapRecord.fromJson(arr.getJSONObject(it)) }
    }

    private suspend fun edit(f: (JSONObject) -> Unit) = lock.withLock {
        blob.update { raw -> parse(raw).also(f).toString() }
    }

    suspend fun all(): List<SwapRecord> = records(parse(blob.read()))

    suspend fun get(id: String): SwapRecord? = all().firstOrNull { it.id == id }

    /** Insert or replace by id. */
    suspend fun put(r: SwapRecord) = edit { doc ->
        val list = records(doc).filter { it.id != r.id } + r
        doc.put("records", JSONArray().apply { list.forEach { put(it.toJson()) } })
    }

    /** Apply [f] to the record [id] if it is still there (in one atomic edit);
     *  the updated record, or `null` if there is none. */
    suspend fun update(id: String, f: (SwapRecord) -> SwapRecord): SwapRecord? {
        var out: SwapRecord? = null
        edit { doc ->
            val list = records(doc)
            val r = list.firstOrNull { it.id == id } ?: return@edit
            val next = f(r).also { out = it }
            val rest = list.filter { it.id != id && it.id != next.id }
            doc.put("records", JSONArray().apply { (rest + next).forEach { put(it.toJson()) } })
        }
        return out
    }

    /**
     * Replace the provisional record [clientId] with [f] of it (its offer id, owner
     * secret, …) in one atomic edit. Promoting twice (the app and the engine
     * racing) is harmless: once the provisional record is gone, or a record
     * with the new id already exists, nothing is overwritten and the promoted
     * record is returned. `null` if there is neither.
     */
    suspend fun promote(clientId: String, f: (SwapRecord) -> SwapRecord): SwapRecord? {
        var out: SwapRecord? = null
        edit { doc ->
            val list = records(doc)
            val p = list.firstOrNull { it.id == clientId && it.step == Step.PROVISIONAL }
            if (p == null) {
                out = list.firstOrNull { it.clientId == clientId && it.step != Step.PROVISIONAL }
                return@edit
            }
            val next = f(p)
            val existing = list.firstOrNull { it.id == next.id }
            out = existing ?: next
            val rest = list.filter { it.id != clientId && it.id != next.id }
            doc.put("records", JSONArray().apply { (rest + (existing ?: next)).forEach { put(it.toJson()) } })
        }
        return out
    }

    /** The record with [id], or the one promoted from the provisional record [id]. */
    suspend fun find(id: String): SwapRecord? = all().let { all ->
        all.firstOrNull { it.id == id } ?: all.firstOrNull { it.clientId == id }
    }

    suspend fun remove(id: String) = edit { doc ->
        val list = records(doc).filter { it.id != id }
        doc.put("records", JSONArray().apply { list.forEach { put(it.toJson()) } })
    }

    /** Swaps still being driven (before DONE / REFUNDED / CANCELLED / FAILED). */
    suspend fun active(): List<SwapRecord> = all().filter { it.step.active }

    /**
     * The swap index for a new swap from `walletId`: drawn uniformly at random
     * in `[0, 2^31)` with [SecureRandom], redrawn while it collides with any
     * record's index or any index handed out before (on any wallet: a clone of
     * the same seed on the other chain derives the same keys per index). The
     * caller saves it in the record.
     *
     * Not a counter: swap keys — and the initiator's preimage, the HMAC of its
     * XBT swap key — are seed-deterministic per index, and this store does not
     * survive a reinstall or clear-data (no backup). A counter would restart at
     * 0 and reuse a hashlock whose preimage an earlier swap already published,
     * letting a participant take the XBT leg without funding. A random index
     * repeats with probability ~n/2^31.
     */
    suspend fun nextSwapIndex(@Suppress("UNUSED_PARAMETER") walletId: String): Int {
        var out = 0
        edit { doc ->
            val issued = doc.optJSONArray("swap_indices") ?: JSONArray()
            val taken = records(doc).map { it.swapIndex }.toMutableSet()
            for (i in 0 until issued.length()) taken += issued.getInt(i)
            do {
                out = draw()
            } while (out in taken)
            doc.put("swap_indices", issued.put(out))
        }
        return out
    }

    /** The install token used for the `/swap/…` routes (strikes are counted against it). */
    suspend fun token(): String? = parse(blob.read()).optString("token").ifBlank { null }

    suspend fun setToken(token: String) = edit { it.put("token", token) }

    /** The stored token, minted once with `POST {edge}/register` if there is none. */
    suspend fun edgeToken(http: OkHttpClient, base: String): String =
        token() ?: edgeRegister(http, base).also { setToken(it) }
}
