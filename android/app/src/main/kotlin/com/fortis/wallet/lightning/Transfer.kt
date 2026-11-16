package com.fortis.wallet.lightning

/**
 * Moving funds between an on-chain wallet and the Lightning wallet (spec §2).
 *
 * Fee: `max(1 % of the amount, 1,000 sat, network fee)`, the network fee
 * included, never charged twice. The operator's share is the fee less the
 * network fees.
 *
 * - **To Lightning:** the on-chain wallet sends to the Lightning node's
 *   transit address (one transaction, with the operator's share as an output
 *   to the service-fee address). Once it confirms, the node opens a channel
 *   to the LSP, or splices into its open one.
 * - **To on-chain:** the node splices the amount out of its channel to an
 *   address of the on-chain wallet, then pays the operator's share over
 *   Lightning.
 */

const val TRANSFER_FEE_FLOOR_SAT = 1_000L

/** vbytes of the channel open or splice the node builds (≈ 1 input, 2 outputs, P2WSH funding). */
const val CHANNEL_TX_VBYTES = 200L

/** The smallest output worth creating. */
const val DUST_SAT = 546L

/** The smallest channel the user can fund (fortis-lightning's `USER_CHANNEL_MIN_SAT`). */
const val USER_CHANNEL_MIN_SAT = 5_000L

/** How long a broadcast deposit may take to confirm before the transfer is dropped (the funds stay in transit and are swept back). */
const val PENDING_TRANSFER_MAX_AGE_MS = 48L * 60 * 60 * 1000

fun transferFeeSat(amountSat: Long, networkFeeSat: Long): Long =
    maxOf(amountSat / 100, TRANSFER_FEE_FLOOR_SAT, networkFeeSat)

/**
 * The operator's share: the fee less [networkFeeSat]. On-chain ([floorSat] > 0: the edge's
 * service-fee floor, which every deposit must pay) at least the floor; otherwise none below dust.
 */
fun operatorShareSat(amountSat: Long, networkFeeSat: Long, floorSat: Long = 0L): Long {
    val share = transferFeeSat(amountSat, networkFeeSat) - networkFeeSat
    return when {
        floorSat > 0L -> maxOf(share, floorSat)
        share < DUST_SAT -> 0L
        else -> share
    }
}

/** A move of [amountSat] (everything leaving the on-chain wallet) to Lightning. */
data class ToLightningQuote(
    val amountSat: Long,
    /** The deposit transaction's network fee. */
    val depositFeeSat: Long,
    /** The channel open / splice-in the node pays out of the deposit (estimated). */
    val channelFeeSat: Long,
    val shareSat: Long,
    /** What the deposit pays to the transit address. */
    val toTransitSat: Long,
) {
    val feeSat: Long get() = depositFeeSat + channelFeeSat + shareSat
    /** ≈ what the channel gains. */
    val arrivesSat: Long get() = toTransitSat - channelFeeSat
}

/** `null` when too little would arrive for a channel. */
fun quoteToLightning(amountSat: Long, depositFeeSat: Long, feerateSatVb: Long, floorSat: Long): ToLightningQuote? {
    val channelFee = CHANNEL_TX_VBYTES * feerateSatVb.coerceAtLeast(1)
    val share = operatorShareSat(amountSat, depositFeeSat + channelFee, floorSat)
    val q = ToLightningQuote(amountSat, depositFeeSat, channelFee, share, amountSat - depositFeeSat - share)
    return q.takeIf { it.arrivesSat >= USER_CHANNEL_MIN_SAT }
}

/** A move of [amountSat] (everything leaving the channel) to on-chain. */
data class ToOnchainQuote(
    val amountSat: Long,
    /** The splice-out's network fee, paid out of the channel (estimated). */
    val networkFeeSat: Long,
    /** Paid over Lightning to the LSP after the splice. */
    val shareSat: Long,
    /** The feerate (sat/vB) [networkFeeSat] was estimated at: what the move runs at once the
     *  user confirms it. */
    val feerateSatVb: Long = 0L,
    /** [feerateSatVb] is the 2 sat/vB stand-in (the destination's rate was not known yet). */
    val fallbackFeerate: Boolean = false,
) {
    val feeSat: Long get() = networkFeeSat + shareSat
    /** What the on-chain wallet receives. */
    val payoutSat: Long get() = amountSat - feeSat
}

