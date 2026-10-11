package com.fortis.wallet.swap

import com.fortis.wallet.data.ChainWatch
import com.fortis.wallet.data.MatchInfo
import com.fortis.wallet.data.OfferCreated
import com.fortis.wallet.data.OfferView
import com.fortis.wallet.data.Outpoint
import com.fortis.wallet.data.Quote
import com.fortis.wallet.data.RelayMsg
import com.fortis.wallet.data.Step
import com.fortis.wallet.data.SwapApi
import com.fortis.wallet.data.SwapBlob
import com.fortis.wallet.data.SwapRecord
import com.fortis.wallet.data.SwapHttpException
import com.fortis.wallet.data.SwapStore
import com.fortis.wallet.data.Taken
import com.fortis.wallet.data.TxOutInfo
import kotlinx.coroutines.runBlocking
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.wallet_ffi.FfiPresignedRedeem
import uniffi.wallet_ffi.HtlcInfo
import uniffi.wallet_ffi.SwapPubkeys
import java.io.IOException

private const val MATCHED_AT = 1_800_000_000L
private const val PRICE = 1_120_000L
private const val XBT_SAT = 10_000_000L
private const val BTC_SAT = 112_000L // XBT_SAT * PRICE / 1e8
private const val FEE_ADDR = "bcrt1qfee"
private val PREIMAGE = "11".repeat(32)
private const val ACCEPTED_MARK = "accepted"
private const val XBT_LOCKTIME = MATCHED_AT + 144 * 3600
private const val BTC_LOCKTIME = MATCHED_AT + 24 * 3600
private const val ACCEPT_BY = MATCHED_AT + 120
private const val INITIATOR_FUND_BY = ACCEPT_BY + 1800

private fun hashOf(preimage: String) = "H($preimage)"

// ---------------------------------------------------------------------------
// fakes
// ---------------------------------------------------------------------------

class MemoryBlob : SwapBlob {
    private var v: String? = null
    override suspend fun read(): String? = synchronized(this) { v }
    override suspend fun update(f: (String?) -> String) = synchronized(this) { v = f(v) }
}

/** The edge's swap routes, in memory, shared by both phones. */
class FakeApi(private val chain: FakeChain, private val clock: () -> Long) : SwapApi {
    class Offer(
        val id: String, val secret: String, val direction: String, val amount: Long,
        val clientId: String? = null, val private: Boolean = false,
    ) {
        var status = "open"
        var matchId: String? = null
    }

    class Match(
        val id: String,
        val terms: JSONObject,
        val initiatorSecret: String,
        val participantSecret: String,
        val initiatorOffer: String,
        val participantOffer: String,
    ) {
        var status = "proposed"
        val accepted = mutableSetOf<String>()
        var participantFundBy: Long? = null
        var xbtFunding: Outpoint? = null
        var btcFunding: Outpoint? = null
        val msgs = mutableListOf<RelayMsg>()
        val keys = mutableMapOf<String, String>()
        var hashlock: String? = null
    }

    val offers = mutableMapOf<String, Offer>()
    val matches = mutableMapOf<String, Match>()
    val watches = mutableListOf<Pair<String, JSONObject>>()
    var pricing: String? = FEE_ADDR

    /** `GET /pricing` is unreachable (a transient error, not an answer). */
    var pricingDown = false

    /** `funded` posts fail in transit. */
    var failFunded = false

    /** Watch posts matching this fail in transit. */
    var failWatch: (JSONObject) -> Boolean = { false }

    /** Txids the edge's own chain reader doesn't show yet. */
    val blind = mutableSetOf<String>()

    /** `POST /swap/offers` never reaches the edge. */
    var createDown = false

    /** `POST /swap/offers` reaches the edge (the offer is made) but the
     *  response is lost (a timeout, or the process dies before reading it). */
    var dropCreateResponse = false

    /** `POST /swap/offers` is refused with this status (no offer made). */
    var refuseCreate: Int? = null

    /** Every `POST /swap/offers` call that reached the edge. */
    var createCalls = 0

    /** Every `DELETE /swap/offers/{id}` call, by offer id. */
    val cancelCalls = mutableListOf<String>()

    /** Matches the edge no longer has (purged a week after they ended): 404. */
    val goneMatches = mutableSetOf<String>()

    /** Offers the edge no longer has: 404. */
    val goneOffers = mutableSetOf<String>()
    private var n = 0

    /**
     * As `matcher::tick` cancelling a match on a lapsed deadline: each side's
     * offer goes back on the book (`reopen…`) or off it for good.
     * - accept_by passed: both reopen;
     * - initiator_fund_by passed: the initiator's is cancelled, the participant's reopens;
     * - participant_fund_by passed: both are cancelled.
     */
    fun lapse(m: Match, reopenInitiator: Boolean, reopenParticipant: Boolean) {
        m.status = "cancelled"
        for ((id, reopen) in listOf(m.initiatorOffer to reopenInitiator, m.participantOffer to reopenParticipant)) {
            val o = offers.getValue(id)
            if (o.matchId == m.id) {
                o.matchId = null
                o.status = if (reopen) "open" else "cancelled"
            } else if (o.status == "open" && !reopen) {
                o.status = "cancelled"
            }
        }
    }

    /** `participantRemainder`: the BTC offer is only partly filled, so (as on the
     *  edge) it stays open with its remainder, and its live match is found via
     *  the offer's match list. */
    fun pair(xbtOffer: String, btcOffer: String, participantRemainder: Boolean = false): Match {
        val (a, b) = offers.getValue(xbtOffer) to offers.getValue(btcOffer)
        val id = "match-${++n}"
        val terms = JSONObject()
            .put("match_id", id).put("network", "regtest")
            .put("xbt_sat", XBT_SAT).put("btc_sat", BTC_SAT).put("price_sat_per_xbt", PRICE)
            .put("fee_bps", 50).put("fee_address", FEE_ADDR).put("matched_at", MATCHED_AT)
            .put("xbt_locktime", XBT_LOCKTIME).put("btc_locktime", BTC_LOCKTIME)
            .put("xbt_confs", 6).put("btc_confs", 2)
            .put("accept_by", ACCEPT_BY).put("initiator_fund_by", INITIATOR_FUND_BY)
        val m = Match(id, terms, a.secret, b.secret, a.id, b.id)
        matches[id] = m
        a.matchId = id; a.status = "matched"
        if (!participantRemainder) {
            b.matchId = id; b.status = "matched"
        }
        return m
    }

    private fun roleOf(m: Match, secret: String) = when (secret) {
        m.initiatorSecret -> "initiator"
        m.participantSecret -> "participant"
        else -> error("403")
    }

    fun bodies(matchId: String, from: String, type: String) =
        matches.getValue(matchId).msgs.filter { it.from == from && it.body.getString("type") == type }.map { it.body }

    override suspend fun quote() = Quote(PRICE, 50, 80_000, 1_000_000)
    override suspend fun pricingAddress(): String? {
        if (pricingDown) throw IOException("GET /pricing: connection reset")
        return pricing
    }

    override suspend fun createOffer(
        direction: String, amountSat: Long, slippageBps: Int, private: Boolean, clientId: String?,
    ): OfferCreated {
        if (createDown) throw IOException("POST /swap/offers: connection refused")
        createCalls++
        // as the edge: a repeat with the same client id returns the same offer, unmatched again
        val o = clientId?.let { c -> offers.values.firstOrNull { it.clientId == c } } ?: run {
            refuseCreate?.let { throw SwapHttpException(it, "POST /swap/offers: refused") }
            val id = "offer-${++n}"
            Offer(id, "secret-$id", direction, amountSat, clientId, private).also { offers[id] = it }
        }
        if (dropCreateResponse) throw IOException("POST /swap/offers: timeout")
        return OfferCreated(o.id, o.secret, if (o.private) "CODE-${o.id}" else null)
    }

    override suspend fun offer(id: String, secret: String): OfferView {
        if (id in goneOffers) throw SwapHttpException(404, "no such offer")
        val o = offers.getValue(id)
        check(o.secret == secret) { "403" }
        // as the edge: the final match, else the latest live one
        val live = o.matchId ?: matches.values.lastOrNull {
            (it.initiatorOffer == id || it.participantOffer == id) && it.status != "cancelled"
        }?.id
        return OfferView(o.id, o.status, live, MATCHED_AT + 86_400)
    }

    override suspend fun cancelOffer(id: String, secret: String): OfferView {
        cancelCalls += id
        val o = offers.getValue(id)
        check(o.secret == secret) { "403" }
        if (o.status != "open") throw SwapHttpException(409, "only an unmatched offer can be cancelled")
        o.status = "cancelled"
        return OfferView(o.id, o.status, o.matchId, MATCHED_AT + 86_400)
    }
    override suspend fun take(code: String, slippageBps: Int): Taken = error("unused")

