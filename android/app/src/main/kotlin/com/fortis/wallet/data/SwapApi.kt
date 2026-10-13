package com.fortis.wallet.data

import com.fortis.wallet.HOSTED_EDGE
import okhttp3.MediaType.Companion.toMediaTypeOrNull
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONObject
import java.util.concurrent.TimeUnit

/**
 * `GET /swap/quote`. [referencePrice] is the edge's NeoxEX reference (sat of BTC
 * per XBT; `null` when it has none) and [referenceOk] whether it passes the old
 * quoting rules. [bestXbtToBtc] is the lowest open public XBT-for-BTC price and
 * [bestBtcToXbt] the highest open BTC-for-XBT price (`null` when none is open).
 */
data class Quote(
    val referencePrice: Long?,
    val referenceOk: Boolean,
    val bestXbtToBtc: Long?,
    val bestBtcToXbt: Long?,
    val feeBps: Int,
    val minBtcSat: Long,
    /** `null` when the edge sets no maximum. */
    val maxBtcSat: Long?,
)

data class OfferCreated(val offerId: String, val ownerSecret: String, val shareCode: String?)

/** `GET /swap/offers/{id}`: `matchId` is the offer's live (or final) match. */
data class OfferView(val offerId: String, val status: String, val matchId: String?, val expiresAt: Long)

/** `POST /swap/take/{code}`: our side of the new match, as a one-shot offer. */
data class Taken(val matchId: String, val ownerSecret: String, val offerId: String)

/** One listed public offer (`GET /swap/book`): its open amount on its own send
 *  chain, and both legs of that amount at its price. */
data class BookEntry(
    val id: String,
    val direction: String,
    val amountSat: Long,
    val priceSatPerXbt: Long,
    val btcSat: Long,
    val xbtSat: Long,
    val expiresAt: Long,
)

/** The offers a user could take: people selling XBT (`xbtToBtc`) and selling BTC. */
data class Book(val xbtToBtc: List<BookEntry>, val btcToXbt: List<BookEntry>)

internal fun parseBook(o: JSONObject): Book {
    fun side(k: String): List<BookEntry> {
        val a = o.optJSONArray(k) ?: return emptyList()
        return (0 until a.length()).map { i ->
            val e = a.getJSONObject(i)
            BookEntry(
                e.getString("id"), e.getString("direction"), e.getLong("amount_sat"),
                e.getLong("price_sat_per_xbt"), e.getLong("btc_sat"), e.getLong("xbt_sat"), e.getLong("expires_at"),
            )
        }
    }
    return Book(side("xbt_to_btc"), side("btc_to_xbt"))
}

data class Outpoint(val txid: String, val vout: Int)

/** One swap `POST /swap/recover` knows by its tag. [ownerSecret] is a fresh
 *  owner secret, set only when our own leg of [match] is funded. */
data class Recovered(
    val tag: String,
    /** `null` once the edge has purged the offer (the tag stays used). */
    val offerId: String?,
    val direction: String?,
    val match: RecoveredMatch?,
    val ownerSecret: String?,
)

data class RecoveredMatch(
    val id: String,
    val role: String,
    val status: String,
    val terms: JSONObject,
    val xbtFunding: Outpoint?,
    val btcFunding: Outpoint?,
)

/** The `found` list of a `POST /swap/recover` reply. */
internal fun parseRecovered(j: JSONObject): List<Recovered> {
    fun JSONObject.str(k: String): String? = if (has(k) && !isNull(k)) getString(k) else null
    fun JSONObject.outpoint(k: String): Outpoint? =
        optJSONObject(k)?.let { Outpoint(it.getString("txid"), it.getInt("vout")) }
    val a = j.optJSONArray("found") ?: return emptyList()
    return (0 until a.length()).map { i ->
        val f = a.getJSONObject(i)
        val m = f.optJSONObject("match")?.let {
            RecoveredMatch(
                it.getString("id"), it.getString("role"), it.getString("status"), it.getJSONObject("terms"),
                it.outpoint("xbt_funding"), it.outpoint("btc_funding"),
            )
        }
        Recovered(f.getString("tag"), f.str("offer_id"), f.str("direction"), m, f.str("owner_secret"))
    }
}

/** `GET /swap/matches/{id}`. The fundings are what each side announced with `funded`. */
data class MatchInfo(
    val terms: JSONObject,
    val role: String,
    val status: String,
    val participantFundBy: Long?,
    val xbtFunding: Outpoint? = null,
    val btcFunding: Outpoint? = null,
)

