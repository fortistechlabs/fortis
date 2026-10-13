package com.fortis.wallet.swap

import android.util.Log
import com.fortis.wallet.data.ChainCheck
import com.fortis.wallet.data.ChainWatch
import com.fortis.wallet.data.MatchInfo
import com.fortis.wallet.data.Outpoint
import com.fortis.wallet.data.Step
import com.fortis.wallet.data.SwapApi
import com.fortis.wallet.data.SwapHttpException
import com.fortis.wallet.data.SwapRecord
import com.fortis.wallet.data.SwapStore
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withTimeout
import org.json.JSONArray
import org.json.JSONObject
import uniffi.wallet_ffi.HtlcInfo
import uniffi.wallet_ffi.SwapPubkeys
import java.util.concurrent.ConcurrentHashMap

/** What the engine tells the user about (see `SwapNotifications`). */
enum class Event { MATCHED, NEED_FUNDING, THEY_FUNDED, DONE, FAILED, REFUNDING }

/**
 * Pays our HTLC from the swap's from-wallet: the HTLC amount plus the
 * commission (0.5%, at least 400 sat) output. Must save `pendingFundingTxid` **and** `pendingFundingHex`
 * (the signed tx) to the store **before** it broadcasts, so a funding that
 * isn't seen yet is re-sent as is rather than planned again. Returns the
 * funding txid, or `null` when the wallet is locked.
 */
fun interface Funder {
    suspend fun fund(r: SwapRecord, htlc: HtlcInfo): String?
}

/** The terms fields the engine reads (`wallet_core::SwapTerms` JSON). */
private class Terms(json: String) {
    private val o = JSONObject(json)
    val xbtSat = o.getLong("xbt_sat")
    val btcSat = o.getLong("btc_sat")
    val feeAddress: String = o.getString("fee_address")
    val xbtLocktime = o.getLong("xbt_locktime")
    val btcLocktime = o.getLong("btc_locktime")
    val xbtConfs = o.getLong("xbt_confs")
    val btcConfs = o.getLong("btc_confs")
    val acceptBy = o.getLong("accept_by")
    val initiatorFundBy = o.getLong("initiator_fund_by")
    /** The HTLC version (2 has the cooperative release); terms from before versions are 1. */
    val contractVersion = o.optInt("contract_version", 1)
}

/** Initiator: the release was asked of the participant. */
private const val RELEASE_ASKED = "release_asked"
/** Initiator: the release went out — its coins are on the way back. */
internal const val RELEASED = "released"
/** Participant: it signed the initiator's coins back, and so never funds this swap. */
internal const val RELEASE_SIGNED = "release_signed"
/** Participant: no release can be owed (the initiator never locked anything, or we did). */
private const val RELEASE_NONE = "release_none"

/** How long past the initiator's funding deadline its funding may still turn up at the edge. */
private const val RELEASE_GRACE_SECS = 3600L

/**
 * A cancelled swap whose participant still owes the initiator a release (v2):
 * the participant's own coins never left (nothing funded, nothing in flight),
 * it has the session to sign with, and the XBT locktime hasn't come (after
 * it, the initiator just refunds). Stepped although it is finished.
 */
internal fun releaseDue(r: SwapRecord, now: Long): Boolean {
    if (r.role != "participant" || r.step != Step.CANCELLED) return false
    if (RELEASE_SIGNED in r.marks || RELEASE_NONE in r.marks || COUNTERPARTY !in r.marks) return false
    if (r.fundingTxid != null || r.pendingFundingTxid != null || r.rejectedFundings.isNotEmpty()) return false
    if (r.sessionJson == null || r.wrappedXbt.isBlank()) return false
    val t = r.termsJson?.let { runCatching { Terms(it) }.getOrNull() } ?: return false
    return t.contractVersion >= 2 && now < t.xbtLocktime
}

/** The swaps to step now: every active one, and the finished ones that owe a release. */
suspend fun SwapStore.due(now: Long): List<SwapRecord> = all().filter { it.step.active || releaseDue(it, now) }

// One-shot milestones, kept in `SwapRecord.marks` so a re-run never repeats them.
/** Trace lines kept per swap while the edge can't be reached. */
private const val TRACE_BUFFER = 200
/** Lines per `POST …/log` (the edge's maximum). */
private const val LOG_CHUNK = 50
/** The most sending one step's trace may take. */
private const val LOG_TIMEOUT_MS = 10_000L

internal const val ACCEPTED = "accepted"
internal const val REMAINDER_CANCELLED = "remainder_cancelled"
private const val COUNTERPARTY = "counterparty"
internal const val FUNDED_POSTED = "funded_posted"
private const val REFUND_WATCHED = "refund_watched"
private const val REDEEM_WATCHED = "redeem_watched"

/** `close:<STEP>|<why>`: an unfunded swap decided to stop; finishing it (taking
 *  our offer off the book first) is retried until it goes through. */
private const val CLOSE = "close:"

/** `walk_away:<STEP>|<why>`: we refused the terms, but the edge said it was too
 *  late to decline (both had accepted). Never fund; once the match ends (the
 *  funding deadline lapses), stop as `close:` would — taking down our offer,
 *  which the edge may have put back on the book. */
private const val WALK_AWAY = "walk_away:"

/** One-shot milestones of the match a record follows: dropped when it re-follows its offer. */
private val MATCH_MARKS = setOf(ACCEPTED, REMAINDER_CANCELLED, COUNTERPARTY, FUNDED_POSTED, REFUND_WATCHED, REDEEM_WATCHED)

/** A match that is over on the edge. */
private fun ended(status: String) = status == "cancelled" || status == "closed"

private const val INITIATOR = "initiator"

private const val CANCELLED_BY_USER = "cancelled by you"

/**
 * A terms check that can't be completed (the edge's `/pricing`
 * unreachable) is retried, not declined — until accept_by is this close, when
 * waiting longer only lets the match die: then decline.
 */
private const val ACCEPT_GRACE_SECS = 20L

/**
 * The initiator never *starts* its BTC redeem later than this before the BTC
 * locktime. Revealing the preimage when the participant can already refund (or
 * soon can) invites a race: the participant refunds its BTC and still redeems
 * our XBT with the preimage. Two hours covers the cheap (6-block) fee variant
 * confirming, plus slack; past it the initiator waits for its own XBT refund at
 * the XBT locktime (and the participant refunds its BTC) — both sides whole.
 * A redeem already broadcast (preimage public) is always re-sent.
 */
const val REDEEM_MARGIN_SECS = 2 * 3600L

/** The node will never take this tx (its inputs are spent or it is invalid):
 *  the only case in which a pending funding may be given up and planned again.
 *  Not `txn-mempool-conflict`: a conflict with an *unconfirmed* tx proves
 *  nothing (ours may be the one that confirms), and planning again could fund
 *  twice. */
internal fun definitiveReject(msg: String): Boolean {
    val m = msg.lowercase()
    return listOf(
        "missingorspent", "missing-inputs", "missing inputs", "inputs-spent",
        "mandatory-script-verify-flag-failed",
    ).any { it in m }
}

/** The node already has this tx: a re-send that succeeded. */
private fun alreadyKnown(msg: String): Boolean {
    val m = msg.lowercase()
    return listOf("already in", "already known", "txn-already", "already-in").any { it in m }
}

/** The edge refused a new offer outright (bad amount, struck out): no offer was
 *  made, so a provisional record behind it can stop. */