    override suspend fun match(matchId: String, secret: String): MatchInfo {
        if (matchId in goneMatches) throw SwapHttpException(404, "no such match")
        val m = matches.getValue(matchId)
        val role = roleOf(m, secret)
        // the watchtower's participant clock: XBT funding with 6 confirmations
        val f = m.xbtFunding
        if (m.status == "initiator_funded" && m.participantFundBy == null && f != null &&
            (chain.txout("xbt", f.txid, f.vout)?.confirmations ?: 0) >= 6
        ) {
            val latest = BTC_LOCKTIME - minOf(12 * 3600L, (BTC_LOCKTIME - MATCHED_AT) / 2)
            val by = minOf(clock() + 7200, latest)
            m.participantFundBy = by
            m.msgs += RelayMsg(m.msgs.size + 1L, "edge", JSONObject().put("type", "deadline").put("participant_fund_by", by))
        }
        return MatchInfo(m.terms, role, m.status, m.participantFundBy, m.xbtFunding, m.btcFunding)
    }

    override suspend fun post(matchId: String, secret: String, body: JSONObject): Long? {
        val m = matches.getValue(matchId)
        val role = roleOf(m, secret)
        when (body.getString("type")) {
            "accept" -> {
                check(m.status == "proposed" || m.status == "accepted") { "409" }
                m.accepted += role
                if (m.accepted.size == 2) m.status = "accepted"
            }
            "decline" -> {
                // matcher::on_decline: only while proposed; both offers go back on the book
                if (m.status != "proposed" || m.xbtFunding != null) {
                    throw SwapHttpException(409, "match can no longer be declined")
                }
                m.status = "cancelled"
                listOf(m.initiatorOffer, m.participantOffer).map(offers::getValue).forEach {
                    if (it.matchId == m.id) {
                        it.matchId = null; it.status = "open"
                    }
                }
            }
            "keys" -> {
                // the edge takes keys in any live status — including `proposed`, before any accept
                if (m.status == "cancelled") throw SwapHttpException(409, "match is over")
                val k = body.getString("xbt") + "/" + body.getString("btc")
                m.keys[role]?.let { check(it == k) { "409" }; return null }
                m.keys[role] = k
            }
            "hashlock" -> {
                check(role == "initiator") { "403" }
                m.hashlock?.let { check(it == body.getString("hex")) { "409" }; return null }
                m.hashlock = body.getString("hex")
            }
            "funded" -> {
                if (failFunded) throw IOException("POST funded: connection reset")
                val op = Outpoint(body.getString("txid"), body.getInt("vout"))
                if (role == "initiator") {
                    check(body.getString("chain") == "xbt") { "400" }
                    // a refund watch may have recorded it first: `funded` still moves the match on
                    m.xbtFunding?.let { check(it == op) { "409" }; if (m.status != "accepted") return null }
                    check(m.status == "accepted") { "409 status ${m.status}" }
                    m.xbtFunding = op; m.status = "initiator_funded"
                } else {
                    check(body.getString("chain") == "btc") { "400" }
                    m.btcFunding?.let { check(it == op) { "409" }; if (m.status != "initiator_funded") return null }
                    check(m.status == "initiator_funded") { "409 status ${m.status}" }
                    m.btcFunding = op; m.status = "participant_funded"
                }
            }
            else -> error("400")
        }
        val seq = m.msgs.size + 1L
        m.msgs += RelayMsg(seq, role, body)
        return seq
    }

    override suspend fun messages(matchId: String, secret: String, after: Long): List<RelayMsg> {
        val m = matches.getValue(matchId)
        roleOf(m, secret)
        return m.msgs.filter { it.seq > after }
    }

    /**
     * As the edge's `watch` + `record_leg_funding`: the outpoint must be the
     * leg's recorded funding — or, with none recorded, both keys and the
     * hashlock relayed and the chain showing exactly the leg's HTLC (script
     * and amount), whatever the match status. That outpoint is then recorded
     * as the leg's funding; the status is left alone.
     */
    override suspend fun watch(secret: String, body: JSONObject) {
        if (failWatch(body)) throw IOException("POST /swap/watch: connection reset")
        val m = matches.getValue(body.getString("match_id"))
        val role = roleOf(m, secret)
        val leg = body.getString("chain")
        val own = leg == if (role == "initiator") "xbt" else "btc"
        if (own != (body.getString("kind") == "refund")) throw SwapHttpException(403, "refund your own HTLC, redeem the counterparty's")
        val op = body.getJSONObject("htlc_outpoint").let { Outpoint(it.getString("txid"), it.getInt("vout")) }
        val recorded = if (leg == "xbt") m.xbtFunding else m.btcFunding
        if (recorded != op) {
            if (recorded != null || !own) throw SwapHttpException(409, "not this match's recorded funding")
            val out = if (op.txid in blind) null else chain.txout(leg, op.txid, op.vout)
            val amount = m.terms.getLong(if (leg == "xbt") "xbt_sat" else "btc_sat")
            // FakeCrypto's HTLC script for the leg, built only from both keys and the hashlock
            val ready = m.keys.size == 2 && m.hashlock != null
            if (!ready || out == null || out.scriptPubkeyHex != "spk-$leg-htlc" || out.valueSat != amount) {
                throw SwapHttpException(409, "not this match's HTLC on that chain")
            }
            if (leg == "xbt") m.xbtFunding = op else m.btcFunding = op
        }
        watches += role to body
    }
}

/** Both chains. Fake transactions are `kind|chain|txid|vout|…` strings. */
class FakeChain : ChainWatch {
    class Out(val address: String, val value: Long, val spk: String) {
        var confs = 0L
        var spender: String? = null
        var spendConfs = 0L
    }

    val outs = mutableMapOf<String, Out>()
    val broadcasts = mutableListOf<Pair<String, String>>()

    /** Funding transactions sent (by the funder or re-sent by the engine). */
    val fundingBroadcasts = mutableListOf<String>()

    /** Every funding broadcast tried, including the ones the node refused. */
    val fundingAttempts = mutableListOf<String>()
    var fees = listOf(2L, 10L)

    /** Every broadcast fails in transit (no answer from the node). */
    var down = false

    /** The node definitively rejects funding broadcasts with this message. */
    var reject: String? = null

    /** Txids the indexer doesn't show yet (lag). */
    val hidden = mutableSetOf<String>()
    private var n = 0

    private fun key(chain: String, txid: String, vout: Int) = "$chain:$txid:$vout"

    private fun feeOf(hex: String) = Regex("fee=(\\d+)").find(hex)?.groupValues?.get(1)?.toLong() ?: 0L

    fun add(chain: String, txid: String, vout: Int, value: Long, spk: String, address: String) {
        outs[key(chain, txid, vout)] = Out(address, value, spk)
    }

    fun mine(blocks: Int = 1) = outs.values.forEach {
        it.confs += blocks
        if (it.spender != null) it.spendConfs += blocks
    }

    fun spenderOf(chain: String, txid: String, vout: Int) = outs[key(chain, txid, vout)]?.spender

    override suspend fun txout(chain: String, txid: String, vout: Int): TxOutInfo? =
        if (txid in hidden) null else outs[key(chain, txid, vout)]?.takeIf { it.spender == null }?.let { TxOutInfo(it.value, it.spk, it.confs) }

    override suspend fun spendingTxHex(chain: String, txid: String, vout: Int): String? =
        if (chain == "btc") outs[key(chain, txid, vout)]?.spender else null

    override suspend fun spendConfirmed(chain: String, txid: String, vout: Int): Boolean =
        outs[key(chain, txid, vout)]?.let { it.spender != null && it.spendConfs > 0 } ?: false

    override suspend fun findFunding(chain: String, address: String, amountSat: Long): Pair<String, Int>? {
        if (chain != "btc") return null
        return outs.entries.firstOrNull { (k, o) -> k.startsWith("btc:") && k.split(":")[1] !in hidden && o.address == address && o.value == amountSat }
            ?.key?.split(":")?.let { it[1] to it[2].toInt() }
    }

    override suspend fun feerates(chain: String): List<Long> = fees

    override suspend fun broadcast(chain: String, hex: String): String {
        if (down) throw IOException("POST /$chain/tx: 503")
        val p = hex.split("|")
        check(p[1] == chain) { "wrong chain" }
        if (p[0] == "funding") {
            // funding|chain|txid|htlc value|htlc spk|htlc address: commission at vout 0, HTLC at 1
            fundingAttempts += hex
            reject?.let { throw IllegalStateException(it) }
            if (key(chain, p[2], 1) !in outs) {
                add(chain, p[2], 0, 560, "spk-fee", FEE_ADDR)
                add(chain, p[2], 1, p[3].toLong(), p[4], p[5])
            }
            fundingBroadcasts += hex
            return p[2]
        }
        val o = outs[key(chain, p[2], p[3].toInt())] ?: error("missing input")
        o.spender?.let { cur ->
            // BIP 125: an unconfirmed spend is replaced only by one paying more
            if (cur != hex) check(o.spendConfs == 0L && feeOf(hex) > feeOf(cur)) { "insufficient fee: conflict" }
        }
        o.spender = hex
        broadcasts += chain to hex
        return "spend-${++n}"
    }
}

/** `SwapSession` stand-in. Its whole state is its JSON, as with the real one. */
class FakeCrypto private constructor(private val st: JSONObject) : SwapCrypto {
    companion object {
        fun open(r: SwapRecord) = FakeCrypto(
            r.sessionJson?.let(::JSONObject)
                ?: JSONObject().put("role", r.role).put("terms", JSONObject(r.termsJson!!)),
        )
    }