/** `null` when nothing worth sending would arrive. [fallbackFeerate]: [feerateSatVb] is a
 *  stand-in, not the destination's estimate. */
fun quoteToOnchain(amountSat: Long, feerateSatVb: Long, fallbackFeerate: Boolean = false): ToOnchainQuote? {
    val rate = feerateSatVb.coerceAtLeast(1)
    val net = CHANNEL_TX_VBYTES * rate
    val q = ToOnchainQuote(amountSat, net, operatorShareSat(amountSat, net), rate, fallbackFeerate)
    return q.takeIf { it.payoutSat >= DUST_SAT }
}

/** What a confirmed move out does next. */
sealed interface MoveOutStep {
    /** Run [quote] (the one the user confirmed) at [feerateSatVb]. */
    data class Execute(val quote: ToOnchainQuote, val feerateSatVb: Long) : MoveOutStep
    /** Fees moved since the quote: show the amounts at [feerateSatVb] and ask again. */
    data class Requote(val feerateSatVb: Long) : MoveOutStep
    /** No quote and not everything: too little would arrive. */
    data object TooSmall : MoveOutStep
}

/** A fresh feerate more than this many times the confirmed one stops the move (as 3 / 2). */
private const val REQUOTE_RISE_NUM = 3L
private const val REQUOTE_RISE_DEN = 2L

/** The step for a move of [amountSat] the user confirmed with [approved] (null: no quote was
 *  shown — only a move of everything, a close, goes ahead then), given the destination's
 *  feerate read just before ([freshFeerate], null when it can't be had). The confirmed quote
 *  runs as shown, at its own feerate — never re-quoted unseen. It stops for a new
 *  confirmation instead when the fresh rate is more than 1.5× the quote's, or the quote used
 *  the 2 sat/vB stand-in and the real rate differs, or it was for another amount. */
fun moveOutStep(approved: ToOnchainQuote?, amountSat: Long, all: Boolean, freshFeerate: Long?): MoveOutStep {
    val fresh = freshFeerate?.coerceAtLeast(1)
    if (approved == null) {
        val rate = fresh ?: 2L
        return if (all) MoveOutStep.Execute(ToOnchainQuote(amountSat, 0L, 0L, feerateSatVb = rate), rate) else MoveOutStep.TooSmall
    }
    val rate = approved.feerateSatVb.coerceAtLeast(1)
    val requote = when {
        approved.amountSat != amountSat -> true
        fresh == null -> false
        approved.fallbackFeerate -> fresh != rate
        else -> fresh * REQUOTE_RISE_DEN > rate * REQUOTE_RISE_NUM
    }
    return if (requote) MoveOutStep.Requote(fresh ?: rate) else MoveOutStep.Execute(approved, rate)
}

/** A deposit to the transit address was broadcast at [atMs] and should become channel funds. */
data class PendingTransfer(val atMs: Long, val feerateSatVb: Long)

/** What [LightningSession] does with the transit balance while a transfer is pending. */
enum class TransitAction { Wait, Open, SpliceIn }

/** Margin left in transit for the open / splice fee. */
fun channelFeeMarginSat(feerateSatVb: Long): Long = maxOf(500L, 2 * CHANNEL_TX_VBYTES * feerateSatVb)

fun transitAction(spendableSat: Long, channel: uniffi.wallet_ffi.LnChannelState, feerateSatVb: Long): TransitAction {
    val usable = spendableSat - channelFeeMarginSat(feerateSatVb)
    return when (channel) {
        uniffi.wallet_ffi.LnChannelState.NONE -> if (usable >= USER_CHANNEL_MIN_SAT) TransitAction.Open else TransitAction.Wait
        uniffi.wallet_ffi.LnChannelState.READY -> if (usable >= DUST_SAT) TransitAction.SpliceIn else TransitAction.Wait
        else -> TransitAction.Wait
    }
}