/** One relayed message. `from` is "initiator", "participant" or "edge". */
data class RelayMsg(val seq: Long, val from: String, val body: JSONObject)

class SwapHttpException(val code: Int, message: String) : Exception(message)

/** The edge's swap routes (`/swap/…`) (plus `/pricing`). An interface so the engine's
 *  JVM tests run against an in-memory edge; [EdgeSwapApi] is the real one. */
interface SwapApi {
    suspend fun quote(): Quote

    /** The commission address the edge advertises at `GET /pricing`; `null` if
     *  it advertises none. Throws when the edge can't be reached. */
    suspend fun pricingAddress(): String?

    /** `clientId` (hex, ≤ 64 digits) makes the POST idempotent: a repeat with the
     *  same one returns the same offer and owner secret, so a lost response or a
     *  crash before the record is promoted is recovered by posting again. */
    suspend fun createOffer(
        direction: String, amountSat: Long, priceSatPerXbt: Long, private: Boolean, clientId: String?,
        recoveryTag: String? = null,
    ): OfferCreated
    suspend fun offer(id: String, secret: String): OfferView
    suspend fun cancelOffer(id: String, secret: String): OfferView
    suspend fun take(code: String, recoveryTag: String? = null): Taken

    /** `POST /swap/recover`: the swaps these recovery tags (≤ 100) belong to.
     *  Only with [claim] does the edge hand out (new) owner secrets for funded
     *  ones — re-keying them, so whoever held the old secret loses it. */
    suspend fun recover(tags: List<String>, claim: Boolean): List<Recovered>

    /** The open public offers (`GET /swap/book`). */
    suspend fun book(): Book

    /** Take [amountSat] (on our send chain) of listed offer [offerId] at the
     *  [priceSatPerXbt] we were shown; a repeat with [clientId] takes once. */
    suspend fun takeOffer(
        offerId: String, amountSat: Long, priceSatPerXbt: Long, clientId: String, recoveryTag: String? = null,
    ): Taken
    suspend fun match(matchId: String, secret: String): MatchInfo

    /** The relay seq, or `null` for an idempotent repeat the edge didn't relay again. */
    suspend fun post(matchId: String, secret: String, body: JSONObject): Long?

    /** Messages after `after`. The edge long-polls (up to 25 s) when there are none. */
    suspend fun messages(matchId: String, secret: String, after: Long): List<RelayMsg>
    suspend fun watch(secret: String, body: JSONObject)

    /** Send this phone's swap-step trace for [offerId] to the edge's journal
     *  (`POST /swap/offers/{id}/log`, at most 50 lines a call). */
    suspend fun log(offerId: String, secret: String, lines: List<String>)
}

private val JSON = "application/json".toMediaTypeOrNull()