    private val role get() = st.getString("role")
    private val terms get() = st.getJSONObject("terms")
    private val send get() = if (role == "initiator") "xbt" else "btc"
    private val recv get() = if (role == "initiator") "btc" else "xbt"
    private fun amount(chain: String) = terms.getLong(if (chain == "xbt") "xbt_sat" else "btc_sat")
    private fun htlc(chain: String): HtlcInfo {
        check(st.has("their")) { "counterparty not set yet" }
        return HtlcInfo("addr-$chain-htlc", amount(chain).toULong(), "spk-$chain-htlc")
    }

    override fun validateTerms(termsJson: String, ourPrice: Long, slippageBps: Int, role: String, now: Long) {
        val t = JSONObject(termsJson)
        val diff = Math.abs(t.getLong("price_sat_per_xbt") - ourPrice)
        check(ourPrice > 0 && diff * 10_000 <= ourPrice * slippageBps) { "price outside slippage" }
        check(now <= t.getLong("accept_by")) { "accept deadline passed" }
    }

    override fun participantFundLatest(termsJson: String): Long {
        val t = JSONObject(termsJson)
        val (bl, at) = t.getLong("btc_locktime") to t.getLong("matched_at")
        return bl - minOf(12 * 3600L, (bl - at) / 2)
    }

    override fun ourPubkeys() = SwapPubkeys("xbt-$role", "btc-$role")
    override fun hashlock(): String? = if (role == "initiator") hashOf(PREIMAGE) else null
    override fun preimage(): String? = if (role == "initiator") PREIMAGE else null

    override fun setCounterparty(their: SwapPubkeys, hashlockHex: String) {
        val v = JSONObject().put("xbt", their.xbt).put("btc", their.btc).put("hashlock", hashlockHex)
        if (st.has("their")) {
            check(st.getJSONObject("their").toString() == v.toString()) { "counterparty already set to different keys" }
            return
        }
        st.put("their", v)
    }

    override fun ourHtlc() = htlc(send)
    override fun theirHtlc() = htlc(recv)

    override fun verifyTheirFunding(scriptPubkeyHex: String, valueSat: Long) {
        val h = theirHtlc()
        check(scriptPubkeyHex == h.scriptPubkeyHex && valueSat == h.amountSat.toLong()) { "contract mismatch" }
    }

    override fun recordTheirOutpoint(txid: String, vout: Int) { st.put("their_op", "$txid|$vout") }
    override fun recordOurOutpoint(txid: String, vout: Int) { st.put("our_op", "$txid|$vout") }

    override fun signedRefund(feerates: List<Long>): List<String> {
        val op = st.optString("our_op").ifEmpty { error("our funding outpoint is not recorded") }
        return feerates.map { "refund|$send|$op|fee=$it" }
    }

    override fun signedRedeem(preimageHex: String, feerates: List<Long>): List<String> {
        check(hashOf(preimageHex) == st.getJSONObject("their").getString("hashlock")) { "wrong preimage" }
        val op = st.optString("their_op").ifEmpty { error("their funding outpoint is not recorded") }
        return feerates.map { "redeem|$recv|$op|preimage=$preimageHex|fee=$it" }
    }

    override fun presignedRedeem(feerates: List<Long>): List<FfiPresignedRedeem> {
        val op = st.optString("their_op").ifEmpty { error("their funding outpoint is not recorded") }
        return feerates.map { FfiPresignedRedeem("presigned|$recv|$op|fee=$it", "sig-$it", "pub-$role", "ws-$recv") }
    }

    override fun preimageFromTx(txHex: String): String? {
        val p = Regex("preimage=([^|]+)").find(txHex)?.groupValues?.get(1) ?: return null
        return p.takeIf { hashOf(it) == st.getJSONObject("their").getString("hashlock") }
    }

    override fun toJson(): String = st.toString()
}

class FakeFunder(private val store: SwapStore, private val chain: FakeChain) : Funder {
    var locked = false
    var calls = 0
    var dieAfterBroadcast = false

    /** Sign and save, but the broadcast never reaches the node (and no error comes back). */
    var skipBroadcast = false
    val hexes = mutableListOf<String>()