private fun refusedOffer(e: SwapHttpException) = e.code == 400 || e.code == 429 || indexUsed(e)

/** An RBF re-send the node turns down because ours (or an equal one) is
 *  already there, or the replacement doesn't pay more: nothing to do. */
private fun replacementNoop(msg: String): Boolean {
    val m = msg.lowercase()
    return alreadyKnown(msg) || listOf("insufficient fee", "rejecting replacement", "insufficient-fee").any { it in m }
}

private suspend fun SwapApi.cancelOfferIfOpen(id: String, secret: String) {
    try {
        cancelOffer(id, secret)
    } catch (e: SwapHttpException) {
        // 409: not open (fully matched, already cancelled or expired); 404: gone
        if (e.code != 409 && e.code != 404) throw e
    }
}

/**
 * Drives one swap a step at a time. [step] is idempotent: it re-reads the record,
 * does whatever is due now, saves after every milestone and returns. Calling it
 * again — from the foreground service, the worker, or after the process died
 * anywhere in between — carries on from what was saved, never repeating a
 * funding or a message.
 *
 * The flow mirrors the edge's end-to-end driver (`crates/fortis-edge/tests/swap_e2e.rs`):
 * match → accept + keys (+ hashlock) → initiator funds XBT → participant checks
 * it (exact script and amount, 6 confirmations) and funds BTC → initiator checks
 * that (2 confirmations) and redeems BTC, revealing the preimage → participant
 * takes the preimage from that redeem and redeems XBT. Every funded side also
 * hands its refund to the edge's watchtower, and refunds itself once its
 * locktime passes with its HTLC unspent.
 */