class EdgeSwapApi(
    http: OkHttpClient,
    base: String = HOSTED_EDGE,
    private val token: suspend () -> String,
) : SwapApi {
    private val base = base.trimEnd('/')

    /** The relay long-poll holds a request open for up to 25 s. */
    private val http = http.newBuilder().readTimeout(35, TimeUnit.SECONDS).build()

    private suspend fun call(
        method: String,
        path: String,
        secret: String? = null,
        body: JSONObject? = null,
    ): JSONObject {
        val auth = token()
        val b = Request.Builder().url(base + path).header("Authorization", "Bearer $auth")
        secret?.let { b.header("X-Swap-Secret", it) }
        when (method) {
            "GET" -> b.get()
            "DELETE" -> b.delete()
            else -> b.method(method, (body ?: JSONObject()).toString().toRequestBody(JSON))
        }
        return http.send(b.build()) { r ->
            val text = r.body?.string().orEmpty()
            if (!r.isSuccessful) {
                val msg = runCatching { JSONObject(text).getString("error") }.getOrNull() ?: text.ifBlank { "HTTP ${r.code}" }
                throw SwapHttpException(r.code, "$method $path: $msg")
            }
            if (text.isBlank()) JSONObject() else JSONObject(text)
        }
    }

    private fun JSONObject.longOrNull(k: String): Long? = if (has(k) && !isNull(k)) getLong(k) else null
    private fun JSONObject.strOrNull(k: String): String? = if (has(k) && !isNull(k)) getString(k) else null
    private fun JSONObject.outpoint(k: String): Outpoint? =
        optJSONObject(k)?.let { Outpoint(it.getString("txid"), it.getInt("vout")) }

    private fun offerView(o: JSONObject) =
        OfferView(o.getString("offer_id"), o.getString("status"), o.strOrNull("match_id"), o.optLong("expires_at"))

    override suspend fun quote(): Quote = call("GET", "/swap/quote").let {
        val best = it.optJSONObject("best")
        Quote(
            referencePrice = it.longOrNull("reference_price"),
            referenceOk = it.optBoolean("reference_ok", false),
            bestXbtToBtc = best?.longOrNull("xbt_to_btc"),
            bestBtcToXbt = best?.longOrNull("btc_to_xbt"),
            feeBps = it.getInt("fee_bps"),
            minBtcSat = it.getLong("min_btc_sat"),
            maxBtcSat = it.longOrNull("max_btc_sat"),
        )
    }

    /** Throws on a network or HTTP error (retry); `null` only when the edge
     *  answers without an address (a definite mismatch). */
    override suspend fun pricingAddress(): String? = call("GET", "/pricing").strOrNull("address")

    override suspend fun createOffer(
        direction: String, amountSat: Long, priceSatPerXbt: Long, private: Boolean, clientId: String?,
        recoveryTag: String?,
    ): OfferCreated {
        val body = JSONObject().put("direction", direction).put("amount_sat", amountSat)
            .put("price_sat_per_xbt", priceSatPerXbt).put("private", private)
        clientId?.let { body.put("client_id", it) }
        recoveryTag?.let { body.put("recovery_tag", it) }
        return call("POST", "/swap/offers", body = body).let {
            OfferCreated(it.getString("offer_id"), it.getString("owner_secret"), it.strOrNull("share_code"))
        }
    }

    override suspend fun offer(id: String, secret: String) = offerView(call("GET", "/swap/offers/$id", secret))

    override suspend fun log(offerId: String, secret: String, lines: List<String>) {
        call("POST", "/swap/offers/$offerId/log", secret, JSONObject().put("lines", org.json.JSONArray(lines)))
    }

    override suspend fun cancelOffer(id: String, secret: String) = offerView(call("DELETE", "/swap/offers/$id", secret))

    override suspend fun book(): Book = parseBook(call("GET", "/swap/book"))

    override suspend fun takeOffer(
        offerId: String, amountSat: Long, priceSatPerXbt: Long, clientId: String, recoveryTag: String?,
    ): Taken =
        call(
            "POST", "/swap/offers/$offerId/take",
            body = JSONObject().put("amount_sat", amountSat).put("price_sat_per_xbt", priceSatPerXbt).put("client_id", clientId)
                .also { b -> recoveryTag?.let { b.put("recovery_tag", it) } },
        ).let { Taken(it.getString("match_id"), it.getString("owner_secret"), it.getString("offer_id")) }

    override suspend fun take(code: String, recoveryTag: String?): Taken =
        call("POST", "/swap/take/$code", body = JSONObject().also { b -> recoveryTag?.let { b.put("recovery_tag", it) } }).let {
            Taken(it.getString("match_id"), it.getString("owner_secret"), it.getString("offer_id"))
        }

    override suspend fun recover(tags: List<String>, claim: Boolean): List<Recovered> =
        parseRecovered(
            call("POST", "/swap/recover", body = JSONObject().put("tags", org.json.JSONArray(tags)).put("claim", claim)),
        )

    override suspend fun match(matchId: String, secret: String): MatchInfo =
        call("GET", "/swap/matches/$matchId", secret).let {
            MatchInfo(
                terms = it.getJSONObject("terms"),
                role = it.getString("role"),
                status = it.getString("status"),
                participantFundBy = it.longOrNull("participant_fund_by"),
                xbtFunding = it.outpoint("xbt_funding"),
                btcFunding = it.outpoint("btc_funding"),
            )
        }

    override suspend fun post(matchId: String, secret: String, body: JSONObject): Long? =
        call("POST", "/swap/matches/$matchId/messages", secret, body).longOrNull("seq")

    override suspend fun messages(matchId: String, secret: String, after: Long): List<RelayMsg> {
        val arr = call("GET", "/swap/matches/$matchId/messages?after=$after", secret).optJSONArray("messages")
            ?: return emptyList()
        return (0 until arr.length()).map {
            val m = arr.getJSONObject(it)
            RelayMsg(m.getLong("seq"), m.getString("from"), m.getJSONObject("body"))
        }
    }

    override suspend fun watch(secret: String, body: JSONObject) {
        call("POST", "/swap/watch", secret, body)
    }
}