/**
 * A move of Lightning funds on-chain, kept on the phone so the history can show it (it is
 * not a Lightning payment). [amountSat] left Lightning in all: [payoutSat] reached
 * [toWallet] and [feeSat] went in fees (network + transfer). [feePaymentId] is the
 * transfer-fee payment, folded into this entry instead of listed on its own.
 */
data class LnMove(
    val atSecs: Long,
    val amountSat: Long,
    val payoutSat: Long,
    val feeSat: Long,
    val toWallet: String,
    val feePaymentId: String?,
    /** "splice" (the channel stays open), "close" (it was closed), "in" (moved into Lightning) or
     *  "open" (the funding transaction of its channel: [txid] is that transaction; moves no money). */
    val kind: String,
    /** The on-chain transaction that delivered it to [toWallet], once matched in that wallet's
     *  history; then [payoutSat] is what actually arrived and [feeSat] = [amountSat] − [payoutSat]. */
    val txid: String? = null,
)

/**
 * Matches each move on-chain that has no transaction yet to the transaction in [history] (the
 * destination wallet's) that delivered it: a receive between 10 minutes before and 6 hours
 * after the move, no larger than what left Lightning and short of it by at most
 * [MOVE_MAX_FEE_SAT] (the fees), not already matched. Returns the moves with those filled in
 * (exact payout and fee), or null when nothing new matched.
 */
fun settleMoves(moves: List<LnMove>, history: List<com.fortis.wallet.data.HistoryEntry>): List<LnMove>? {
    val used = moves.mapNotNull { it.txid }.toMutableSet()
    var changed = false
    val out = moves.map { m ->
        if (m.kind == "in" || m.txid != null) return@map m
        val tx = history
            .filter { !it.send && it.txid !in used }
            .filter { it.amountSat in (m.amountSat - MOVE_MAX_FEE_SAT)..m.amountSat }
            .filter { it.time == 0L || it.time in (m.atSecs - 600)..(m.atSecs + 6 * 3600) }
            .minByOrNull { m.amountSat - it.amountSat }
            ?: return@map m
        used += tx.txid
        changed = true
        m.copy(payoutSat = tx.amountSat, feeSat = m.amountSat - tx.amountSat, txid = tx.txid)
    }
    return if (changed) out else null
}

/** The most a move's fees (network + transfer) can come to, for matching it to its transaction. */
const val MOVE_MAX_FEE_SAT = 20_000L

/**
 * The history entry for a stuck channel that was cancelled: the latest move into Lightning came
 * back out, so a "close" of exactly what the move in counted to [toWallet] (the ledger nets to zero). Null when
 * there is no move in, or its cancel is already recorded. Once the sweep confirms,
 * [settleMoves] fills in the exact payout.
 */
fun cancelMoveFor(moves: List<LnMove>, nowSecs: Long, toWallet: String): LnMove? {
    val i = moves.indexOfLast { it.kind == "in" }
    if (i < 0) return null
    if (moves.drop(i + 1).any { it.kind == "close" }) return null
    val m = moves[i]
    return LnMove(nowSecs, m.amountSat, m.amountSat, 0L, toWallet, null, "close")
}

/**
 * A move into Lightning is on record but its channel never existed: no channel, nothing in it or
 * on its way, and no close entry. That is a stuck channel cancelled by an app version that did not
 * write the close entry; it gets one now (see [cancelMoveFor]).
 */
fun needsCancelBackfill(moves: List<LnMove>, ln: LnState): Boolean {
    val r = ln as? LnState.Ready ?: return false
    val b = r.balances
    if (r.channel != uniffi.wallet_ffi.LnChannelState.NONE) return false
    if (b.lightningSat != 0uL || b.onchainTransitSat != 0uL || b.pendingCloseSat != 0uL || b.pendingTransitSat != 0uL) return false
    return cancelMoveFor(moves, 0L, "") != null
}

/** An earlier build wrote the cancel's close as the deposit including its fee, which leaves the
 *  ledger short by that fee. Corrects such an entry to what the move in counted; null when nothing
 *  needs it. */
