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
) {
    val feeSat: Long get() = networkFeeSat + shareSat
    /** What the on-chain wallet receives. */
    val payoutSat: Long get() = amountSat - feeSat
}

/** `null` when nothing worth sending would arrive. */
fun quoteToOnchain(amountSat: Long, feerateSatVb: Long): ToOnchainQuote? {
    val net = CHANNEL_TX_VBYTES * feerateSatVb.coerceAtLeast(1)
    val q = ToOnchainQuote(amountSat, net, operatorShareSat(amountSat, net))
    return q.takeIf { it.payoutSat >= DUST_SAT }
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
    /** "splice" (the channel stays open), "close" (it was closed) or "in" (moved into Lightning). */
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