class SwapEngine(
    private val api: SwapApi,
    private val store: SwapStore,
    private val chain: ChainWatch,
    /** Independent explorers: what the edge says about a funding is believed
     *  only once one of them agrees, and the preimage-revealing redeem goes out
     *  through them too (see [ChainCheck]). */
    private val independent: ChainCheck,
    private val crypto: (SwapRecord) -> SwapCrypto,
    private val funder: Funder,
    private val now: () -> Long,
    private val notify: (SwapRecord, Event) -> Unit,
) {
    private val locks = ConcurrentHashMap<String, Mutex>()

    private fun lock(id: String) = locks.getOrPut(id) { Mutex() }

    /**
     * A provisional record is first posted (or re-posted) with its client id,
     * which the edge answers with the same offer however often it is asked, and
     * promoted to its offer id; then driven under that id. One the edge refuses
     * outright is FAILED. A provisional record never gets further than that.
     */
    suspend fun step(id: String): SwapRecord {
        val r = store.find(id) ?: error("no swap $id")
        if (r.step != Step.PROVISIONAL) return drive(r.id)
        val posted = try {
            postOffer(id)
        } catch (e: CancellationException) {
            throw e
        } catch (e: SwapHttpException) {
            if (!refusedOffer(e)) return noteProvisional(id, e)
            val why = e.message ?: e.toString()
            val after = store.update(id) { if (it.step == Step.PROVISIONAL) it.copy(step = Step.FAILED, error = why) else it }
                ?: return store.find(id) ?: error("no swap $id")
            if (after.step == Step.FAILED) notify(after, Event.FAILED)
            return after
        } catch (e: Exception) {
            return noteProvisional(id, e)
        }
        return drive(posted.id)
    }

    /** Keep why a post failed on the provisional record (unless it was promoted meanwhile). */
    private suspend fun noteProvisional(id: String, e: Exception): SwapRecord =
        store.update(id) { if (it.step == Step.PROVISIONAL) it.copy(error = e.message ?: e.toString()) else it }
            ?: store.find(id) ?: error("no swap $id")

    /**
     * POST the provisional record [clientId]'s offer with its client id and
     * promote the record (offer id, owner secret, share code; WAITING_MATCH),
     * carrying any marks over (a cancel the user asked for meanwhile). A repeat
     * adopts the offer an earlier POST made. Returns the promoted record (as is,
     * if promoted already); throws if the POST fails, leaving it provisional.
     */
    suspend fun postOffer(clientId: String): SwapRecord = lock(clientId).withLock {
        val p = store.get(clientId)?.takeIf { it.step == Step.PROVISIONAL }
            ?: return@withLock store.find(clientId) ?: error("no swap $clientId")
        val direction = if (p.sendChain == "xbt") "xbt_to_btc" else "btc_to_xbt"
        val o = api.createOffer(direction, p.amountSat, p.limitPrice, p.privateOffer, p.clientId ?: clientId, p.recoveryTag)
        store.promote(clientId) {
            it.copy(
                id = o.offerId, ownerSecret = o.ownerSecret, shareCode = o.shareCode,
                clientId = it.clientId ?: clientId, step = Step.WAITING_MATCH, error = null,
            )
        } ?: error("no swap $clientId")
    }

    /**
     * The user cancels a swap whose offer may or may not be on the book yet.
     * `true` if the record is still provisional: the cancel is saved on it, and
     * its next step adopts the offer and takes it down. `false` once promoted
     * (cancel the offer by its id instead).
     */
    suspend fun cancelProvisional(clientId: String): Boolean = lock(clientId).withLock {
        store.update(clientId) {
            if (it.step == Step.PROVISIONAL) it.copy(marks = it.marks + "${CLOSE}${Step.CANCELLED.name}|$CANCELLED_BY_USER") else it
        }?.step == Step.PROVISIONAL
    }

    /** Each swap's trace lines not yet sent to the edge (see [flushLog]). */
    private val traces = ConcurrentHashMap<String, ArrayDeque<String>>()
    private val dropped = ConcurrentHashMap<String, Int>()

    /**
     * One line per decision: to logcat (`adb logcat -s SwapEngine`) and, after
     * the step, to the edge's journal under the swap's offer. Ids, steps,
     * statuses, outpoints and reasons only — never secrets or keys.
     * android.util.Log is a stub under JVM unit tests: logging never fails a step.
     */
    private fun trace(id: String, msg: String) {
        // Logcat in debug builds only: release builds keep outpoints off the device log.
        if (com.fortis.wallet.BuildConfig.DEBUG) runCatching { Log.i("SwapEngine", "${id.take(8)}: $msg") }
        val q = traces.getOrPut(id) { ArrayDeque() }
        synchronized(q) {
            if (q.size >= TRACE_BUFFER) {
                q.removeFirst()
                dropped.merge(id, 1, Int::plus)
            }
            q.addLast(msg)
        }
    }

    /** A note in [id]'s trace from outside a step (the runner's time limit). */
    fun note(id: String, msg: String) = trace(id, msg)

    /**
     * Send [r]'s buffered trace to the edge, best-effort: a failure (or the
     * 10 s limit) keeps the unsent lines for the next step, and never fails or
     * holds up this one. A swap with no owner secret yet (provisional) keeps
     * its lines until it has an offer to log under.
     */
    private suspend fun flushLog(r: SwapRecord) {
        if (r.ownerSecret.isBlank()) return
        val q = traces[r.id] ?: return
        val lines = synchronized(q) {
            val skipped = dropped.remove(r.id)
            buildList {
                skipped?.let { add("($it earlier trace lines dropped)") }
                addAll(q); q.clear()
            }
        }
        var sent = 0
        try {
            for (chunk in lines.chunked(LOG_CHUNK)) {
                withTimeout(LOG_TIMEOUT_MS) { api.log(r.id, r.ownerSecret, chunk) }
                sent += chunk.size
            }
        } catch (e: TimeoutCancellationException) {
            requeue(r.id, lines.drop(sent))
        } catch (e: CancellationException) {
            requeue(r.id, lines.drop(sent))
            throw e
        } catch (e: Exception) {
            if (com.fortis.wallet.BuildConfig.DEBUG) {
                runCatching { Log.w("SwapEngine", "${r.id.take(8)}: sending the trace failed: ${e.message}") }
            }
            requeue(r.id, lines.drop(sent))
        }
    }

    /** Put unsent [lines] back in front of anything traced meanwhile, within the cap. */
    private fun requeue(id: String, lines: List<String>) {
        if (lines.isEmpty()) return
        val q = traces.getOrPut(id) { ArrayDeque() }
        synchronized(q) {
            val all = lines + q
            q.clear()
            val over = all.size - TRACE_BUFFER
            if (over > 0) dropped.merge(id, over, Int::plus)
            q.addAll(all.takeLast(TRACE_BUFFER))
        }
    }

    private suspend fun drive(id: String): SwapRecord = lock(id).withLock {
        val start = store.get(id) ?: error("no swap $id")
        trace(id, "step ${start.step} role=${start.role} marks=${start.marks}")
        if (!start.step.active) {
            if (!releaseDue(start, now())) return@withLock start
            val run = Run(start)
            return@withLock try {
                run.release()
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                trace(id, "release failed: ${e.message ?: e}")
                (store.get(id) ?: start).copy(error = e.message ?: e.toString()).also { store.put(it) }
            } finally {
                run.close()
            }
        }
        val run = Run(start.copy(error = null))
        val result = try {
            run.go()
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            // The store is the truth (the funder writes to it too); keep it, note why.
            trace(id, "step failed: ${e.message ?: e}")
            (store.get(id) ?: start).copy(error = e.message ?: e.toString()).also { store.put(it) }
        }
        trace(id, "→ ${result.step}" + (result.error?.let { " error: $it" } ?: ""))
        flushLog(result)
        result
    }

    private inner class Run(var r: SwapRecord) {
        private var session: SwapCrypto? = null

        /** Opened once the record has terms, and only when first needed. */
        private val c: SwapCrypto get() = session ?: crypto(r).also { session = it }

        private val initiator get() = r.role == INITIATOR
        private val matchId get() = r.matchId!!

        suspend fun go(): SwapRecord = try {
            advance()
        } finally {
            session?.close()
        }

        fun close() {
            session?.close()
        }

        /**
         * The participant of a cancelled swap (see [releaseDue]) signs the
         * initiator's XBT back, once asked: only while the edge has the match
         * cancelled with nothing of ours locked. The mark is saved after the
         * post, and with it this record is never stepped again.
         */
        suspend fun release(): SwapRecord {
            var next = r
            for (m in api.messages(matchId, r.ownerSecret, r.relaySeq)) {
                next = next.copy(relaySeq = maxOf(next.relaySeq, m.seq))
                if (m.from == INITIATOR && m.body.optString("type") == "release_request") {
                    val a = m.body.getJSONArray("txs")
                    next = next.copy(releaseTxs = (0 until a.length()).map { a.getString(it) })
                }
            }
            save(next)
            val t = Terms(r.termsJson!!)
            // No release can be owed: the match is gone, our BTC is locked, or
            // the initiator never locked its XBT (and its deadline is long past).
            val info = matchInfo() ?: return save(r.copy(marks = r.marks + RELEASE_NONE))
            val neverFunded = info.xbtFunding == null && now() > t.initiatorFundBy + RELEASE_GRACE_SECS
            if (info.btcFunding != null || neverFunded) return save(r.copy(marks = r.marks + RELEASE_NONE))
            if (r.releaseTxs.isEmpty() || info.status != "cancelled") return r
            val op = info.xbtFunding ?: return r
            val sigs = r.releaseTxs.map { c.signRelease(it, op.txid, op.vout) }
            try {
                api.post(matchId, r.ownerSecret, JSONObject().put("type", "release").put("sigs", JSONArray(sigs)))
            } catch (e: SwapHttpException) {
                if (e.code != 409) throw e
                trace(r.id, "the edge took no release: ${e.message}")
            }
            trace(r.id, "released the initiator's XBT")
            return save(r.copy(marks = r.marks + RELEASE_SIGNED, error = null))
        }

        /**
         * The initiator, its XBT locked in a cancelled match (v2): ask the
         * participant for a release, and once its signatures come, finish and
         * send one — the coins come back now instead of at the locktime. A
         * release that doesn't check out is never sent; the refund at the
         * locktime stays. Non-null: refunding.
         */
        private suspend fun releaseBack(): SwapRecord? {
            if (RELEASE_ASKED !in r.marks) {
                val txs = r.releaseTxs.ifEmpty { c.releaseTxs(chain.feerates(r.sendChain)).also { save(r.copy(releaseTxs = it)) } }
                try {
                    api.post(matchId, r.ownerSecret, JSONObject().put("type", "release_request").put("txs", JSONArray(txs)))
                } catch (e: SwapHttpException) {
                    if (e.code != 409) throw e
                    // a different request already recorded (we lost ours): the relay
                    // carries it, and pollRelay adopts it
                    trace(r.id, "the edge kept an earlier release request: ${e.message}")
                }
                mark(RELEASE_ASKED)
            }
            pollRelay()?.let { return it }
            if (r.releaseSigs.isEmpty()) return null
            val signed = r.releaseTxs.zip(r.releaseSigs).mapNotNull { (tx, sig) ->
                runCatchingNonCancel { c.finishRelease(tx, sig) }
                    .onFailure { trace(r.id, "a release didn't check out: ${it.message ?: it}") }
                    .getOrNull()
            }
            if (signed.isEmpty()) {
                r = r.copy(error = "the other side's release didn't check out; your coins come back at the refund time")
                return null
            }
            val txid = broadcastAny(r.sendChain, signed)
            save(r.copy(releaseSigned = signed, refundTxid = txid, marks = r.marks + RELEASED))
            return finish(Step.REFUNDING, event = Event.REFUNDING)
        }

        private suspend fun save(next: SwapRecord = r): SwapRecord {
            r = next
            store.put(r)
            return r
        }

        /** Persist the session (it changes as we feed it) along with `next`. */
        private suspend fun saveSession(next: SwapRecord = r) = save(next.copy(sessionJson = c.toJson()))

        private suspend fun finish(step: Step, why: String? = null, event: Event? = null): SwapRecord {
            val settled = step == Step.DONE || step == Step.REFUNDED
            save(
                r.copy(
                    step = step, error = why ?: r.error, deadline = null,
                    // the swap keys are no use once settled
                    wrappedXbt = if (settled) "" else r.wrappedXbt,
                    wrappedBtc = if (settled) "" else r.wrappedBtc,
                ),
            )
            event?.let { notify(r, it) }
            return r
        }

        private suspend fun mark(m: String) = save(r.copy(marks = r.marks + m))

        /** Nothing of ours is, or may yet be, on chain: no funding recorded and
         *  none signed and possibly sent. Only then may the swap stop. */
        private val unfunded get() = r.fundingTxid == null && r.pendingFundingTxid == null

        /**
         * Stop an unfunded swap. The edge puts a cancelled match's offers back
         * on the book (a decline cancels only the decliner's own offer), and
         * this record follows only one match, so take our offer down first. The decision is saved as a mark, so a
         * transient failure here is finished on the next step.
         */
        private suspend fun closeUnfunded(step: Step, why: String, event: Event? = null): SwapRecord {
            check(unfunded) { "refusing to close a swap with a funding in flight" }
            val m = "$CLOSE${step.name}|$why"
            if (m !in r.marks) mark(m)
            api.cancelOfferIfOpen(r.id, r.ownerSecret)
            return finish(step, why, event)
        }

        private suspend fun resumeClose(m: String): SwapRecord {
            val (step, why) = m.removePrefix(CLOSE).split("|", limit = 2)
            val s = Step.valueOf(step)
            return closeUnfunded(s, why, if (s == Step.FAILED) Event.FAILED else null)
        }

        /**
         * A cancel saved while the record was provisional: its offer was posted
         * meanwhile, and may even be matched already. Take it down — declining
         * any match first (which cancels our offer on the edge; the follow-up
         * cancel then 409s harmlessly).
         */
        private suspend fun closeBeforeMatch(m: String): SwapRecord {
            api.offer(r.id, r.ownerSecret).matchId?.let { id ->
                runCatching { api.post(id, r.ownerSecret, JSONObject().put("type", "decline")) }
            }
            return resumeClose(m)
        }

        private suspend fun advance(): SwapRecord {
            // 1. Waiting for a match.
            if (r.matchId == null) {
                r.marks.firstOrNull { it.startsWith(CLOSE) }?.let { return closeBeforeMatch(it) }
                val o = api.offer(r.id, r.ownerSecret)
                val m = o.matchId ?: return if (o.status == "cancelled" || o.status == "expired") {
                    finish(Step.CANCELLED, "offer ${o.status}")
                } else save()
                save(r.copy(matchId = m))
            }
            if (r.termsJson == null) {
                // never funded anything for a match we have no terms of
                val info = matchInfo() ?: return matchEnded("the match is gone")
                save(r.copy(termsJson = info.terms.toString(), role = info.role, step = Step.MATCHED))
                notify(r, Event.MATCHED)
            }
            val t = Terms(r.termsJson!!)
            r.marks.firstOrNull { it.startsWith(CLOSE) }?.let { return resumeClose(it) }

            // A swap rebuilt after a restore (`recoveredRecord`): funded, with no
            // session yet. Rebuild it before any chain work needs it.
            if (r.fundingTxid != null && COUNTERPARTY !in r.marks) restoreSession()?.let { return it }

            // Chain-only work first, once we have funded: an edge outage must
            // never stand between us and a refund or a redeem.
            when (r.step) {
                Step.REFUNDING -> return refundProgress()
                Step.REDEEMED -> return doneCheck(t)
                else -> {}
            }
            if (r.fundingTxid != null) {
                refundIfDue(t)?.let { return it }
                if (!initiator) participantRedeem()?.let { return it }
            }

            val info = matchInfo()
            trace(r.id, "match ${info?.status} xbt=${info?.xbtFunding} btc=${info?.btcFunding}")
            if (info != null && info.status != r.matchStatus) save(r.copy(matchStatus = info.status))
            info ?: return if (unfunded) {
                matchEnded("the match is gone")
            } else {
                save(r.copy(error = "the edge no longer knows this match; our refund stays due at its locktime"))
            }

            // A cancelled match with only our XBT locked: ask for it back at once.
            if (initiator && r.fundingTxid != null && r.theirTxid == null && t.contractVersion >= 2 &&
                info.status == "cancelled" && info.btcFunding == null && now() < t.xbtLocktime
            ) {
                // A release that can't be had now (edge refusing, relay down) never
                // holds up the rest: the refund watch, and the refund at the locktime.
                try {
                    releaseBack()?.let { return it }
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    trace(r.id, "release: ${e.message ?: e}")
                    r = r.copy(error = "asking for an early release failed (${e.message ?: e}); retried next step")
                }
            }

            // Too late to decline: we never fund, and wait for the match to end.
            if (unfunded && r.marks.any { it.startsWith(WALK_AWAY) }) {
                return if (ended(info.status)) matchEnded("the match ended") else save()
            }

            // 2. Review the terms; accept + keys (+ hashlock), or decline.
            if (ACCEPTED !in r.marks) {
                if (ended(info.status)) return matchEnded("the match was cancelled")
                review(t)?.let { return it }
            }
            if (REMAINDER_CANCELLED !in r.marks) cancelRemainder()

            // 3. The counterparty's keys and the hashlock, from the relay.
            if (COUNTERPARTY !in r.marks) {
                pollRelay()?.let { return it }
                val hashlock = if (initiator) c.hashlock() else r.hashlock
                val (x, b) = r.theirXbtKey to r.theirBtcKey
                if (x == null || b == null || hashlock == null) {
                    return if (unfunded && ended(info.status)) matchEnded("the match was cancelled") else save()
                }
                c.setCounterparty(SwapPubkeys(x, b), hashlock)
                saveSession(r.copy(marks = r.marks + COUNTERPARTY))
            }

            // 4–5. Funding: the participant checks the initiator's first, then funds.
            if (!initiator) theirFunding(t, info)
            if (r.fundingTxid == null) fund(t, info)?.let { return it }
            afterFunding(t, info)
            if (initiator) {
                theirFunding(t, info)
                // 6. The initiator redeems BTC once it has its confirmations.
                if (r.theirTxid != null) return initiatorRedeem(t)
            }
            return save()
        }

        /**
         * A recovered swap's session: the counterparty's keys, the hashlock and
         * (the initiator's) preimage nonce from the relay, kept by the edge from
         * the start — the preimage is rebuilt from the seed, the match and the
         * nonce, and must hash to the relayed hashlock — then our funding. That funding must be exactly the HTLC this session
         * builds — the wrong seed or terms the edge altered would sign for
         * another contract — or the swap stops, its keys kept. Non-null: stop
         * here this step.
         */
        private suspend fun restoreSession(): SwapRecord? {
            pollRelay()
            val (x, b) = r.theirXbtKey to r.theirBtcKey
            val hashlock = r.hashlock
            if (x == null || b == null || hashlock == null || (initiator && r.preimageNonce == null)) {
                return save(r.copy(error = "recovering: waiting for the swap's keys from the relay"))
            }
            // Our funding as an independent explorer has it, spent or not: the
            // edge can neither pass a wrong contract nor fail a good one.
            val (txid, vout) = r.fundingTxid!! to r.fundingVout!!
            val out = try {
                independent.output(r.sendChain, txid, vout)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                return save(r.copy(error = "recovering: no independent explorer answered (${e.message ?: e}); waiting"))
            } ?: return save(r.copy(error = "recovering: our funding $txid:$vout is not found at an independent explorer; waiting"))
            // The session opens here, with the nonce: the initiator's hashlock
            // must be its own preimage's, so a forged nonce stops it.
            try {
                c.setCounterparty(SwapPubkeys(x, b), hashlock)
            } catch (e: Exception) {
                return finish(Step.FAILED, "the recovered swap's relayed keys don't fit it (${e.message ?: e}); nothing was signed", Event.FAILED)
            }
            val htlc = c.ourHtlc()
            if (!out.scriptPubkeyHex.equals(htlc.scriptPubkeyHex, ignoreCase = true) || out.valueSat != htlc.amountSat.toLong()) {
                return finish(Step.FAILED, "the recovered swap does not match its funding $txid:$vout; nothing was signed", Event.FAILED)
            }
            c.recordOurOutpoint(txid, vout)
            saveSession(r.copy(pendingFundingTxid = txid, marks = r.marks + COUNTERPARTY, error = null))
            return null
        }

        /** Accept, or decline and stop. `null` once accepted. */
        private suspend fun review(t: Terms): SwapRecord? {
            val why: String? = try {
                termsProblem(t)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                // Couldn't check (the edge's /pricing unreachable): not a
                // "no". Retry on the next step — unless accept_by is closing in.
                if (now() + ACCEPT_GRACE_SECS < t.acceptBy) throw e
                "could not check the terms before the accept deadline: ${e.message ?: e}"
            }
            if (why != null) return declineAndStop(Step.CANCELLED, why)
            val keys = runCatching { c.ourPubkeys() }.getOrElse { e ->
                // a session that can't be built (bad address, bad secret) never will be
                return declineAndStop(Step.FAILED, e.message ?: e.toString(), Event.FAILED)
            }
            saveSession()
            api.post(matchId, r.ownerSecret, JSONObject().put("type", "accept"))
            api.post(matchId, r.ownerSecret, JSONObject().put("type", "keys").put("xbt", keys.xbt).put("btc", keys.btc))
            if (initiator) {
                // the nonce too: a phone restored from the seed rebuilds the preimage from it
                api.post(
                    matchId, r.ownerSecret,
                    JSONObject().put("type", "hashlock").put("hex", c.hashlock()!!).put("nonce", c.preimageNonce()!!),
                )
            }
            mark(ACCEPTED)
            return null
        }

        /**
         * Decline the match and stop, taking our offer down (the edge's
         * decline already cancels it; the cancel is a harmless 409 then): we
         * chose to walk away. A decline the edge refuses
         * (409: both sides accepted already, or the match is over) is too late
         * to decline — then we simply never fund, and stop once the match has
         * ended (its funding deadline lapses). A decline that didn't get
         * through (network) is retried on the next step.
         */
        private suspend fun declineAndStop(step: Step, why: String, event: Event? = null): SwapRecord {
            try {
                api.post(matchId, r.ownerSecret, JSONObject().put("type", "decline"))
            } catch (e: SwapHttpException) {
                if (e.code != 409) throw e
                val m = "$WALK_AWAY${step.name}|$why"
                if (m !in r.marks) mark(m)
                return save(r.copy(error = "$why; too late to decline, so we won't fund and the match lapses at its deadline"))
            }
            return closeUnfunded(step, why, event)
        }

        /**
         * The match we followed ended (cancelled, closed, or gone from the
         * edge) with nothing of ours on chain or in flight. If we walked away
         * from it, stop. Otherwise the edge has put our offer back on the book
         * (spec: "the match dissolves and both offers reopen"): follow it again
         * rather than leave a ghost offer that matches into nothing.
         */
        private suspend fun matchEnded(why: String): SwapRecord {
            check(unfunded) { "refusing to leave a match with a funding in flight" }
            r.marks.firstOrNull { it.startsWith(WALK_AWAY) }?.let {
                return resumeClose(CLOSE + it.removePrefix(WALK_AWAY))
            }
            return refollow(why)
        }

        /**
         * Back to [Step.WAITING_MATCH] on our offer if the edge reopened it (or
         * already matched it again); otherwise stop, unfunded. Only for a record
         * that never funded and has nothing pending — not even a funding the
         * node rejected, which belonged to the old match's contract. Keeps the
         * offer, its secret, the swap index, the wrapped secrets and addresses;
         * drops everything that came with the old match.
         */
        private suspend fun refollow(why: String): SwapRecord {
            val o = try {
                api.offer(r.id, r.ownerSecret)
            } catch (e: SwapHttpException) {
                if (e.code != 404) throw e
                null
            }
            val reopened = o != null && o.matchId != r.matchId &&
                (o.status == "open" || (o.status in setOf("matched", "filled") && o.matchId != null))
            if (!reopened || r.rejectedFundings.isNotEmpty()) return closeUnfunded(Step.CANCELLED, why)
            return save(
                r.copy(
                    matchId = null, role = null, termsJson = null, sessionJson = null,
                    theirXbtKey = null, theirBtcKey = null, hashlock = null, relaySeq = 0,
                    theirTxid = null, theirVout = null, redeemTxid = null, refundTxid = null,
                    presigned = emptyList(), deadline = null, step = Step.WAITING_MATCH,
                    marks = r.marks.filterNot { it in MATCH_MARKS || it.startsWith(WALK_AWAY) }.toSet(),
                    error = "$why; your offer is back on the book",
                ),
            )
        }

        /** The match as the edge has it; `null` once the edge no longer knows
         *  it (purged a week after it ended). */
        private suspend fun matchInfo(): MatchInfo? = try {
            api.match(matchId, r.ownerSecret)
        } catch (e: SwapHttpException) {
            if (e.code != 404) throw e
            null
        }

        /**
         * Why the terms are refused — definite answers only — or `null` if they
         * are fine. Throws when a check can't be completed (network or HTTP).
         */
        private suspend fun termsProblem(t: Terms): String? {
            if ((r.sendChain == "xbt") != initiator) return "role ${r.role} does not match the offer"
            val ourAmount = if (initiator) t.xbtSat else t.btcSat
            if (ourAmount > r.amountSat) return "the match asks for more than the offer"
            if (api.pricingAddress() != t.feeAddress) return "the commission address is not the edge's"
            return runCatching { c.validateTerms(r.termsJson!!, r.limitPrice, r.role!!, now()) }
                .exceptionOrNull()?.let { it.message ?: it.toString() }
        }

        /**
         * A partial fill leaves our offer open with the remainder, and the edge
         * would match that again — into a match this record (one per offer)
         * never follows. Take it down once we've accepted; a full fill answers
         * 409 (nothing open). A transient failure is retried next step.
         */
        private suspend fun cancelRemainder() {
            try {
                api.cancelOfferIfOpen(r.id, r.ownerSecret)
                mark(REMAINDER_CANCELLED)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                r = r.copy(error = "could not cancel the offer's remainder: ${e.message ?: e}")
            }
        }

        /** Take in relayed keys, hashlock and deadline. Non-null: the match ended. */
        private suspend fun pollRelay(): SwapRecord? {
            val them = if (initiator) "participant" else INITIATOR
            var next = r
            var declined = false
            for (m in api.messages(matchId, r.ownerSecret, r.relaySeq)) {
                next = next.copy(relaySeq = maxOf(next.relaySeq, m.seq))
                val b = m.body
                when (b.optString("type")) {
                    "keys" -> if (m.from == them) next = next.copy(theirXbtKey = b.getString("xbt"), theirBtcKey = b.getString("btc"))
                    "hashlock" -> if (m.from == INITIATOR) {
                        next = next.copy(hashlock = b.getString("hex"))
                        // our own, read back by a recovered initiator; never replaces one we hold
                        if (initiator && next.preimageNonce == null && b.has("nonce")) next = next.copy(preimageNonce = b.getString("nonce"))
                    }
                    "decline" -> if (m.from == them) declined = true
                    // our own request as the edge recorded it: a phone that lost (or
                    // rebuilt) its own adopts that one, which the signatures fit
                    "release_request" -> if (m.from == INITIATOR && initiator) {
                        val a = b.getJSONArray("txs")
                        next = next.copy(releaseTxs = (0 until a.length()).map { a.getString(it) })
                    }
                    "release" -> if (m.from == them && initiator) {
                        val a = b.getJSONArray("sigs")
                        next = next.copy(releaseSigs = (0 until a.length()).map { a.getString(it) })
                    }
                    // Only the edge sets the participant's funding deadline.
                    "deadline" -> if (m.from == "edge" && !initiator && b.has("participant_fund_by")) next = next.copy(deadline = b.getLong("participant_fund_by"))
                    else -> {} // forward-compatible: ignore what we don't know
                }
            }
            save(next)
            return if (declined && unfunded) matchEnded("the other side declined") else null
        }

        /** Our HTLC output in `txid`, if it's there: exactly the script and amount. */
        private suspend fun ourOutput(txid: String, htlc: HtlcInfo): Int? = (0..3).firstOrNull { v ->
            chain.txout(r.sendChain, txid, v)?.let {
                it.scriptPubkeyHex.equals(htlc.scriptPubkeyHex, ignoreCase = true) && it.valueSat == htlc.amountSat.toLong()
            } == true
        }

        private suspend fun recordFunding(txid: String, vout: Int) {
            c.recordOurOutpoint(txid, vout)
            saveSession(
                r.copy(
                    pendingFundingTxid = txid, pendingFundingHex = null, fundingTxid = txid, fundingVout = vout,
                    deadline = null, step = if (initiator) Step.FUNDED else Step.THEY_FUNDED,
                ),
            )
        }

        /** The latest we may still fund, by role. */
        private fun fundDeadline(t: Terms, info: MatchInfo): Long = if (initiator) {
            t.initiatorFundBy
        } else {
            val latest = c.participantFundLatest(r.termsJson!!)
            minOf(latest, info.participantFundBy ?: r.deadline ?: latest)
        }

        /** Find or make our funding. Non-null: stop here this step. */
        private suspend fun fund(t: Terms, info: MatchInfo): SwapRecord? {
            val htlc = c.ourHtlc()
            // A funding we signed earlier (saved before its broadcast) — or one we
            // gave up on, in case it lands after all …
            for (txid in listOfNotNull(r.pendingFundingTxid) + r.rejectedFundings) {
                ourOutput(txid, htlc)?.let { v -> recordFunding(txid, v); return null }
            }
            // … or, on BTC, one found by address.
            chain.findFunding(r.sendChain, htlc.address, htlc.amountSat.toLong())?.let { (txid, v) ->
                recordFunding(txid, v)
                return null
            }
            // Signed and maybe sent, not seen yet: send that same tx again. Never
            // a new one, never a deadline cancel, while it may still land.
            if (r.pendingFundingTxid != null) return resendPending(htlc)

            // Nothing of ours anywhere: a match that ended (the initiator never
            // funded, a declared funding vanished) is left before any waiting.
            if (ended(info.status)) return matchEnded("the match was ${info.status}")
            // The participant funds only once the initiator's XBT HTLC is verified.
            if (!initiator && r.theirTxid == null) return save()
            // Only into a match both sides accepted: the edge takes keys (and so we
            // can have the counterparty's) while the match is still `proposed`.
            if (info.status != if (initiator) "accepted" else "initiator_funded") return save()
            val deadline = fundDeadline(t, info)
            if (now() > deadline) return closeUnfunded(Step.CANCELLED, "the funding deadline passed")
            val txid = funder.fund(r, htlc)
            if (txid == null) {
                if (r.step == Step.NEED_FUNDING) return save()
                val waiting = save(r.copy(step = Step.NEED_FUNDING, deadline = deadline))
                notify(waiting, Event.NEED_FUNDING) // carries the computed deadline
                return waiting
            }
            // The funder saved the pending txid and hex itself; keep the store's record.
            save((store.get(r.id) ?: r).copy(pendingFundingTxid = txid, error = null))
            val v = ourOutput(txid, htlc) ?: return save() // not visible yet: found (or re-sent) next step
            recordFunding(txid, v)
            return null
        }

        /** Re-send the pending funding as signed. Only a definitive rejection
         *  (its inputs are gone) frees us to plan a new one. */
        private suspend fun resendPending(htlc: HtlcInfo): SwapRecord? {
            val txid = r.pendingFundingTxid!!
            val hex = r.pendingFundingHex ?: return save(r.copy(error = "waiting for our funding $txid to appear"))
            try {
                chain.broadcast(r.sendChain, hex)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                val msg = e.message ?: e.toString()
                if (definitiveReject(msg)) {
                    return save(
                        r.copy(
                            pendingFundingTxid = null, pendingFundingHex = null,
                            rejectedFundings = r.rejectedFundings + txid,
                            error = "the node rejected our funding $txid ($msg); planning a new one",
                        ),
                    )
                }
                if (!alreadyKnown(msg)) return save(r.copy(error = "our funding $txid is not seen yet; re-sending it failed: $msg"))
            }
            ourOutput(txid, htlc)?.let { v -> recordFunding(txid, v); return null }
            return save()
        }

        /** Hand the refund (and the participant's redeem) to the watchtower and
         *  announce our funding — each once, each independently of the others:
         *  the refund goes first, and a refused or failed `funded` never keeps it
         *  from the watchtower. A failure is retried on the next step. */
        private suspend fun afterFunding(t: Terms, info: MatchInfo) {
            val txid = r.fundingTxid ?: return
            val vout = r.fundingVout!!
            var failed: Exception? = null
            suspend fun attempt(part: suspend () -> Unit) = try {
                part()
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                failed = failed ?: e
            }
            if (REFUND_WATCHED !in r.marks) attempt {
                if (r.presigned.isEmpty()) save(r.copy(presigned = c.signedRefund(chain.feerates(r.sendChain))))
                api.watch(
                    r.ownerSecret,
                    watchBody(r.sendChain, "refund", Outpoint(txid, vout), if (initiator) t.xbtLocktime else t.btcLocktime, r.presigned),
                )
                mark(REFUND_WATCHED)
            }
            // The participant announces even on a cancelled match: the edge
            // reinstates one cancelled only for its deadline if this funding was
            // mined in time. A refusal is kept as the error and retried next step.
            if (FUNDED_POSTED !in r.marks && (info.status != "cancelled" || !initiator)) attempt {
                try {
                    api.post(
                        matchId, r.ownerSecret,
                        JSONObject().put("type", "funded").put("chain", r.sendChain).put("txid", txid).put("vout", vout),
                    )
                } catch (e: SwapHttpException) {
                    // 410: mined after the deadline — final. Stop re-announcing; the
                    // reason is kept as the error, and the refund stays watched.
                    if (e.code == 410) mark(FUNDED_POSTED)
                    throw e
                }
                mark(FUNDED_POSTED)
            }
            val theirTxid = r.theirTxid
            if (!initiator && REDEEM_WATCHED !in r.marks && theirTxid != null) attempt {
                val pre = c.presignedRedeem(chain.feerates("xbt"))
                val body = watchBody("xbt", "redeem", Outpoint(theirTxid, r.theirVout!!), 0, pre.map { it.txHex })
                    .put("sigs", JSONArray(pre.map { it.sigHex }))
                    .put("pubkey", pre.first().pubkeyHex)
                    .put("witness_script", pre.first().witnessScriptHex)
                api.watch(r.ownerSecret, body)
                mark(REDEEM_WATCHED)
            }
            failed?.let { e -> r = r.copy(error = e.message ?: e.toString()) }
        }

        private fun watchBody(chainName: String, kind: String, op: Outpoint, notBefore: Long, variants: List<String>) =
            JSONObject()
                .put("match_id", matchId).put("chain", chainName).put("kind", kind)
                .put("htlc_outpoint", JSONObject().put("txid", op.txid).put("vout", op.vout))
                .put("not_before", notBefore)
                .put("variants", JSONArray(variants))

        /** Their announced HTLC: exactly the agreed script and amount, with the
         *  required confirmations. The same txout feeds the check and the record. */
        private suspend fun theirFunding(t: Terms, info: MatchInfo) {
            if (r.theirTxid != null) return
            val op = (if (initiator) info.btcFunding else info.xbtFunding)
                ?: return trace(r.id, "their funding: none declared yet")
            val out = chain.txout(r.recvChain, op.txid, op.vout)
                ?: return trace(r.id, "their funding ${op.txid}:${op.vout}: not visible (or spent)")
            try {
                c.verifyTheirFunding(out.scriptPubkeyHex, out.valueSat)
            } catch (e: Exception) {
                // Never fund (or redeem) against a wrong contract; our refund path stays open.
                save(r.copy(error = "their funding does not match the contract: ${e.message}"))
                return
            }
            val required = if (initiator) t.btcConfs else t.xbtConfs
            if (out.confirmations < required) return trace(r.id, "their funding: ${out.confirmations}/$required confirmations")
            // Funding (participant) or revealing the preimage (initiator) against
            // a funding that isn't there would lose our coins: believe the edge
            // only once an independent explorer shows the same output, unspent,
            // with the confirmations. Unreachable or disagreeing: wait (our
            // refund path stays open), never go ahead.
            val seen = try {
                independent.unspent(r.recvChain, op.txid, op.vout)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                return trace(r.id, "their funding: no independent explorer answered (${e.message ?: e}); waiting")
            }
            if (seen == null || !seen.scriptPubkeyHex.equals(out.scriptPubkeyHex, ignoreCase = true) || seen.valueSat != out.valueSat) {
                return trace(r.id, "their funding ${op.txid}:${op.vout}: an independent explorer doesn't show it as the edge does; waiting")
            }
            if (seen.confirmations < required) {
                return trace(r.id, "their funding: ${seen.confirmations}/$required confirmations at an independent explorer")
            }
            c.recordTheirOutpoint(op.txid, op.vout)
            saveSession(
                r.copy(
                    theirTxid = op.txid, theirVout = op.vout,
                    step = if (initiator) Step.THEY_FUNDED else r.step,
                ),
            )
            notify(r, Event.THEY_FUNDED)
        }

        /**
         * The initiator's BTC redeem, revealing the preimage. Any spend of their
         * HTLC that reveals our preimage is our redeem — whatever fee variant,
         * from whichever run (variants are re-signed at live feerates each time,
         * so comparing hex would miss our own). Any other spend is their refund.
         */
        private suspend fun initiatorRedeem(t: Terms): SwapRecord {
            val (txid, vout) = r.theirTxid!! to r.theirVout!!
            if (chain.txout("btc", txid, vout) != null) {
                if (r.redeemTxid == null && now() > t.btcLocktime - REDEEM_MARGIN_SECS) {
                    return save(
                        r.copy(error = "too close to the BTC locktime to reveal the preimage; our XBT refunds at its locktime"),
                    )
                }
                val variants = c.signedRedeem(c.preimage()!!, chain.feerates("btc"))
                save(r.copy(redeemTxid = broadcastEverywhere("btc", variants.first())))
                watchRedeem(txid, vout, variants)
                return save(r.copy(step = Step.REDEEMED))
            }
            val spender = chain.spendingTxHex("btc", txid, vout) ?: return save() // spent; spender not readable yet
            if (c.preimageFromTx(spender) == null) {
                return save(r.copy(error = "the participant refunded its BTC; our XBT refunds at its locktime"))
            }
            watchRedeem(txid, vout, c.signedRedeem(c.preimage()!!, chain.feerates("btc")))
            return save(r.copy(step = Step.REDEEMED))
        }

        /**
         * Send [hex] through the edge and an independent explorer, so the edge
         * alone can't hold back a redeem: the txid from whichever took it.
         * Throws (the edge's error) only when neither did.
         */
        private suspend fun broadcastEverywhere(on: String, hex: String): String {
            val viaEdge = runCatchingNonCancel { chain.broadcast(on, hex) }
            val viaExplorer = runCatchingNonCancel { independent.broadcast(on, hex) }
            viaEdge.exceptionOrNull()?.let { trace(r.id, "the edge didn't take our $on spend: ${it.message ?: it}") }
            viaExplorer.exceptionOrNull()?.let { trace(r.id, "no independent explorer took our $on spend: ${it.message ?: it}") }
            return viaEdge.getOrNull() ?: viaExplorer.getOrNull() ?: throw viaEdge.exceptionOrNull()!!
        }

        private suspend fun watchRedeem(txid: String, vout: Int, variants: List<String>) {
            if (REDEEM_WATCHED in r.marks) return
            api.watch(r.ownerSecret, watchBody("btc", "redeem", Outpoint(txid, vout), 0, variants))
            mark(REDEEM_WATCHED)
        }

        /** The participant: once our BTC HTLC is spent by the initiator's redeem,
         *  take the preimage from it and redeem XBT. Non-null: redeemed. */
        private suspend fun participantRedeem(): SwapRecord? {
            val theirTxid = r.theirTxid ?: return null
            // The edge, then an independent explorer: an edge hiding the
            // initiator's redeem until the XBT locktime must not cost us the XBT.
            val hex = chain.spendingTxHex("btc", r.fundingTxid!!, r.fundingVout!!)
                ?: runCatchingNonCancel { independent.spendingTxHex("btc", r.fundingTxid!!, r.fundingVout!!) }
                    .onFailure { trace(r.id, "our BTC HTLC's spender: no independent explorer answered (${it.message ?: it})") }
                    .getOrNull()
                ?: return null
            val preimage = c.preimageFromTx(hex) ?: return null // e.g. our own refund
            val variants = c.signedRedeem(preimage, chain.feerates("xbt"))
            // Unspent: redeem it. Spent: the watchtower's presigned redeem got there first.
            if (chain.txout("xbt", theirTxid, r.theirVout!!) != null) {
                save(r.copy(redeemTxid = broadcastEverywhere("xbt", variants.first())))
            }
            return save(r.copy(step = Step.REDEEMED, deadline = null))
        }

        /** Redeemed: done once that spend is confirmed — and, for the initiator,
         *  once it is ours: if the participant's refund won the race instead, our
         *  XBT is still ours to refund, so go back to waiting for that. */
        private suspend fun doneCheck(t: Terms): SwapRecord {
            val (txid, vout) = r.theirTxid!! to r.theirVout!!
            if (!chain.spendConfirmed(r.recvChain, txid, vout)) {
                if (initiator) bumpRedeem(t, txid, vout)
                return save()
            }
            // DONE wipes the swap keys: an independent explorer must see the spend
            // confirmed too (and, for the initiator, say whose it is), or a lying
            // edge could end the swap while our redeem is still unconfirmed.
            val confirmedThere = runCatchingNonCancel { independent.spendConfirmed(r.recvChain, txid, vout) }
                .onFailure { trace(r.id, "our redeem: no independent explorer answered (${it.message ?: it}); waiting") }
                .getOrDefault(false)
            if (!confirmedThere) {
                trace(r.id, "our redeem: not confirmed at an independent explorer yet; waiting")
                if (initiator) bumpRedeem(t, txid, vout)
                return save()
            }
            if (initiator) {
                val spender = runCatchingNonCancel { independent.spendingTxHex("btc", txid, vout) }.getOrNull() ?: return save()
                if (c.preimageFromTx(spender) == null) {
                    return save(
                        r.copy(
                            step = Step.THEY_FUNDED,
                            error = "the participant's refund beat our BTC redeem; our XBT refunds at its locktime",
                        ),
                    )
                }
            }
            return finish(Step.DONE, event = Event.DONE)
        }

        /**
         * Our BTC redeem isn't confirmed yet. The preimage is public, so the
         * participant has its XBT; if ours stalls until the BTC locktime, its
         * refund can replace it and we lose both legs. So each step re-signs at
         * the live feerates and re-sends (the redeem signals RBF): the 6-block
         * variant normally, the highest one within [REDEEM_MARGIN_SECS] of the
         * locktime. Also re-sends a redeem that was evicted. A re-send the node
         * turns down as no better than what it has is fine.
         */
        private suspend fun bumpRedeem(t: Terms, txid: String, vout: Int) {
            val variants = c.signedRedeem(c.preimage()!!, chain.feerates("btc"))
            val best = if (now() > t.btcLocktime - REDEEM_MARGIN_SECS) variants.last() else variants.first()
            if (chain.spendingTxHex("btc", txid, vout) == best) return
            try {
                save(r.copy(redeemTxid = chain.broadcast("btc", best)))
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                val msg = e.message ?: e.toString()
                if (!replacementNoop(msg)) r = r.copy(error = "re-sending our BTC redeem failed: $msg")
            }
        }

        /** Past our locktime with our HTLC still unspent: refund. Non-null: refunding. */
        private suspend fun refundIfDue(t: Terms): SwapRecord? {
            val notBefore = if (initiator) t.xbtLocktime else t.btcLocktime
            if (now() < notBefore) return null
            if (chain.txout(r.sendChain, r.fundingTxid!!, r.fundingVout!!) == null) {
                // Spent past our locktime, with no refund recorded: our refund went
                // out but its save was cut off (a step's time limit, the process
                // dying), or the watchtower sent it. Follow it as refunding; for
                // the participant, refundProgress still takes the initiator's
                // redeem (the preimage) if that is what spent it. Only if an
                // independent explorer agrees: an edge that lies "spent" must not
                // stop us sending our refund.
                val there = try {
                    independent.unspent(r.sendChain, r.fundingTxid!!, r.fundingVout!!)
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    trace(r.id, "our HTLC: the edge says spent, no independent explorer answered (${e.message ?: e}); waiting")
                    return save()
                }
                if (there == null) return finish(Step.REFUNDING, event = Event.REFUNDING)
                trace(r.id, "our HTLC: the edge says spent, an independent explorer shows it unspent; sending our refund")
            }
            val variants = r.presigned.ifEmpty { c.signedRefund(chain.feerates(r.sendChain)) }
            val txid = broadcastAny(r.sendChain, variants)
            save(r.copy(presigned = variants, refundTxid = txid))
            return finish(Step.REFUNDING, event = Event.REFUNDING)
        }

        private suspend fun refundProgress(): SwapRecord {
            // Our BTC refund can lose to the initiator's redeem: then the preimage
            // is on chain and the XBT is ours to redeem — never REFUNDED.
            if (!initiator) participantRedeem()?.let { return it }
            val (txid, vout) = r.fundingTxid!! to r.fundingVout!!
            if (chain.spendConfirmed(r.sendChain, txid, vout)) {
                // REFUNDED wipes the swap keys: settle only when an independent
                // explorer shows the spend confirmed and it is our refund (no
                // preimage in it; a redeem is handled above).
                val ours = runCatchingNonCancel {
                    independent.spendConfirmed(r.sendChain, txid, vout) &&
                        independent.spendingTxHex(r.sendChain, txid, vout)?.let { c.preimageFromTx(it) == null } == true
                }.onFailure { trace(r.id, "our refund: no independent explorer answered (${it.message ?: it}); waiting") }
                    .getOrDefault(false)
                if (ours) return finish(Step.REFUNDED)
                trace(r.id, "our refund: not confirmed as ours at an independent explorer yet; waiting")
            }
            // Released, before our locktime: there is no refund to sign yet. Send
            // the release again while our HTLC is still unspent (evicted, or
            // never seen).
            val notBefore = Terms(r.termsJson!!).let { if (initiator) it.xbtLocktime else it.btcLocktime }
            if (RELEASED in r.marks && now() < notBefore) {
                val stillThere = chain.txout(r.sendChain, txid, vout) != null ||
                    runCatchingNonCancel { independent.unspent(r.sendChain, txid, vout) != null }.getOrDefault(false)
                if (stillThere) {
                    try {
                        save(r.copy(refundTxid = broadcastAny(r.sendChain, r.releaseSigned)))
                    } catch (e: CancellationException) {
                        throw e
                    } catch (e: Exception) {
                        val msg = e.message ?: e.toString()
                        if (!replacementNoop(msg)) r = r.copy(error = "re-sending our release failed: $msg")
                    }
                }
                return save()
            }
            // Not settled: re-sign at the live feerates. Send again when the HTLC
            // is unspent at the edge or an explorer (evicted from the mempool, or
            // never sent because the edge said "spent"), or when fees moved since
            // we last sent (the refund signals RBF): a refund stuck at an old rate
            // gives the counterparty, who knows the preimage, time to take the coins.
            val live = c.signedRefund(chain.feerates(r.sendChain))
            val unspent = chain.txout(r.sendChain, txid, vout) != null ||
                runCatchingNonCancel { independent.unspent(r.sendChain, txid, vout) != null }.getOrDefault(false)
            if (unspent || live.first() != r.presigned.firstOrNull()) {
                try {
                    val sent = broadcastAny(r.sendChain, if (unspent) live else live.take(1))
                    save(r.copy(presigned = live, refundTxid = sent))
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    val msg = e.message ?: e.toString()
                    if (!replacementNoop(msg)) r = r.copy(error = "re-sending our refund failed: $msg")
                }
            }
            return save()
        }

        /** The first variant (cheapest first) the edge or an independent
         *  explorer takes; each is offered to both, so the edge alone can't
         *  hold back a refund. */
        private suspend fun broadcastAny(chainName: String, variants: List<String>): String {
            var last: Throwable? = null
            for (v in variants) {
                val viaEdge = runCatchingNonCancel { chain.broadcast(chainName, v) }
                val viaExplorer = runCatchingNonCancel { independent.broadcast(chainName, v) }
                viaEdge.getOrNull()?.let { return it }
                viaExplorer.getOrNull()?.let { return it }
                last = viaEdge.exceptionOrNull()
            }
            throw last ?: IllegalStateException("nothing to broadcast")
        }
    }
}

/** [runCatching] that lets cancellation through. */
private inline fun <T> runCatchingNonCancel(block: () -> T): Result<T> = try {
    Result.success(block())
} catch (e: CancellationException) {
    throw e
} catch (e: Exception) {
    Result.failure(e)
}