fun normalizeCancelMoves(moves: List<LnMove>): List<LnMove>? {
    var changed = false
    val out = moves.mapIndexed { i, m ->
        val prevIn = moves.take(i).lastOrNull { it.kind == "in" }
        if (m.kind == "close" && m.txid == null && m.feePaymentId == null && m.feeSat == 0L && prevIn != null &&
            m.amountSat == prevIn.amountSat + prevIn.feeSat && prevIn.feeSat > 0L
        ) {
            changed = true
            m.copy(amountSat = prevIn.amountSat, payoutSat = prevIn.amountSat)
        } else m
    }
    return if (changed) out else null
}

/**
 * Plans with a network-fee guess that is raised by [step] after each failure, up to [attempts]
 * tries; the last error when none fits. The planner's fee depends on how many coins it needs, which
 * the guess cannot know: moving a whole balance of two coins failed (2026-11-16) because the first
 * guess was 118 sat short and was never raised.
 */
inline fun <T> withFeeRetry(startFee: Long, step: Long, attempts: Int, plan: (fee: Long) -> T): T {
    var fee = startFee
    var last: Exception? = null
    repeat(attempts) {
        try {
            return plan(fee)
        } catch (e: Exception) {
            last = e
            fee += step
        }
    }
    throw last ?: IllegalStateException("no attempts")
}

/** What the Move-to-Lightning tab says about the wallet's balance. */
enum class MoveInNotice { None, NothingToMove, UnderWay }

/** "Nothing to move yet" only when the wallet has nothing and no move is under way: right after
 *  moving everything the confirmed balance is 0 too, and then the move is in progress. */
fun moveInNotice(maxSat: Long, transferPending: Boolean): MoveInNotice = when {
    maxSat > 0L -> MoveInNotice.None
    transferPending -> MoveInNotice.UnderWay
    else -> MoveInNotice.NothingToMove
}

/** What the Lightning wallet's tile says while funds are in motion. There is no bare "Moving…":
 *  every state names what is happening and what it waits for (or shows nothing). */
enum class LnMoveLabel { MovingIn, ChannelOpening, ChannelClosing }

/**
 * - [LnMoveLabel.MovingIn]: a move into Lightning whose deposit has not confirmed (the transfer is
 *   pending): "Moving to Lightning: waiting for 1 confirmation";
 * - [LnMoveLabel.ChannelOpening]: the deposit confirmed and the channel's funding is waiting for
 *   confirmations: "Channel opening: waiting for confirmation x of y";
 * - [LnMoveLabel.ChannelClosing]: "Closing channel: waiting for confirmation";
 * - null: nothing to say. Funds on their way out to an on-chain wallet have their own line (with
 *   the amount and the wallet); a few hundred sat of leftover change is not a move at all.
 */
fun lightningMoveLabel(transferPending: Boolean, opening: Boolean, closing: Boolean): LnMoveLabel? = when {
    transferPending -> LnMoveLabel.MovingIn
    opening -> LnMoveLabel.ChannelOpening
    closing -> LnMoveLabel.ChannelClosing
    else -> null
}

/** The most the estimate of a move's channel opening fee can be off by and still be settled. */
const val MAX_OPEN_FEE_SETTLE_SAT = 2_000L

/**
 * After the channel opened, the wallet holds [deltaSat] more (or less) than the newest move in
 * counted, because its channel opening fee was an estimate: that move becomes what really
 * arrived, the fee what it really cost. Null unless the newest move is a move in and the gap is
 * fee-sized (anything bigger is not a fee and stays visible as "not accounted for").
 */
fun adjustLatestMoveIn(moves: List<LnMove>, deltaSat: Long): List<LnMove>? {
    val newest = moves.filter { it.kind != "open" }.maxByOrNull { it.atSecs } ?: return null
    if (newest.kind != "in") return null
    if (deltaSat == 0L || kotlin.math.abs(deltaSat) > MAX_OPEN_FEE_SETTLE_SAT) return null
    if (newest.feeSat - deltaSat < 0L) return null
    val fixed = newest.copy(amountSat = newest.amountSat + deltaSat, payoutSat = newest.payoutSat + deltaSat, feeSat = newest.feeSat - deltaSat)
    return moves.map { if (it === newest) fixed else it }
}