    override suspend fun fund(r: SwapRecord, htlc: HtlcInfo): String? {
        if (locked) return null
        calls++
        val txid = "fund-${r.sendChain}-${r.id}-$calls"
        val hex = "funding|${r.sendChain}|$txid|${htlc.amountSat}|${htlc.scriptPubkeyHex}|${htlc.address}"
        hexes += hex
        // the txid and the signed hex are known before the broadcast, and saved first
        store.put(store.get(r.id)!!.copy(pendingFundingTxid = txid, pendingFundingHex = hex))
        if (!skipBroadcast) chain.broadcast(r.sendChain, hex)
        if (dieAfterBroadcast) throw IllegalStateException("process died after the broadcast")
        return txid
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

class SwapEngineTest {
    private var clock = MATCHED_AT + 10
    private val chain = FakeChain()
    private val edge = FakeApi(chain) { clock }

    inner class Phone(val sendChain: String, var price: Long? = PRICE) {
        /** NeoxEX can't be reached (as opposed to refusing to quote). */
        var priceDown = false
        val store = SwapStore(MemoryBlob())
        val funder = FakeFunder(store, chain)
        val events = mutableListOf<Pair<Event, Long?>>()
        var engine = engine(funder)
        lateinit var id: String

        fun engine(f: Funder) = SwapEngine(
            edge, store, chain, { FakeCrypto.open(it) }, f, { clock },
            { r, e -> events += e to r.deadline },
            ownPrice = { if (priceDown) throw IOException("neoxa.exchange unreachable") else price },
        )

        fun offer(): String = runBlocking {
            val (direction, amount) = if (sendChain == "xbt") "xbt_to_btc" to XBT_SAT else "btc_to_xbt" to BTC_SAT
            val o = edge.createOffer(direction, amount, 100, false, null)
            id = o.offerId
            store.put(
                SwapRecord(
                    id = o.offerId, fromWalletId = "w-$sendChain", toWalletId = "w-other",
                    sendChain = sendChain, amountSat = amount, slippageBps = 100,
                    ownerSecret = o.ownerSecret, swapIndex = store.nextSwapIndex("w-$sendChain"), account = 0,
                    wrappedXbt = "wx", wrappedBtc = "wb",
                    redeemAddress = "redeem-$sendChain", refundAddress = "refund-$sendChain",
                ),
            )
            id
        }

        /** As `WalletViewModel.createOffer`: the record saved before any POST. */
        fun provisional(private: Boolean = false): String = runBlocking {
            val amount = if (sendChain == "xbt") XBT_SAT else BTC_SAT
            id = "c0ffee$sendChain"
            store.put(
                SwapRecord(
                    id = id, clientId = id, privateOffer = private, step = Step.PROVISIONAL,
                    fromWalletId = "w-$sendChain", toWalletId = "w-other",
                    sendChain = sendChain, amountSat = amount, slippageBps = 100,
                    ownerSecret = "", swapIndex = store.nextSwapIndex("w-$sendChain"), account = 0,
                    wrappedXbt = "wx", wrappedBtc = "wb",
                    redeemAddress = "redeem-$sendChain", refundAddress = "refund-$sendChain",
                ),
            )
            id
        }

        fun step(): SwapRecord = runBlocking { engine.step(id) }
        fun record(): SwapRecord = runBlocking { store.get(id)!! }
        val names get() = events.map { it.first }
    }

    private val alice = Phone("xbt") // initiator
    private val bob = Phone("btc") // participant

    private fun matched(): FakeApi.Match {
        alice.offer(); bob.offer()
        assertEquals(Step.WAITING_MATCH, alice.step().step)
        return edge.pair(alice.id, bob.id)
    }

    /** Matched and both accepted; bob has alice's keys and hashlock, alice gets
     *  bob's keys (and funds) on her next step. */
    private fun negotiated(): FakeApi.Match {
        val m = matched()
        alice.step(); bob.step()
        return m
    }

    /** Step both phones, mining one block per round, until both settle. */
    private fun runToEnd(rounds: Int = 40) {
        repeat(rounds) {
            if (!alice.record().step.active && !bob.record().step.active) return
            alice.step(); bob.step(); chain.mine(1)
        }
    }

    @Test
    fun quick_offer_to_done_initiator() {
        val m = negotiated()
        runToEnd()
        val r = alice.record()
        assertEquals(Step.DONE, r.step)
        assertEquals(listOf(Event.MATCHED, Event.THEY_FUNDED, Event.DONE), alice.names)
        assertEquals(1, alice.funder.calls)
        assertEquals("", r.wrappedXbt); assertEquals("", r.wrappedBtc)
        // accept, keys and the hashlock were relayed
        assertEquals(1, edge.bodies(m.id, "initiator", "accept").size)
        assertEquals(hashOf(PREIMAGE), edge.bodies(m.id, "initiator", "hashlock").single().getString("hex"))
        // the XBT funding: the HTLC output, announced once
        val funded = edge.bodies(m.id, "initiator", "funded").single()
        assertEquals("xbt", funded.getString("chain"))
        assertEquals(1, funded.getInt("vout"))
        // the refund is watched from the XBT locktime
        val refund = edge.watches.single { it.first == "initiator" && it.second.getString("kind") == "refund" }.second
        assertEquals("xbt", refund.getString("chain"))
        assertEquals(XBT_LOCKTIME, refund.getLong("not_before"))
        assertEquals(2, refund.getJSONArray("variants").length())
        assertEquals(r.presigned, (0 until 2).map { refund.getJSONArray("variants").getString(it) })
        // the BTC redeem reveals the preimage and is watched too
        val redeem = chain.broadcasts.single { it.first == "btc" }.second
        assertTrue(redeem.startsWith("redeem|btc|${m.btcFunding!!.txid}|1|preimage=$PREIMAGE"))
        assertTrue(edge.watches.any { it.first == "initiator" && it.second.getString("kind") == "redeem" && it.second.getString("chain") == "btc" })
    }

    @Test
    fun quick_offer_to_done_participant() {
        val m = negotiated()
        runToEnd()
        val r = bob.record()
        assertEquals(Step.DONE, r.step)
        assertEquals(listOf(Event.MATCHED, Event.THEY_FUNDED, Event.DONE), bob.names)
        assertEquals(1, bob.funder.calls)
        assertEquals("", r.wrappedXbt); assertEquals("", r.wrappedBtc)
        assertTrue(edge.bodies(m.id, "participant", "hashlock").isEmpty())
        val funded = edge.bodies(m.id, "participant", "funded").single()
        assertEquals("btc", funded.getString("chain"))
        // the deadline from the edge was taken in
        assertTrue(m.participantFundBy != null)
        // the refund from the BTC locktime, and the preimage-less XBT redeem
        val watches = edge.watches.filter { it.first == "participant" }.associate { it.second.getString("kind") to it.second }
        assertEquals(BTC_LOCKTIME, watches.getValue("refund").getLong("not_before"))
        assertEquals("btc", watches.getValue("refund").getString("chain"))
        val redeem = watches.getValue("redeem")
        assertEquals("xbt", redeem.getString("chain"))
        assertEquals(m.xbtFunding!!.txid, redeem.getJSONObject("htlc_outpoint").getString("txid"))
        assertEquals(redeem.getJSONArray("variants").length(), redeem.getJSONArray("sigs").length())
        assertEquals("sig-2", redeem.getJSONArray("sigs").getString(0))
        assertEquals("pub-participant", redeem.getString("pubkey"))
        assertEquals("ws-xbt", redeem.getString("witness_script"))
        // our XBT redeem, with the preimage from the BTC redeem
        val xbt = chain.broadcasts.single { it.first == "xbt" }.second
        assertTrue(xbt.startsWith("redeem|xbt|${m.xbtFunding!!.txid}|1|preimage=$PREIMAGE"))
    }

    @Test
    fun slippage_rejection_declines() {
        bob.price = PRICE * 105 / 100
        val m = matched()
        alice.step() // alice follows the match (a phone that hadn't would just see its offer reopened)
        val r = bob.step()
        assertEquals(Step.CANCELLED, r.step)
        assertTrue(r.error!!.contains("slippage"))
        assertEquals(1, edge.bodies(m.id, "participant", "decline").size)
        assertTrue(edge.bodies(m.id, "participant", "accept").isEmpty())
        assertTrue(edge.bodies(m.id, "participant", "keys").isEmpty())
        assertEquals("cancelled", m.status)
        // the other side sees the dissolved match and follows its reopened offer, unfunded
        val a = alice.step()
        assertEquals(Step.WAITING_MATCH, a.step)
        assertNull(a.matchId)
        assertEquals("open", edge.offers.getValue(alice.id).status)
        assertEquals(0, alice.funder.calls)
    }

    @Test
    fun wrong_fee_address_declines() {
        edge.pricing = "bcrt1qsomeoneelse"
        val m = matched()
        assertEquals(Step.CANCELLED, alice.step().step)
        assertEquals(1, edge.bodies(m.id, "initiator", "decline").size)
    }

    @Test
    fun missed_deadline_cancels_unfunded() {
        alice.funder.locked = true
        negotiated()
        assertEquals(Step.NEED_FUNDING, alice.step().step)
        clock = INITIATOR_FUND_BY + 1
        val r = alice.step()
        assertEquals(Step.CANCELLED, r.step)
        assertEquals(0, alice.funder.calls)
        assertNull(r.fundingTxid)
    }

    @Test
    fun locked_wallet_notifies_need_funding_then_funds_on_unlock() {
        alice.funder.locked = true
        val m = negotiated()
        var r = alice.step()
        assertEquals(Step.NEED_FUNDING, r.step)
        assertEquals(INITIATOR_FUND_BY, r.deadline)
        assertEquals(Event.NEED_FUNDING to INITIATOR_FUND_BY, alice.events.last())
        // still locked: no second notification
        alice.step()
        assertEquals(1, alice.names.count { it == Event.NEED_FUNDING })
        alice.funder.locked = false
        r = alice.step()
        assertEquals(Step.FUNDED, r.step)
        assertEquals(1, alice.funder.calls)
        assertEquals(r.fundingTxid, edge.bodies(m.id, "initiator", "funded").single().getString("txid"))
    }

    /** [Important 1] The alert carries the computed funding deadline, for both roles. */
    @Test
    fun need_funding_alert_carries_the_computed_deadline_for_the_initiator() {
        alice.funder.locked = true
        negotiated()
        alice.step()
        assertEquals(Event.NEED_FUNDING to INITIATOR_FUND_BY, alice.events.last { it.first == Event.NEED_FUNDING })
    }

    @Test
    fun need_funding_alert_carries_the_computed_deadline_for_the_participant() {
        bob.funder.locked = true
        negotiated()
        alice.step() // alice funds XBT
        chain.mine(6)
        alice.step() // the watchtower sets participant_fund_by on the next match read
        val r = bob.step()
        assertEquals(Step.NEED_FUNDING, r.step)
        val deadline = r.deadline
        assertNotNull(deadline)
        assertEquals(Event.NEED_FUNDING to deadline, bob.events.last { it.first == Event.NEED_FUNDING })
    }

    @Test
    fun resume_after_funding_broadcast_does_not_fund_twice() {
        val m = negotiated()
        alice.funder.dieAfterBroadcast = true
        alice.step()
        assertEquals(1, alice.funder.calls)
        assertTrue(edge.bodies(m.id, "initiator", "funded").isEmpty())
        val pending = alice.record().pendingFundingTxid!!
        // the process restarts: a new engine (and funder) on the same store
        val again = FakeFunder(alice.store, chain)
        alice.engine = alice.engine(again)
        val r = alice.step()
        assertEquals(0, again.calls)
        assertEquals(Step.FUNDED, r.step)
        assertEquals(pending, r.fundingTxid)
        assertEquals(1, r.fundingVout)
        val funded = edge.bodies(m.id, "initiator", "funded").single()
        assertEquals(pending, funded.getString("txid"))
        assertEquals(1, funded.getInt("vout"))
        // and stepping again changes nothing
        alice.step()
        assertEquals(0, again.calls)
        assertEquals(1, edge.bodies(m.id, "initiator", "funded").size)
    }

    @Test
    fun resume_finds_btc_funding_by_address_without_a_pending_txid() {
        alice.offer(); bob.offer()
        val m = edge.pair(alice.id, bob.id)
        alice.step(); bob.step(); alice.step() // alice funds XBT
        chain.mine(6)
        bob.funder.dieAfterBroadcast = true
        bob.step()
        // lose the pending txid too: only the chain knows
        runBlocking { bob.store.put(bob.record().copy(pendingFundingTxid = null, pendingFundingHex = null)) }
        val again = FakeFunder(bob.store, chain)
        bob.engine = bob.engine(again)
        val r = bob.step()
        assertEquals(0, again.calls)
        assertEquals(1, edge.bodies(m.id, "participant", "funded").size)
        assertEquals(Step.THEY_FUNDED, r.step)
    }

    @Test
    fun refund_after_locktime_when_counterparty_never_funds() {
        val m = negotiated()
        assertEquals(Step.FUNDED, alice.step().step)
        chain.mine(6)
        // bob never funds
        clock = XBT_LOCKTIME - 1
        assertEquals(Step.FUNDED, alice.step().step)
        assertTrue(chain.broadcasts.isEmpty())
        clock = XBT_LOCKTIME
        var r = alice.step()
        assertEquals(Step.REFUNDING, r.step)
        assertEquals(Event.REFUNDING, alice.names.last())
        assertEquals(r.presigned.first(), chain.spenderOf("xbt", m.xbtFunding!!.txid, 1))
        alice.step()
        assertEquals(1, chain.broadcasts.size)
        chain.mine(1)
        r = alice.step()
        assertEquals(Step.REFUNDED, r.step)
        assertEquals("", r.wrappedXbt); assertEquals("", r.wrappedBtc)
    }

    @Test
    fun participant_redeems_from_initiator_btc_redeem() {
        val m = negotiated()
        alice.step() // alice funds XBT
        chain.mine(6)
        assertEquals(Step.THEY_FUNDED, bob.step().step) // bob verifies it and funds BTC
        chain.mine(2)
        // alice's redeem appears on BTC, sent from elsewhere (bob never sees alice's phone)
        val btc = m.btcFunding!!
        runBlocking { chain.broadcast("btc", "redeem|btc|${btc.txid}|${btc.vout}|preimage=$PREIMAGE|fee=7") }
        val r = bob.step()
        assertEquals(Step.REDEEMED, r.step)
        val xbt = chain.spenderOf("xbt", m.xbtFunding!!.txid, m.xbtFunding!!.vout)!!
        assertTrue(xbt.startsWith("redeem|xbt|${m.xbtFunding!!.txid}|1|preimage=$PREIMAGE"))
        chain.mine(1)
        assertEquals(Step.DONE, bob.step().step)
    }

    @Test
    fun participant_does_not_fund_a_short_xbt_htlc() {
        val m = negotiated()
        alice.step()
        // the XBT HTLC is one sat short
        val f = m.xbtFunding!!
        chain.outs["xbt:${f.txid}:${f.vout}"] = FakeChain.Out("addr-xbt-htlc", XBT_SAT - 1, "spk-xbt-htlc")
        chain.mine(6)
        val r = bob.step()
        assertEquals(0, bob.funder.calls)
        assertNull(r.theirTxid)
        assertTrue(r.error!!.contains("mismatch"))
        assertFalse(Event.THEY_FUNDED in bob.names)
    }

    @Test
    fun store_round_trips_records_and_counts_swap_indices() = runBlocking {
        val s = SwapStore(MemoryBlob())
        val r = SwapRecord(
            id = "o1", matchId = "m1", role = "initiator", fromWalletId = "a", toWalletId = "b",
            sendChain = "xbt", amountSat = 5, slippageBps = 50, ownerSecret = "s", swapIndex = 2, account = 0,
            sessionJson = "{}", wrappedXbt = "x", wrappedBtc = "y", relaySeq = 4, pendingFundingTxid = "p",
            fundingTxid = "f", fundingVout = 1, presigned = listOf("v1", "v2"), step = Step.FUNDED,
            deadline = 9, error = "e", redeemAddress = "ra", refundAddress = "rf", termsJson = "{\"a\":1}",
            theirXbtKey = "tx", theirBtcKey = "tb", hashlock = "h", theirTxid = "t", theirVout = 0,
            redeemTxid = "rt", refundTxid = "ft", marks = setOf("accepted"),
            pendingFundingHex = "hex", rejectedFundings = listOf("r1"),
        )
        s.put(r)
        s.put(r.copy(id = "o2", step = Step.DONE))
        assertEquals(r, s.get("o1"))
        assertEquals(listOf("o1"), s.active().map { it.id })
        assertEquals(2, s.all().size)
        s.remove("o1")
        assertNull(s.get("o1"))
        s.setToken("tok")
        assertEquals("tok", s.token())
    }

    // --- fix round 1 -------------------------------------------------------

    /** [Critical 1] A reinstall (empty store) must not reuse swap index 0: the
     *  keys and the initiator's preimage are seed-deterministic per index. */
    @Test
    fun fresh_stores_draw_random_swap_indices() = runBlocking {
        val first = SwapStore(MemoryBlob()).nextSwapIndex("a")
        val afterReinstall = SwapStore(MemoryBlob()).nextSwapIndex("a")
        assertTrue("a fresh store must not start at 0", first != 0)
        assertTrue("two fresh stores must not draw the same index", first != afterReinstall)
        assertTrue(first >= 0 && afterReinstall >= 0)
    }

    @Test
    fun swap_index_redraws_on_collision_with_records_and_earlier_draws() = runBlocking {
        val draws = ArrayDeque(listOf(5, 5, 9, 9, 5, 11))
        val s = SwapStore(MemoryBlob()) { draws.removeFirst() }
        s.put(
            SwapRecord(
                id = "o1", fromWalletId = "a", toWalletId = "b", sendChain = "xbt", amountSat = 5,
                slippageBps = 50, ownerSecret = "s", swapIndex = 5, wrappedXbt = "x", wrappedBtc = "y",
                redeemAddress = "ra", refundAddress = "rf",
            ),
        )
        // 5 is a record's: redrawn
        assertEquals(9, s.nextSwapIndex("a"))
        // 9 was handed out already (its record not saved yet), 5 is a record's: redrawn — on any wallet
        assertEquals(11, s.nextSwapIndex("b"))
    }

    /** [Important 2] The edge takes keys while the match is still `proposed`;
     *  the initiator must not fund until both sides have accepted. */
    @Test
    fun keys_without_accept_do_not_fund() {
        val m = matched()
        alice.step() // accept + keys + hashlock
        // bob's phone posts keys but never accepts
        runBlocking { edge.post(m.id, m.participantSecret, JSONObject().put("type", "keys").put("xbt", "xbt-participant").put("btc", "btc-participant")) }
        alice.step(); alice.step()
        assertEquals("proposed", m.status)
        assertEquals(0, alice.funder.calls)
        assertNull(alice.record().pendingFundingTxid)
    }

    /** [Important 2] The refund watch goes out before, and independently of, `funded`. */
    @Test
    fun refund_watch_is_posted_even_when_funded_post_fails() {
        val m = negotiated()
        edge.failFunded = true
        alice.step()
        assertEquals(1, alice.funder.calls)
        assertTrue(edge.watches.any { it.first == "initiator" && it.second.getString("kind") == "refund" })
        assertTrue(edge.bodies(m.id, "initiator", "funded").isEmpty())
        edge.failFunded = false
        alice.step()
        assertEquals(1, edge.bodies(m.id, "initiator", "funded").size)
        assertEquals(1, edge.watches.count { it.first == "initiator" && it.second.getString("kind") == "refund" })
        assertEquals(1, alice.funder.calls)
    }

    /** [Important 3] A pending funding that isn't visible is re-sent as the same
     *  signed hex; the funder is never asked again. */
    @Test
    fun pending_funding_not_visible_rebroadcasts_the_same_hex() {
        negotiated()
        alice.funder.skipBroadcast = true
        alice.step()
        assertEquals(1, alice.funder.calls)
        val pending = alice.record().pendingFundingTxid!!
        val hex = alice.funder.hexes.single()
        assertEquals(hex, alice.record().pendingFundingHex)
        // the indexer lags behind the re-sent transaction
        chain.hidden += pending
        alice.step()
        assertEquals(1, alice.funder.calls)
        assertEquals(listOf(hex), chain.fundingBroadcasts)
        assertEquals(pending, alice.record().pendingFundingTxid)
        alice.step()
        assertEquals(1, alice.funder.calls)
        assertTrue(chain.fundingBroadcasts.all { it == hex })
        chain.hidden.clear()
        val r = alice.step()
        assertEquals(1, alice.funder.calls)
        assertEquals(Step.FUNDED, r.step)
        assertEquals(pending, r.fundingTxid)
    }

    /** [Important 3] Past the deadline (and the edge's cancel) with a funding
     *  still unresolved: stay active; when it shows up, watch and refund it. */
    @Test
    fun unresolved_pending_funding_past_deadline_stays_active_and_is_refunded() {
        val m = negotiated()
        chain.down = true
        alice.step() // signed and saved, the broadcast got no answer
        assertEquals(1, alice.funder.calls)
        assertTrue(alice.record().pendingFundingTxid != null)
        clock = INITIATOR_FUND_BY + 1
        m.status = "cancelled" // the edge's deadline tick
        var r = alice.step()
        assertTrue("must stay active while our funding may still land", r.step.active)
        assertEquals(1, alice.funder.calls)
        chain.down = false
        r = alice.step()
        assertEquals(1, alice.funder.calls)
        assertEquals(alice.record().pendingFundingTxid, r.fundingTxid)
        assertTrue(r.step.active)
        val refund = edge.watches.single { it.first == "initiator" && it.second.getString("kind") == "refund" }.second
        assertEquals(XBT_LOCKTIME, refund.getLong("not_before"))
        clock = XBT_LOCKTIME
        r = alice.step()
        assertEquals(Step.REFUNDING, r.step)
        assertEquals(r.presigned.first(), chain.spenderOf("xbt", r.fundingTxid!!, r.fundingVout!!))
    }

    /** [Important 3] The match is cancelled (the edge's deadline tick) while our
     *  funding is in flight: stay active, refund it later — never re-follow. */
    @Test
    fun match_cancelled_while_our_funding_is_in_flight_keeps_the_swap() {
        val m = negotiated()
        chain.down = true
        alice.step()
        edge.lapse(m, reopenInitiator = false, reopenParticipant = true)
        assertEquals("cancelled", m.status)
        assertTrue(alice.step().step.active)
        chain.down = false
        val r = alice.step()
        assertEquals(Step.FUNDED, r.step)
        assertTrue(edge.watches.any { it.first == "initiator" && it.second.getString("kind") == "refund" })
        assertEquals(1, alice.funder.calls)
    }

    /** [Important 3] Only a definitive node rejection (inputs spent) frees us to plan again. */
    @Test
    fun definitively_rejected_funding_is_replanned() {
        negotiated()
        chain.reject = "sendrawtransaction RPC error: bad-txns-inputs-missingorspent"
        alice.step()
        assertEquals(1, alice.funder.calls)
        alice.step() // the re-send is rejected the same way: give up on that one
        assertEquals(1, alice.funder.calls)
        assertNull(alice.record().pendingFundingTxid)
        assertTrue(alice.record().error!!.contains("missingorspent"))
        chain.reject = null
        val r = alice.step()
        assertEquals(2, alice.funder.calls)
        assertEquals(Step.FUNDED, r.step)
    }

    // --- fix round 2 -------------------------------------------------------

    /** [Important A] The edge takes a refund watch for an outpoint no `funded`
     *  recorded only once its own chain read shows our exact HTLC; until then
     *  the watch is refused, and the engine retries it each step — also after
     *  the match was cancelled (the deadline tick, before our `funded` landed). */
    @Test
    fun refund_watch_refused_by_the_edge_is_retried_until_accepted() {
        val m = negotiated()
        edge.failFunded = true
        edge.blind += "fund-xbt-${alice.id}-1" // the edge's indexer lags behind ours
        var r = alice.step()
        assertEquals(Step.FUNDED, r.step)
        assertTrue("the edge can't check it yet", edge.watches.none { it.first == "initiator" })
        assertTrue(r.error != null)
        assertNull(m.xbtFunding)
        edge.lapse(m, reopenInitiator = false, reopenParticipant = true)
        assertEquals("cancelled", m.status)
        edge.blind.clear()
        r = alice.step()
        val refund = edge.watches.single { it.first == "initiator" && it.second.getString("kind") == "refund" }.second
        assertEquals(r.fundingTxid, refund.getJSONObject("htlc_outpoint").getString("txid"))
        assertEquals(Outpoint(r.fundingTxid!!, r.fundingVout!!), m.xbtFunding)
        assertEquals("cancelled", m.status)
        alice.step()
        assertEquals(1, edge.watches.count { it.first == "initiator" })
        assertEquals(1, alice.funder.calls)
    }

    /** [Important A] The fake edge refuses what the real one does: an
     *  unrecorded outpoint that isn't the leg's exact HTLC, or one checked
     *  before both keys and the hashlock are relayed. */
    @Test
    fun edge_fake_refuses_a_watch_of_anything_but_the_exact_htlc() = runBlocking {
        val m = matched()
        val refund = { txid: String ->
            JSONObject().put("match_id", m.id).put("chain", "xbt").put("kind", "refund")
                .put("htlc_outpoint", JSONObject().put("txid", txid).put("vout", 1))
                .put("not_before", XBT_LOCKTIME).put("variants", org.json.JSONArray(listOf("refund|xbt|$txid|1")))
        }
        fun refused(body: JSONObject) = try {
            runBlocking { edge.watch(m.initiatorSecret, body) }; false
        } catch (e: SwapHttpException) {
            e.code == 409
        }
        chain.add("xbt", "good", 1, XBT_SAT, "spk-xbt-htlc", "addr-xbt-htlc")
        chain.add("xbt", "short", 1, XBT_SAT - 1, "spk-xbt-htlc", "addr-xbt-htlc")
        chain.add("xbt", "other", 1, XBT_SAT, "spk-other", "addr-other")
        assertTrue("keys not relayed yet", refused(refund("good")))
        alice.step(); bob.step()
        assertTrue(refused(refund("short")))
        assertTrue(refused(refund("other")))
        assertTrue(refused(refund("unknown")))
        assertNull(m.xbtFunding)
        assertFalse(refused(refund("good")))
        assertEquals(Outpoint("good", 1), m.xbtFunding)
        assertEquals("accepted", m.status)
        // `funded` with the same outpoint still moves the match on
        edge.post(m.id, m.initiatorSecret, JSONObject().put("type", "funded").put("chain", "xbt").put("txid", "good").put("vout", 1))
        assertEquals("initiator_funded", m.status)
    }

    /** [fix 3] A redeem watch names the counterparty's leg: the edge never
     *  records a funding from it, even for a chain-valid HTLC. */
    @Test
    fun edge_fake_never_records_a_funding_from_a_redeem_watch() = runBlocking {
        val m = matched()
        alice.step(); bob.step()
        chain.add("xbt", "good", 1, XBT_SAT, "spk-xbt-htlc", "addr-xbt-htlc")
        val redeem = JSONObject().put("match_id", m.id).put("chain", "xbt").put("kind", "redeem")
            .put("htlc_outpoint", JSONObject().put("txid", "good").put("vout", 1))
            .put("not_before", 0).put("variants", org.json.JSONArray(listOf("redeem|xbt|good|1")))
        val code = try { edge.watch(m.participantSecret, redeem); 200 } catch (e: SwapHttpException) { e.code }
        assertEquals(409, code)
        assertNull(m.xbtFunding)
    }

    /** [Important B] A conflict with an unconfirmed tx proves nothing about our
     *  funding: keep it, re-send the same hex, never plan another. */
    @Test
    fun mempool_conflict_keeps_the_pending_funding_and_resends_it() {
        negotiated()
        chain.reject = "sendrawtransaction RPC error: txn-mempool-conflict"
        alice.step()
        assertEquals(1, alice.funder.calls)
        val pending = alice.record().pendingFundingTxid!!
        val hex = alice.funder.hexes.single()
        alice.step()
        assertEquals(1, alice.funder.calls)
        assertEquals(pending, alice.record().pendingFundingTxid)
        assertEquals(listOf(hex, hex), chain.fundingAttempts)
        chain.reject = null
        val r = alice.step()
        assertEquals(1, alice.funder.calls)
        assertEquals(Step.FUNDED, r.step)
        assertEquals(pending, r.fundingTxid)
        assertTrue(chain.fundingAttempts.all { it == hex })
    }

    /** Up to both fundings, mined: alice holds XBT in her HTLC, bob BTC in his. */
    private fun bothFunded(): FakeApi.Match {
        val m = negotiated()
        alice.step() // alice funds XBT
        chain.mine(6)
        assertEquals(Step.THEY_FUNDED, bob.step().step) // bob funds BTC
        chain.mine(2)
        return m
    }

    /** [Important 4] Bob broadcast his refund but alice's redeem won: bob must
     *  take the preimage and redeem XBT — never REFUNDED, keys kept. */
    @Test
    fun participant_redeems_when_initiator_redeem_beats_its_refund() {
        val m = bothFunded()
        clock = BTC_LOCKTIME
        assertEquals(Step.REFUNDING, bob.step().step)
        // alice's redeem (sent near the locktime) is what confirms
        val btc = m.btcFunding!!
        chain.outs["btc:${btc.txid}:${btc.vout}"]!!.spender = "redeem|btc|${btc.txid}|${btc.vout}|preimage=$PREIMAGE|fee=10"
        chain.mine(1)
        val r = bob.step()
        assertEquals(Step.REDEEMED, r.step)
        assertTrue(r.wrappedXbt.isNotEmpty())
        val xbt = chain.spenderOf("xbt", m.xbtFunding!!.txid, m.xbtFunding!!.vout)!!
        assertTrue(xbt.startsWith("redeem|xbt|${m.xbtFunding!!.txid}|1|preimage=$PREIMAGE"))
    }

    /** [Important 4] Alice's redeem lost to bob's refund: not DONE; she refunds her XBT. */
    @Test
    fun initiator_sees_participant_refund_and_refunds_its_own_xbt() {
        val m = bothFunded()
        assertEquals(Step.REDEEMED, alice.step().step)
        val btc = m.btcFunding!!
        chain.outs["btc:${btc.txid}:${btc.vout}"]!!.spender = "refund|btc|${btc.txid}|${btc.vout}|fee=10"
        chain.mine(1)
        var r = alice.step()
        assertTrue(r.step != Step.DONE)
        assertTrue(r.step.active)
        assertTrue(r.wrappedXbt.isNotEmpty())
        assertFalse(Event.DONE in alice.names)
        clock = XBT_LOCKTIME
        r = alice.step()
        assertEquals(Step.REFUNDING, r.step)
        assertEquals(r.presigned.first(), chain.spenderOf("xbt", m.xbtFunding!!.txid, m.xbtFunding!!.vout))
    }

    /** [Important 4] Too close to the BTC locktime, revealing the preimage could
     *  let bob refund BTC and still take the XBT: don't start a redeem. */
    @Test
    fun initiator_does_not_start_a_btc_redeem_near_the_btc_locktime() {
        bothFunded()
        clock = BTC_LOCKTIME - 1800
        val r = alice.step()
        assertTrue(chain.broadcasts.none { it.first == "btc" })
        assertEquals(Step.THEY_FUNDED, r.step)
        assertTrue(r.error!!.contains("locktime"))
    }

    /** [Important 5] Our redeem went out, the step failed after it, fees moved:
     *  still recognised as our redeem. */
    @Test
    fun initiator_redeem_is_recognised_after_fees_change() {
        bothFunded()
        edge.failWatch = { it.getString("kind") == "redeem" }
        val r1 = alice.step()
        assertTrue(chain.broadcasts.any { it.first == "btc" })
        assertTrue(r1.step != Step.REDEEMED)
        chain.fees = listOf(5, 20)
        edge.failWatch = { false }
        val r = alice.step()
        assertEquals(Step.REDEEMED, r.step)
        assertEquals(1, chain.broadcasts.count { it.first == "btc" })
    }

    /** [Final I2] A declined match puts both offers back on the edge (spec: "the
     *  match dissolves and both offers reopen"). The decliner walks away and
     *  takes its offer down; the other side follows its reopened offer. */
    @Test
    fun decline_ends_the_decliner_and_the_other_side_refollows() {
        bob.price = PRICE * 105 / 100
        matched()
        alice.step() // alice follows (and accepts) the match
        assertEquals(Step.CANCELLED, bob.step().step)
        assertEquals("cancelled", edge.offers.getValue(bob.id).status)
        val cancels = edge.cancelCalls.count { it == alice.id } // the remainder check at accept
        assertEquals(Step.WAITING_MATCH, alice.step().step)
        assertEquals("open", edge.offers.getValue(alice.id).status)
        assertEquals("no cancel once the match ended", cancels, edge.cancelCalls.count { it == alice.id })
    }

    /** [Important 6] Accepting a partial fill takes the remainder off the book. */
    @Test
    fun partial_fill_accept_cancels_the_remainder() {
        alice.offer(); bob.offer()
        val m = edge.pair(alice.id, bob.id, participantRemainder = true)
        assertEquals("open", edge.offers.getValue(bob.id).status)
        bob.step()
        assertEquals(1, edge.bodies(m.id, "participant", "accept").size)
        assertEquals("cancelled", edge.offers.getValue(bob.id).status)
        assertTrue(bob.record().step.active)
        // a full fill: the 409 is ignored
        alice.step()
        assertTrue(alice.record().step.active)
        assertEquals("accepted", m.status)
    }

    /** [Important 7] An unreachable `/pricing` is not a mismatch: retry, don't decline. */
    @Test
    fun pricing_endpoint_error_retries_without_declining() {
        edge.pricingDown = true
        val m = matched()
        var r = alice.step()
        assertTrue(r.step.active)
        assertTrue(edge.bodies(m.id, "initiator", "decline").isEmpty())
        assertTrue(edge.bodies(m.id, "initiator", "accept").isEmpty())
        assertTrue(r.error != null)
        edge.pricingDown = false
        r = alice.step()
        assertEquals(1, edge.bodies(m.id, "initiator", "accept").size)
        assertTrue(r.step.active)
    }

    /** [Important 7] NeoxEX unreachable: retry; decline only as accept_by closes in. */
    @Test
    fun neoxex_unreachable_retries_then_declines_near_accept_by() {
        alice.priceDown = true
        val m = matched()
        assertTrue(alice.step().step.active)
        assertTrue(edge.bodies(m.id, "initiator", "decline").isEmpty())
        clock = ACCEPT_BY - 5
        assertEquals(Step.CANCELLED, alice.step().step)
        assertEquals(1, edge.bodies(m.id, "initiator", "decline").size)
    }

    /** [Important 7] NeoxEX answering but refusing to quote is a definite decline. */
    @Test
    fun neoxex_refusal_declines() {
        alice.price = null
        val m = matched()
        assertEquals(Step.CANCELLED, alice.step().step)
        assertEquals(1, edge.bodies(m.id, "initiator", "decline").size)
    }

    // --- provisional records: saved before the offer is posted [Task 12 fix 1] ---

    /** The process dies after the POST reached the edge, before the record was promoted. */
    @Test
    fun crash_after_post_before_promote_adopts_the_same_offer() {
        val cid = alice.provisional()
        // the POST reached the edge; the phone never saw (or saved) the reply
        runBlocking { edge.createOffer("xbt_to_btc", XBT_SAT, 100, false, cid) }
        assertEquals(1, edge.offers.size)

        val r = alice.step()
        assertEquals(Step.WAITING_MATCH, r.step)
        val offer = edge.offers.values.single()
        assertEquals(offer.id, r.id)
        assertEquals(offer.secret, r.ownerSecret)
        assertEquals(cid, r.clientId)
        assertNull(runBlocking { alice.store.get(cid) })
        assertEquals(1, runBlocking { alice.store.all() }.size)

        // ...and it carries on normally
        alice.id = r.id
        bob.offer()
        val m = edge.pair(alice.id, bob.id)
        assertEquals(Step.MATCHED, alice.step().step)
        assertEquals(1, edge.bodies(m.id, "initiator", "accept").size)
        assertEquals(2, edge.offers.size)
    }

    /** The POST went through but its response was lost: a retry adopts the offer. */
    @Test
    fun lost_response_retry_adopts_the_same_offer() {
        val cid = alice.provisional(private = true)
        edge.dropCreateResponse = true
        assertTrue(runCatching { runBlocking { alice.engine.postOffer(cid) } }.isFailure)
        assertEquals(Step.PROVISIONAL, runBlocking { alice.store.get(cid)!! }.step)
        assertEquals(1, edge.offers.size)

        edge.dropCreateResponse = false
        val r = runBlocking { alice.engine.postOffer(cid) }
        assertEquals(Step.WAITING_MATCH, r.step)
        assertEquals(edge.offers.values.single().id, r.id)
        assertEquals("CODE-${r.id}", r.shareCode)
        assertEquals(1, edge.offers.size)
        assertEquals(2, edge.createCalls)

        // a stale pass over the old id finds the promoted record and posts nothing more
        assertEquals(r.id, runBlocking { alice.engine.step(cid) }.id)
        assertEquals(1, edge.offers.size)
        assertEquals(2, edge.createCalls)
    }

    /** A record that never got its offer posted is retried, and never funds. */
    @Test
    fun a_provisional_record_never_funds() {
        val cid = alice.provisional()
        edge.createDown = true
        repeat(3) {
            val r = alice.step()
            assertEquals(Step.PROVISIONAL, r.step)
            assertNotNull(r.error)
            chain.mine(1)
        }
        assertEquals(0, alice.funder.calls)
        assertTrue(chain.fundingAttempts.isEmpty())
        assertTrue(edge.offers.isEmpty())
        assertEquals(cid, alice.record().id)
    }

    /** Cancelled while provisional: the engine adopts the offer, then takes it down. */
    @Test
    fun cancel_while_provisional_adopts_then_cancels() {
        val cid = alice.provisional()
        edge.dropCreateResponse = true
        runCatching { runBlocking { alice.engine.postOffer(cid) } }
        edge.dropCreateResponse = false
        assertTrue(runBlocking { alice.engine.cancelProvisional(cid) })

        val r = alice.step()
        assertEquals(Step.CANCELLED, r.step)
        val offer = edge.offers.values.single()
        assertEquals(offer.id, r.id)
        assertEquals("cancelled", offer.status)
        assertEquals(listOf(offer.id), edge.cancelCalls)
        assertEquals(0, alice.funder.calls)
    }

    /** Cancelled while provisional, and the adopted offer was matched meanwhile:
     *  the match is declined and the offer still comes down. */
    @Test
    fun cancel_while_provisional_declines_a_match_made_meanwhile() {
        val cid = alice.provisional()
        runBlocking { edge.createOffer("xbt_to_btc", XBT_SAT, 100, false, cid) }
        bob.offer()
        val m = edge.pair(edge.offers.values.first { it.clientId == cid }.id, bob.id)
        assertTrue(runBlocking { alice.engine.cancelProvisional(cid) })

        val r = alice.step()
        assertEquals(Step.CANCELLED, r.step)
        assertEquals("cancelled", m.status)
        assertEquals(1, edge.bodies(m.id, "initiator", "decline").size)
        assertEquals("cancelled", edge.offers.getValue(r.id).status)
        assertTrue(edge.bodies(m.id, "initiator", "accept").isEmpty())
    }

    /** Too late to cancel as provisional once promoted: the caller cancels the offer instead. */
    @Test
    fun cancel_provisional_is_false_once_promoted() {
        val cid = alice.provisional()
        alice.step()
        assertFalse(runBlocking { alice.engine.cancelProvisional(cid) })
    }

    /** The edge refusing the offer (out of range, struck out) is final: no offer exists. */
    @Test
    fun a_refused_post_fails_the_provisional_record() {
        alice.provisional()
        edge.refuseCreate = 429
        val r = alice.step()
        assertEquals(Step.FAILED, r.step)
        assertTrue(edge.offers.isEmpty())
        assertEquals(0, alice.funder.calls)
    }

    @Test
    fun provisional_fields_round_trip() {
        val r = SwapRecord(
            id = "c1", clientId = "c1", privateOffer = true, step = Step.PROVISIONAL,
            fromWalletId = "a", toWalletId = "b", sendChain = "btc", amountSat = 1, slippageBps = 50,
            ownerSecret = "", swapIndex = 3, wrappedXbt = "x", wrappedBtc = "y",
            redeemAddress = "r", refundAddress = "f",
        )
        assertEquals(r, SwapRecord.fromJson(JSONObject(r.toJson().toString())))
        assertTrue(Step.PROVISIONAL.active)
    }

    // --- final review fixes --------------------------------------------------

    /** [Final C1] Fees rise while our BTC redeem sits unconfirmed: each step
     *  re-signs at the live feerates and re-sends (RBF) — a stalled redeem
     *  must never let the participant's refund win at the BTC locktime. */
    @Test
    fun initiator_redeem_is_fee_bumped_while_unconfirmed() {
        val m = bothFunded()
        assertEquals(Step.REDEEMED, alice.step().step)
        val btc = m.btcFunding!!
        assertTrue(chain.spenderOf("btc", btc.txid, btc.vout)!!.endsWith("fee=2"))
        chain.fees = listOf(30, 60)
        var r = alice.step()
        assertEquals(Step.REDEEMED, r.step)
        val bumped = chain.spenderOf("btc", btc.txid, btc.vout)!!
        assertTrue(bumped, bumped.startsWith("redeem|btc|${btc.txid}|1|preimage=$PREIMAGE") && bumped.endsWith("fee=30"))
        // fees unchanged: nothing new to send
        val sent = chain.broadcasts.size
        alice.step()
        assertEquals(sent, chain.broadcasts.size)
        chain.mine(1)
        r = alice.step()
        assertEquals(Step.DONE, r.step)
    }

    /** [Final C1] Within REDEEM_MARGIN_SECS of the BTC locktime an unconfirmed
     *  redeem goes out at the highest feerate variant. */
    @Test
    fun initiator_redeem_near_the_btc_locktime_uses_the_highest_variant() {
        val m = bothFunded()
        assertEquals(Step.REDEEMED, alice.step().step)
        clock = BTC_LOCKTIME - REDEEM_MARGIN_SECS + 60
        alice.step()
        val btc = m.btcFunding!!
        assertTrue(chain.spenderOf("btc", btc.txid, btc.vout)!!.endsWith("fee=10"))
        assertEquals(Step.REDEEMED, alice.record().step)
    }

    /** Up to both accepted with keys exchanged, alice never funding. */
    private fun aliceNoShow(): FakeApi.Match {
        alice.funder.locked = true
        val m = negotiated()
        alice.step() // alice takes bob's keys, can't fund
        bob.step(); bob.step() // bob waits for alice's XBT
        assertEquals(0, bob.funder.calls)
        return m
    }

    /** [Final I1] The initiator never funds: the edge cancels the match and
     *  reopens the participant's offer. The participant follows that offer
     *  (no ghost on the book), keeping its swap keys, and is matched again. */
    @Test
    fun initiator_no_show_returns_the_participant_to_its_reopened_offer() {
        val m = aliceNoShow()
        val before = bob.record()
        clock = INITIATOR_FUND_BY + 1
        edge.lapse(m, reopenInitiator = false, reopenParticipant = true)
        val cancels = edge.cancelCalls.count { it == bob.id } // the remainder check at accept
        val r = bob.step()
        assertEquals(Step.WAITING_MATCH, r.step)
        assertNull(r.matchId); assertNull(r.termsJson); assertNull(r.role); assertNull(r.sessionJson)
        assertNull(r.theirXbtKey); assertNull(r.theirBtcKey); assertNull(r.hashlock); assertNull(r.deadline)
        assertEquals(0L, r.relaySeq)
        assertTrue(r.marks.isEmpty())
        assertEquals(before.id, r.id); assertEquals(before.ownerSecret, r.ownerSecret)
        assertEquals(before.swapIndex, r.swapIndex)
        assertEquals(before.wrappedXbt, r.wrappedXbt); assertEquals(before.wrappedBtc, r.wrappedBtc)
        assertEquals(before.redeemAddress, r.redeemAddress); assertEquals(before.refundAddress, r.refundAddress)
        assertEquals("no cancel once the match ended", cancels, edge.cancelCalls.count { it == bob.id })
        assertEquals("open", edge.offers.getValue(bob.id).status)

        // a new initiator comes along: bob's record follows the new match
        clock = MATCHED_AT + 10 // the fake's terms are fixed: be inside the new match's accept window
        val carol = Phone("xbt")
        carol.offer()
        val m2 = edge.pair(carol.id, bob.id)
        val again = bob.step()
        assertEquals(m2.id, again.matchId)
        assertEquals(Step.MATCHED, again.step)
        assertEquals(1, edge.bodies(m2.id, "participant", "accept").size)
        assertEquals(0, bob.funder.calls)
    }

    /** [Final I1] The match ended and our offer is not back on the book
     *  (cancelled for good, or matched elsewhere is impossible here): stop,
     *  unfunded, taking the offer down. */
    @Test
    fun participant_whose_offer_is_not_reopened_ends_cancelled() {
        val m = aliceNoShow()
        edge.lapse(m, reopenInitiator = false, reopenParticipant = false)
        val r = bob.step()
        assertEquals(Step.CANCELLED, r.step)
        assertTrue(bob.id in edge.cancelCalls)
        assertEquals(0, bob.funder.calls)
    }

    /** [Final I1] The edge purged the ended match (404): same handling. */
    @Test
    fun match_404_refollows_the_reopened_offer() {
        val m = aliceNoShow()
        edge.lapse(m, reopenInitiator = false, reopenParticipant = true)
        edge.goneMatches += m.id
        val r = bob.step()
        assertEquals(Step.WAITING_MATCH, r.step)
        assertNull(r.matchId)
        assertEquals(0, bob.funder.calls)
    }

    /** [Final I1] Match and offer both purged (404): end, unfunded. */
    @Test
    fun match_and_offer_404_ends_cancelled() {
        val m = aliceNoShow()
        edge.lapse(m, reopenInitiator = false, reopenParticipant = true)
        edge.goneMatches += m.id
        edge.goneOffers += bob.id
        val r = bob.step()
        assertEquals(Step.CANCELLED, r.step)
        assertEquals(0, bob.funder.calls)
    }

    /** [Final I1] An ended match is closed (the declared XBT funding vanished):
     *  our offer stays matched to it, so there is nothing to follow — stop. */
    @Test
    fun closed_match_with_our_offer_still_matched_ends_cancelled() {
        val m = aliceNoShow()
        m.status = "closed"
        val r = bob.step()
        assertEquals(Step.CANCELLED, r.step)
        assertEquals(0, bob.funder.calls)
    }

    /** [Final I2] The counterparty never accepts: at accept_by the edge
     *  dissolves the match and reopens both offers; we (who accepted) follow ours. */
    @Test
    fun counterparty_accept_timeout_refollows() {
        val m = matched()
        alice.step() // alice accepts; bob's phone never shows up
        assertTrue(ACCEPTED_MARK in alice.record().marks)
        clock = ACCEPT_BY + 1
        edge.lapse(m, reopenInitiator = true, reopenParticipant = true)
        val cancels = edge.cancelCalls.count { it == alice.id } // the remainder check at accept
        val r = alice.step()
        assertEquals(Step.WAITING_MATCH, r.step)
        assertNull(r.matchId)
        assertEquals("no cancel once the match ended", cancels, edge.cancelCalls.count { it == alice.id })
        assertEquals("open", edge.offers.getValue(alice.id).status)
        assertEquals(0, alice.funder.calls)
    }

    /** [Final I2] A record's own cancel (the user's) still ends it. */
    @Test
    fun user_cancel_of_a_refollowed_offer_ends_the_record() {
        val m = matched()
        alice.step()
        edge.lapse(m, reopenInitiator = true, reopenParticipant = true)
        assertEquals(Step.WAITING_MATCH, alice.step().step)
        runBlocking { edge.cancelOffer(alice.id, alice.record().ownerSecret) } // as WalletViewModel.cancelOffer
        assertEquals(Step.CANCELLED, alice.step().step)
    }

    /** [Final I3] The edge refuses a decline once both accepted (409): too late
     *  to decline. Never fund; follow the match until it lapses, then stop and
     *  take the reopened offer down. No crash. */
    @Test
    fun decline_refused_as_too_late_does_not_fund_and_stops_when_the_match_ends() {
        val m = matched()
        alice.step() // alice accepts
        // bob's phone posted its accept but died before saving that it had
        runBlocking { edge.post(m.id, m.participantSecret, JSONObject().put("type", "accept")) }
        assertEquals("accepted", m.status)
        bob.price = PRICE * 105 / 100 // and the price moved meanwhile
        var r = bob.step()
        assertTrue(r.step.active)
        assertTrue(r.error!!, r.error.contains("too late to decline"))
        assertTrue(edge.bodies(m.id, "participant", "decline").isEmpty())
        assertTrue(edge.bodies(m.id, "participant", "keys").isEmpty())
        alice.step() // alice can't fund without bob's keys; the match lapses
        r = bob.step()
        assertTrue(r.step.active)
        assertEquals(0, bob.funder.calls)
        edge.lapse(m, reopenInitiator = false, reopenParticipant = true)
        r = bob.step()
        assertEquals(Step.CANCELLED, r.step)
        assertEquals("cancelled", edge.offers.getValue(bob.id).status)
        assertEquals(0, bob.funder.calls)
    }

    /** [Final minor] A record's toString never prints its secrets. */
    @Test
    fun record_to_string_redacts_secrets() {
        val r = SwapRecord(
            id = "o1", fromWalletId = "a", toWalletId = "b", sendChain = "xbt", amountSat = 5,
            slippageBps = 50, ownerSecret = "OWNER-SECRET", swapIndex = 2,
            wrappedXbt = "WRAPPED-XBT", wrappedBtc = "WRAPPED-BTC", pendingFundingHex = "SIGNED-FUNDING-HEX",
            pendingFundingTxid = "ptx", redeemAddress = "ra", refundAddress = "rf",
        )
        val s = r.toString()
        for (secret in listOf("OWNER-SECRET", "WRAPPED-XBT", "WRAPPED-BTC", "SIGNED-FUNDING-HEX")) {
            assertFalse(s, secret in s)
        }
        assertTrue(s, "o1" in s && "ptx" in s)
        assertTrue(s, "redacted" in s)
    }
}
