package com.fortis.wallet.ui.screens

import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
import androidx.annotation.StringRes
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.animation.core.animateFloat
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.SliderDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.fortis.wallet.R
import com.fortis.wallet.TakePreview
import com.fortis.wallet.WalletViewModel
import com.fortis.wallet.data.Book
import com.fortis.wallet.data.BookEntry
import com.fortis.wallet.data.NeoxEx
import com.fortis.wallet.data.Quote
import com.fortis.wallet.data.Step
import com.fortis.wallet.data.SwapHttpException
import com.fortis.wallet.data.SwapRecord
import com.fortis.wallet.data.WalletConfig
import com.fortis.wallet.swap.SwapNotifications
import com.fortis.wallet.ui.*
import com.fortis.wallet.ui.theme.Fx
import com.fortis.wallet.ui.theme.FxType
import com.fortis.wallet.ui.theme.caps
import kotlinx.coroutines.delay
import org.json.JSONObject
import java.math.BigInteger

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested in SwapMathTest / SwapTimelineTest)
// ---------------------------------------------------------------------------

/** The commission per side, on the chain each user sends (an extra output). */
const val SWAP_FEE_BPS = 50

/** The minimum commission per side, in sat (mirrors wallet_core `SWAP_FEE_FLOOR_SAT`). */
const val SWAP_FEE_FLOOR_SAT = 400L

private val COIN = BigInteger.valueOf(100_000_000)

/** The highest price the edge takes: 21 M BTC per XBT, in sat per XBT. */
private const val MAX_PRICE_SAT = 2_100_000_000_000_000L

private val PRICE_TEXT = Regex("""(?=\d|[.,]\d)\d*(?:[.,]\d{1,8})?""")

/**
 * A typed price in BTC per XBT → sat per XBT. `.` or `,` separates the decimals
 * (at most 8). `null` for anything else, zero, or above 21 M BTC per XBT.
 */
fun parsePrice(text: String): Long? {
    val t = text.trim()
    if (!PRICE_TEXT.matches(t)) return null
    val sat = t.replace(',', '.').toBigDecimal().movePointRight(8)
    // compare before converting: toLong() would silently wrap an out-of-range value
    if (sat < java.math.BigDecimal.ONE || sat > java.math.BigDecimal.valueOf(MAX_PRICE_SAT)) return null
    return sat.toLong()
}

/** Text to prefill the price field with, or null to leave it: never over user input. */
fun prefillPrice(current: String, userEdited: Boolean, suggestion: Long?): String? =
    if (current.isEmpty() && !userEdited && suggestion != null) fmtPrice(suggestion) else null

/** [sat] sat per XBT as BTC per XBT: 8 decimals, trailing zeros trimmed to at least 5. */
fun fmtPrice(sat: Long): String =
    "${sat / 100_000_000}." + "%08d".format(java.util.Locale.ROOT, sat % 100_000_000).trimEnd('0').padEnd(5, '0')

/** How far [price] is from [ref], in percent of [ref] (positive: above). */
fun pctVsReference(price: Long, ref: Long): Double = (price - ref) * 100.0 / ref

/** Positive when [price] is worse for the user than [ref]: lower when selling XBT, higher when selling BTC. */
fun worseByPct(price: Long, ref: Long, sendChain: String): Double =
    pctVsReference(price, ref).let { if (sendChain == "xbt") -it else it }

/** Posting asks for a confirmation without a reference, or when [price] is more than 5 % worse than it. */
fun needsConfirm(price: Long, ref: Long?, sendChain: String): Boolean =
    ref == null || worseByPct(price, ref, sendChain) > 5.0

/**
 * The price to prefill: the best open price on the opposite side (selling XBT
 * meets the highest BTC-for-XBT price; selling BTC the lowest XBT-for-BTC one),
 * clamped so it is never better for the user than the reference (this phone's
 * own NeoxEX reference, else the edge's). Posting a bid or ask is free, so a
 * bait price far off the reference must not become every prefill: selling XBT
 * takes `min(best, ref)`, selling BTC `max(best, ref)`. With no best, the
 * reference; with no reference, the best; else null.
 */
fun defaultPrice(q: Quote?, ownRef: NeoxEx.Reference?, sendChain: String): Long? {
    val best = if (sendChain == "xbt") q?.bestBtcToXbt else q?.bestXbtToBtc
    val ref = ownRef?.priceSatPerXbt ?: q?.referencePrice
    if (best == null || ref == null) return best ?: ref
    return if (sendChain == "xbt") minOf(best, ref) else maxOf(best, ref)
}

/** What posting a typed price should do. */
sealed interface PostGate {
    data object Invalid : PostGate
    data class Ready(val price: Long) : PostGate
    /** Ask first: [worsePct] is how much worse than the reference (null: no reference). */
    data class Confirm(val price: Long, val worsePct: Double?) : PostGate
}

/** Decide what posting [priceText] does against the reference [ref]. */
fun postGate(priceText: String, ref: Long?, sendChain: String): PostGate {
    val price = parsePrice(priceText) ?: return PostGate.Invalid
    return if (needsConfirm(price, ref, sendChain)) {
        PostGate.Confirm(price, ref?.let { worseByPct(price, it, sendChain) })
    } else PostGate.Ready(price)
}

/** A percentage with its sign and one decimal: `+1.7%`, `-0.3%`. */
fun fmtSignedPct(pct: Double): String = "%+.1f%%".format(java.util.Locale.ROOT, pct)

/**
 * What [sendSat] on [sendChain] buys on the other chain at [priceSatPerXbt]
 * (BTC sats per whole XBT), rounded down as the edge does. 0 without a price.
 */
fun youReceive(sendSat: Long, sendChain: String, priceSatPerXbt: Long): Long {
    if (priceSatPerXbt <= 0 || sendSat <= 0) return 0
    val s = BigInteger.valueOf(sendSat)
    val p = BigInteger.valueOf(priceSatPerXbt)
    val out = if (sendChain == "xbt") s * p / COIN else s * COIN / p
    return out.min(BigInteger.valueOf(Long.MAX_VALUE)).toLong()
}

/** The BTC leg of a swap of [sendSat] on [sendChain] — what the minimum and cap judge. */
fun btcLeg(sendSat: Long, sendChain: String, priceSatPerXbt: Long): Long =
    if (sendChain == "btc") sendSat else youReceive(sendSat, sendChain, priceSatPerXbt)

/** The smallest send on [sendChain] whose BTC leg meets [minBtcSat] (the
 *  amount field's "Min"); `null` without a price for an XBT send. */
fun minSendSat(minBtcSat: Long, sendChain: String, priceSatPerXbt: Long): Long? {
    if (sendChain == "btc") return minBtcSat
    if (priceSatPerXbt <= 0) return null
    // ceil(min × 1e8 / price): the BTC leg rounds down, so the next sat up meets it
    val n = BigInteger.valueOf(minBtcSat) * COIN
    val p = BigInteger.valueOf(priceSatPerXbt)
    return ((n + p - BigInteger.ONE) / p).toLong()
}

private fun commissionPct(sendSat: Long): Long =
    (BigInteger.valueOf(sendSat) * BigInteger.valueOf(SWAP_FEE_BPS.toLong()) / BigInteger.valueOf(10_000)).toLong()

/** The commission on [sendSat]: 0.5 % rounded down, at least [SWAP_FEE_FLOOR_SAT] (as `ServiceFee` computes it). */
fun commission(sendSat: Long): Long = maxOf(commissionPct(sendSat), SWAP_FEE_FLOOR_SAT)

/** True when the floor sets the commission (the percentage is at or below it). */
fun commissionIsMinimum(sendSat: Long): Boolean = commissionPct(sendSat) <= SWAP_FEE_FLOOR_SAT

@Composable
private fun commissionLabel(sendSat: Long?): String =
    if (sendSat != null && commissionIsMinimum(sendSat)) stringResource(R.string.swap_commission_min)
    else stringResource(R.string.swap_commission, pct(SWAP_FEE_BPS))

/** Network-fee reserve for funding a swap on [chain]: every confirmed input
 *  (P2WPKH, 68 vB), the HTLC output (P2WSH, 43), the commission and change
 *  outputs (31 each), the 11 vB overhead and, on BTC, the replay-guard
 *  OP_RETURN (112: see `swapFundingGuard`), at [feerateSatVb]. */
fun swapFeeReserve(utxoCount: Int, feerateSatVb: Long, chain: String): Long =
    (11L + 68L * utxoCount + 43 + 31 + 31 + if (chain == "btc") 112 else 0) * feerateSatVb

/** Plain words for a failed take, or `null` to show the edge's own message
 *  (an amount it refused, under the amount field). */
@StringRes
fun takeError(e: Throwable): Int? = when {
    e is java.io.IOException -> R.string.swap_take_network
    e !is SwapHttpException -> null
    e.code == 404 -> R.string.swap_take_gone
    e.code == 429 -> R.string.swap_take_struck
    e.code == 409 && "price changed" in (e.message ?: "") -> R.string.swap_take_price_changed
    e.code == 409 && "no longer available" in (e.message ?: "") -> R.string.swap_take_gone
    else -> null
}

/** The chain a taker of [entry] sends: the coin the offer wants. */
fun takerSendChain(entry: BookEntry): String = if (entry.direction == "xbt_to_btc") "btc" else "xbt"

/** What a taker sends to take all of [entry]: its open amount, on the taker's chain. */
fun takerNeedSat(entry: BookEntry): Long = if (takerSendChain(entry) == "btc") entry.btcSat else entry.xbtSat

/** Whether this user can take a listed offer, and how much of it. */
sealed interface Funds {
    data class All(val walletId: String) : Funds
    /** Part of it: at most [maxSat] (on the taker's send chain) from [walletId]. */
    data class UpTo(val walletId: String, val maxSat: Long) : Funds
    data object NotEnough : Funds
    /** No wallet on [chain] to receive the other coin into. */
    data class NeedReceiveWallet(val chain: String) : Funds
}

/**
 * The funds line for listed offer [entry]: only wallets that can sign count
 * (not watch-only or hardware); the one that can swap the most (balance minus
 * commission minus its fee reserve, as the slider) is the one offered. A
 * wallet whose balance or fee reserve isn't known yet counts as nothing.
 * [minBtcSat] is the edge's swap minimum (a BTC-leg amount).
 */
fun fundsFor(
    entry: BookEntry,
    wallets: List<WalletConfig>,
    balances: Map<String, Long>,
    feeReserves: Map<String, Long>,
    minBtcSat: Long,
): Funds {
    val send = takerSendChain(entry)
    val receive = if (send == "btc") "xbt" else "btc"
    val canSign = { w: WalletConfig -> !w.watchOnly && !w.hardware }
    if (wallets.none { it.chain == receive && canSign(it) }) return Funds.NeedReceiveWallet(receive)
    val best = wallets
        .filter { it.chain == send && canSign(it) }
        .mapNotNull { w -> balances[w.id]?.let { b -> feeReserves[w.id]?.let { r -> w.id to maxSwapSat(b, r) } } }
        .maxByOrNull { it.second }
        ?: return Funds.NotEnough
    val (walletId, max) = best
    return when {
        max >= takerNeedSat(entry) -> Funds.All(walletId)
        btcLeg(max, send, entry.priceSatPerXbt) >= minBtcSat -> Funds.UpTo(walletId, max)
        else -> Funds.NotEnough
    }
}

/** The most a wallet holding [balanceSat] can swap: the amount, its commission
 *  (0.5%, at least 400 sat) and [feeReserveSat] must all fit. 0 if nothing does. */
fun maxSwapSat(balanceSat: Long, feeReserveSat: Long): Long {
    val avail = balanceSat - feeReserveSat
    if (avail <= 0) return 0
    var lo = 0L
    var hi = avail
    while (lo < hi) {
        val mid = lo + (hi - lo + 1) / 2
        if (mid + commission(mid) <= avail) lo = mid else hi = mid - 1
    }
    return lo
}

/** Where the amount slider sits for [sat] out of [maxSat] (0..1, pinned at the ends). */
fun fractionOf(sat: Long?, maxSat: Long): Float =
    if (sat == null || maxSat <= 0) 0f else (sat.toDouble() / maxSat).coerceIn(0.0, 1.0).toFloat()

/** The amount at slider position [fraction] of [maxSat], rounded down. */
fun amountAtFraction(fraction: Float, maxSat: Long): Long =
    (maxSat * fraction.toDouble().coerceIn(0.0, 1.0)).toLong()

/** Why a BTC leg of [btcLegSat] can't be offered, or `null` if it can. The
 *  resource takes the one limit it names ([amountLimit]). [maxSat] `null` = the
 *  edge sets no maximum. */
@StringRes
fun amountError(btcLegSat: Long, minSat: Long, maxSat: Long?): Int? = when {
    btcLegSat < minSat -> R.string.swap_error_below_min
    maxSat != null && btcLegSat > maxSat -> R.string.swap_error_above_cap
    else -> null
}

/** The limit an [amountError] message names: the minimum, or the maximum. */
fun amountLimit(@StringRes error: Int, minSat: Long, maxSat: Long?): Long =
    if (error == R.string.swap_error_above_cap) maxSat ?: minSat else minSat

/** The "keep your phone on" warning shown before a swap starts: how long the
 *  coins being sent take to come back if it's cancelled (XBT leg 48 h, BTC 24 h). */
@StringRes
fun keepOnBeforeStart(sendChain: String): Int =
    if (sendChain == "xbt") R.string.swap_keep_on_xbt else R.string.swap_keep_on_btc

/** The detail screen's "keep your phone on" line: [res], formatted with [until]
 *  (unix seconds) when it's our turn against a known deadline. */
data class KeepOn(@StringRes val res: Int, val until: Long?)

/** What to tell the user about keeping the phone on for [r], or `null` once it's over. */
fun keepOnNote(r: SwapRecord): KeepOn? = when {
    !r.step.active -> null
    r.step == Step.NEED_FUNDING && r.deadline != null -> KeepOn(R.string.swap_keep_on_turn, r.deadline)
    else -> KeepOn(R.string.swap_keep_on_active, null)
}

/** The participant's turn to lock its coins: about when it comes ([at]) and
 *  until when it lasts ([until]), unix seconds. */
data class FundTurn(val at: Long, val until: Long)

/** About how long an XBT block takes (what "about" in the countdown rests on). */
private const val XBT_BLOCK_SECS = 600L

/** The edge gives the participant this long once the XBT has its confirmations. */
private const val PARTICIPANT_FUND_SECS = 7_200L

/**
 * For the participant while the initiator's XBT gathers its confirmations:
 * when its own turn comes (the remaining confirmations at about 10 minutes a
 * block) and how long it then has (the edge's 2 hours, never past the latest
 * it may fund: 12 h — or half a short leg — before the BTC locktime). Exact
 * once the edge has set the deadline. `null` when there's nothing to wait for.
 */
fun fundTurn(r: SwapRecord, theirConfs: Long?, now: Long): FundTurn? {
    if (isInitiator(r) || !r.step.active || r.step == Step.NEED_FUNDING) return null
    if (r.fundingTxid != null || r.pendingFundingTxid != null || r.matchStatus != "initiator_funded") return null
    val t = r.termsJson?.let { JSONObject(it) } ?: return null
    r.deadline?.let { return FundTurn(now, it) }
    val btcLocktime = t.getLong("btc_locktime")
    val latest = btcLocktime - minOf(12 * 3600L, (btcLocktime - t.getLong("matched_at")) / 2)
    val left = (t.optLong("xbt_confs", 6) - (theirConfs ?: 0)).coerceAtLeast(0)
    val at = now + left * XBT_BLOCK_SECS
    return FundTurn(at, minOf(at + PARTICIPANT_FUND_SECS, latest))
}

/** A cancelled swap's explanation: [res], formatted with [at] (unix seconds,
 *  when our coins come back) where the text names it. */
data class CancelNote(@StringRes val res: Int, val at: Long?)

/** Our coins are locked in a match the edge cancelled: they come back on their own. */
private fun cancelledLocked(r: SwapRecord) = r.step.active && r.matchStatus == "cancelled" && r.fundingTxid != null

/**
 * What to tell the user about a cancelled swap: nothing of ours was locked
 * (nothing left the wallet), or ours is locked and comes back on its own at
 * the refund time — the initiator's because the other side never locked
 * theirs, the participant's unless the server still sees its coins in time.
 */
fun cancelledNote(r: SwapRecord): CancelNote? {
    val terms = r.termsJson?.let { JSONObject(it) }
    return when {
        r.step == Step.CANCELLED && r.fundingTxid == null && r.pendingFundingTxid == null ->
            CancelNote(R.string.swap_cancelled_nothing_spent, null)
        // the other side handed ours straight back (a cooperative release)
        r.step.active && com.fortis.wallet.swap.RELEASED in r.marks -> CancelNote(R.string.swap_cancelled_released, null)
        !cancelledLocked(r) -> null
        isInitiator(r) -> CancelNote(R.string.swap_cancelled_refund_on, terms?.optLong("xbt_locktime")?.takeIf { it > 0 })
        else -> CancelNote(R.string.swap_cancelled_locked_participant, terms?.optLong("btc_locktime")?.takeIf { it > 0 })
    }
}

/** One line of the detail screen's timeline, with the plain-words [hint] shown under
 *  it while it's the current step. [label] once it happened, [pendingLabel] until
 *  then (a step still to come never reads as done). [confsNeeded] is set on "they funded". */
data class Stage(
    @StringRes val label: Int,
    @StringRes val hint: Int,
    val done: Boolean,
    val confsNeeded: Long? = null,
    @StringRes val pendingLabel: Int = label,
)

private fun isInitiator(r: SwapRecord) = r.role?.let { it == "initiator" } ?: (r.sendChain == "xbt")

/**
 * The swap's timeline: waiting for a match → matched → you funded / they funded
 * (the initiator, who sends XBT, funds first) → redeemed → done.
 */
fun timeline(r: SwapRecord): List<Stage> {
    val terms = r.termsJson?.let { JSONObject(it) }
    val settled = r.step == Step.REDEEMED || r.step == Step.DONE
    // Until ours are in, say what puts them in — never that they're locked.
    val youHint = when {
        r.fundingTxid != null || settled -> R.string.swap_hint_you_funded
        r.step == Step.NEED_FUNDING -> R.string.swap_hint_open_to_fund
        else -> R.string.swap_hint_you_funding
    }
    val you = Stage(
        R.string.swap_stage_you_funded, youHint, r.fundingTxid != null || settled,
        pendingLabel = R.string.swap_pending_you_fund,
    )
    // Their funding is on chain (the edge has it) but not yet confirmed enough
    // for us: their coins are locked, and we wait for the network, not them.
    val theirsAnnounced = r.matchStatus in if (isInitiator(r)) setOf("participant_funded") else setOf("initiator_funded", "participant_funded")
    // Until their funding tx is seen, say we're waiting for it — not that it's locked.
    val theyHint = when {
        r.theirTxid != null || settled -> R.string.swap_hint_they_funded
        theirsAnnounced && isInitiator(r) -> R.string.swap_hint_they_confirming_after_you
        theirsAnnounced -> R.string.swap_hint_they_confirming_first
        isInitiator(r) -> R.string.swap_hint_they_funding_after_you
        else -> R.string.swap_hint_they_funding_first
    }
    val they = Stage(
        R.string.swap_stage_they_funded, theyHint, r.theirTxid != null || settled,
        terms?.optLong(if (r.recvChain == "xbt") "xbt_confs" else "btc_confs"),
        pendingLabel = if (theirsAnnounced) R.string.swap_pending_they_confirming else R.string.swap_pending_they_fund,
    )
    // The participant can only collect once the initiator has (that reveals the key).
    val redeemHint = if (!isInitiator(r) && !settled) R.string.swap_hint_redeem_after_them else R.string.swap_hint_redeemed
    return listOf(
        Stage(R.string.swap_stage_waiting, R.string.swap_hint_waiting, r.matchId != null),
        Stage(R.string.swap_stage_matched, R.string.swap_hint_matched, terms != null, pendingLabel = R.string.swap_pending_matched),
    ) + (if (isInitiator(r)) listOf(you, they) else listOf(they, you)) + listOf(
        Stage(R.string.swap_stage_redeemed, redeemHint, settled, pendingLabel = R.string.swap_pending_redeem),
        Stage(R.string.swap_stage_done, R.string.swap_hint_done, r.step == Step.DONE, pendingLabel = R.string.swap_pending_done),
    )
}

/**
 * The deadline the swap is up against now, as (label, unix seconds), or `null`:
 * accepting the match, then funding, then — once funded — when our refund opens.
 */
fun activeDeadline(r: SwapRecord, now: Long): Pair<Int, Long>? {
    if (!r.step.active || r.step == Step.REDEEMED || r.step == Step.REFUNDING) return null
    val t = r.termsJson?.let { JSONObject(it) } ?: return null
    val funded = r.fundingTxid != null || r.pendingFundingTxid != null
    if (!funded) {
        r.deadline?.let { return R.string.swap_deadline_fund to it }
        val acceptBy = t.getLong("accept_by")
        if (now < acceptBy) return R.string.swap_deadline_accept to acceptBy
        return if (isInitiator(r)) R.string.swap_deadline_fund to t.getLong("initiator_fund_by") else null
    }
    return R.string.swap_deadline_refund to t.getLong(if (r.sendChain == "xbt") "xbt_locktime" else "btc_locktime")
}

/** How an active swap touches a wallet. */
enum class WalletSwapKind {
    /** Not funded yet: this much will leave the wallet. */
    SENDING,
    /** Funded: these coins sit in the swap's HTLC until it completes. */
    LOCKED,
    /** What the swap pays into this wallet. */
    INCOMING,
    /** Refunding: the coins are on their way back. */
    RETURNING,
}

data class WalletSwapLine(val swapId: String, val kind: WalletSwapKind, val sat: Long, val chain: String)

/**
 * The active swaps paying from or into [walletId], one line each: the amount
 * leaving it (the offer's until matched, the terms' after), locked once funded
 * (a funding sent but not yet seen counts), coming back while refunding; and,
 * once matched, the amount coming in. Finished swaps show nothing.
 */
fun walletSwapLines(walletId: String, swaps: List<SwapRecord>): List<WalletSwapLine> = swaps.flatMap { r ->
    if (!r.step.active) return@flatMap emptyList()
    val terms = r.termsJson?.let { JSONObject(it) }
    fun termsSat(chain: String) = terms?.getLong(if (chain == "xbt") "xbt_sat" else "btc_sat")
    val funded = r.fundingTxid != null || r.pendingFundingTxid != null
    buildList {
        if (r.fromWalletId == walletId) {
            val kind = when {
                r.step == Step.REFUNDING -> WalletSwapKind.RETURNING
                funded -> WalletSwapKind.LOCKED
                else -> WalletSwapKind.SENDING
            }
            add(WalletSwapLine(r.id, kind, termsSat(r.sendChain) ?: r.amountSat, r.sendChain))
        }
        if (r.toWalletId == walletId && r.step != Step.REFUNDING) {
            termsSat(r.recvChain)?.let { add(WalletSwapLine(r.id, WalletSwapKind.INCOMING, it, r.recvChain)) }
        }
    }
}

/** A wallet's line for one of its active swaps; tapping it opens the swap. */
@Composable
internal fun WalletSwapText(l: WalletSwapLine, onOpen: (String) -> Unit) {
    val amount = fmtCoin(l.sat)
    val text = when (l.kind) {
        WalletSwapKind.SENDING -> stringResource(R.string.wallet_swap_sending, amount, unit(l.chain))
        WalletSwapKind.LOCKED -> stringResource(R.string.wallet_swap_locked, amount, unit(l.chain))
        WalletSwapKind.INCOMING -> stringResource(R.string.wallet_swap_incoming, amount, unit(l.chain))
        WalletSwapKind.RETURNING -> stringResource(R.string.wallet_swap_returning, amount, unit(l.chain))
    }
    Text(
        text, style = FxType.fiatSmall, color = Fx.warn,
        modifier = Modifier.clickable { onOpen(l.swapId) },
    )
}

/** Home's "Active swaps", only while there are any: each opens its swap. */
@Composable
internal fun ActiveSwapsSection(swaps: List<SwapRecord>, onOpen: (String) -> Unit) {
    val active = swaps.filter { it.step.active }
    if (active.isEmpty()) return
    Column(verticalArrangement = Arrangement.spacedBy(Fx.s2)) {
        Text(stringResource(R.string.home_active_swaps), style = MaterialTheme.typography.titleMedium, color = Fx.text)
        active.forEach { r -> SwapRow(r) { onOpen(r.id) } }
    }
}

/**
 * The swap's status in one line (Home, the Swaps list, the detail header):
 * the detail timeline's current stage, so every screen says the same thing.
 * Steps the timeline doesn't spell out — posting, waiting for a match, our
 * turn to fund, refunding, and the end states — keep their own words.
 */
@StringRes
fun swapStatusLabel(r: SwapRecord): Int = if (cancelledLocked(r)) R.string.swap_status_cancelled_refund else when (r.step) {
    Step.PROVISIONAL, Step.WAITING_MATCH, Step.NEED_FUNDING, Step.REFUNDING,
    Step.DONE, Step.REFUNDED, Step.CANCELLED, Step.FAILED -> stepLabel(r.step)
    else -> timeline(r).firstOrNull { !it.done }?.pendingLabel ?: stepLabel(r.step)
}

@StringRes
private fun stepLabel(s: Step): Int = when (s) {
    Step.PROVISIONAL -> R.string.swap_step_posting
    Step.WAITING_MATCH -> R.string.swap_step_waiting
    Step.MATCHED -> R.string.swap_step_matched
    Step.NEED_FUNDING -> R.string.swap_step_need_funding
    Step.FUNDED -> R.string.swap_step_funded
    Step.THEY_FUNDED -> R.string.swap_step_they_funded
    Step.REDEEMED -> R.string.swap_step_redeemed
    Step.DONE -> R.string.swap_step_done
    Step.REFUNDING -> R.string.swap_step_refunding
    Step.REFUNDED -> R.string.swap_step_refunded
    Step.CANCELLED -> R.string.swap_step_cancelled
    Step.FAILED -> R.string.swap_step_failed
}

/** How a swap's status reads at a glance. */
enum class SwapTone {
    /** Running on its own: the pulsing green dot. */
    LIVE,
    /** Needs the user, or its coins are coming back. */
    ATTENTION,
    BAD,
    DONE,
}

fun swapTone(r: SwapRecord): SwapTone = if (cancelledLocked(r)) SwapTone.ATTENTION else when (r.step) {
    Step.DONE -> SwapTone.DONE
    Step.CANCELLED, Step.FAILED -> SwapTone.BAD
    Step.NEED_FUNDING, Step.REFUNDING, Step.REFUNDED -> SwapTone.ATTENTION
    else -> SwapTone.LIVE
}

private fun stepColor(s: Step): Color = when (s) {
    Step.DONE -> Fx.signal
    Step.CANCELLED, Step.FAILED -> Fx.bad
    Step.NEED_FUNDING, Step.REFUNDING, Step.REFUNDED -> Fx.warn
    else -> Fx.textDim
}

private fun unit(chain: String) = chain.uppercase()
private fun other(chain: String) = if (chain == "btc") "xbt" else "btc"
private fun coin(sat: Long, chain: String) = "${fmtCoin(sat)} ${unit(chain)}"
private fun pct(bps: Int) = (bps / 100.0).toString().removeSuffix(".0")
private fun nowSecs() = System.currentTimeMillis() / 1000

private tailrec fun Context.activity(): Activity? = when (this) {
    is Activity -> this
    is ContextWrapper -> baseContext.activity()
    else -> null
}

/** Ask for POST_NOTIFICATIONS as the user confirms a swap (a no-op once granted). */
private fun askNotifications(ctx: Context) {
    ctx.activity()?.let { SwapNotifications.requestNotificationPermission(it) }
}

// ---------------------------------------------------------------------------
// Shared pieces
// ---------------------------------------------------------------------------

/** Swap screens scroll like Home's tab body, under the nav bar. */
@Composable
private fun SwapColumn(content: @Composable ColumnScope.() -> Unit) = Column(
    Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(top = Fx.s2, bottom = Fx.s4),
    verticalArrangement = Arrangement.spacedBy(Fx.s4),
    content = content,
)

/** A one-line plain-words explanation under a part of the form. */
@Composable
private fun Help(@StringRes text: Int) =
    Text(stringResource(text), style = FxType.meta, color = Fx.textFaint)

@Composable
private fun SwapTitle(text: String) =
    Text(text, style = MaterialTheme.typography.titleMedium, color = Fx.text)

/** A labelled wallet chooser: the chosen wallet as a pressable tile, the
 *  options in a dropdown. */
@Composable
private fun WalletPicker(
    label: String,
    options: List<WalletConfig>,
    selected: WalletConfig?,
    balances: Map<String, Long>,
    onSelect: (WalletConfig) -> Unit,
) {
    var open by remember { mutableStateOf(false) }
    Column(verticalArrangement = Arrangement.spacedBy(Fx.s1)) {
        Text(caps(label), style = FxType.label)
        Box {
            PressableTile(selected = false, onClick = { if (options.isNotEmpty()) open = true }) {
                Column(Modifier.weight(1f)) {
                    Text(selected?.display ?: "—", color = Fx.text, fontWeight = FontWeight.Medium)
                    selected?.let { w ->
                        Text(
                            balances[w.id]?.let { coin(it, w.chain) } ?: stringResource(R.string.home_tap_to_open),
                            style = FxType.meta, color = Fx.dimOnSlate,
                        )
                    }
                }
            }
            DropdownMenu(expanded = open, onDismissRequest = { open = false }, containerColor = Fx.panel) {
                options.forEach { w ->
                    DropdownMenuItem(
                        text = { Text(w.display, color = Fx.text) },
                        onClick = { onSelect(w); open = false },
                    )
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Swap list
// ---------------------------------------------------------------------------

@Composable
fun SwapListScreen(vm: WalletViewModel) {
    LaunchedEffect(Unit) {
        while (true) {
            vm.refreshSwaps()
            delay(3_000)
        }
    }
    LaunchedEffect(Unit) {
        vm.seedWallets.forEach { vm.loadSwapFeeReserve(it.id) }
        while (true) {
            vm.refreshBook()
            delay(BOOK_REFRESH_MS)
        }
    }
    val minBtc = vm.swapQuote?.minBtcSat ?: 0L
    SwapListContent(
        swaps = vm.swaps,
        canStart = vm.swapQuote != null && vm.seedWallets.isNotEmpty(),
        needsPhrase = vm.swapNeedsPhrase,
        onAddWallet = vm::addWallet,
        error = vm.error,
        onNew = vm::openNewSwap,
        onTake = vm::openTakeCode,
        onOpen = vm::openSwap,
        onBack = vm::swapBack,
        book = vm.book,
        funds = { e -> fundsFor(e, vm.seedWallets, vm.walletBalances, vm.swapFeeReserves, minBtc) },
        refPrice = vm.swapReference?.priceSatPerXbt ?: vm.swapQuote?.referencePrice,
        onPick = vm::openTakeListed,
    )
}

/** How often the Swaps page reloads the offers waiting for a match. */
private const val BOOK_REFRESH_MS = 15_000L

/** Active swaps first, then the history; each row opens its detail. */
@Composable
internal fun SwapListContent(
    swaps: List<SwapRecord>,
    canStart: Boolean,
    error: String?,
    onNew: () -> Unit,
    onTake: () -> Unit,
    onOpen: (String) -> Unit,
    onBack: () -> Unit,
    book: Book? = null,
    funds: (BookEntry) -> Funds = { Funds.NotEnough },
    refPrice: Long? = null,
    onPick: (BookEntry) -> Unit = {},
    needsPhrase: Boolean = false,
    onAddWallet: () -> Unit = {},
) = SwapColumn {
    SwapTitle(stringResource(R.string.swap_title))
    GlassCard {
        Text(stringResource(R.string.swap_intro_title), color = Fx.text, fontWeight = FontWeight.SemiBold)
        Text(stringResource(R.string.swap_intro), style = FxType.body, color = Fx.textDim)
    }
    if (needsPhrase) {
        // Only watch-only / hardware wallets: nothing here can sign a swap.
        GlassCard(hero = true) {
            Text(stringResource(R.string.swap_need_phrase_title), color = Fx.text, fontWeight = FontWeight.SemiBold)
            Text(stringResource(R.string.swap_need_phrase), style = FxType.body, color = Fx.textDim)
        }
        PrimaryButton(stringResource(R.string.action_add_wallet)) { onAddWallet() }
    } else {
        if (!canStart) Text(stringResource(R.string.swap_unavailable), style = FxType.meta, color = Fx.textFaint)
        PrimaryButton(stringResource(R.string.swap_new), enabled = canStart) { onNew() }
        GhostButton(stringResource(R.string.swap_take_code), enabled = canStart) { onTake() }
    }
    val (active, history) = swaps.partition { it.step.active }
    if (swaps.isEmpty()) Text(stringResource(R.string.swap_none), style = FxType.meta, color = Fx.textFaint)
    @Composable
    fun group(label: Int, list: List<SwapRecord>) {
        if (list.isNotEmpty()) {
            Column(verticalArrangement = Arrangement.spacedBy(Fx.s2)) {
                Text(caps(stringResource(label)), style = FxType.label)
                list.forEach { r -> SwapRow(r) { onOpen(r.id) } }
            }
        }
    }
    // Your swaps in progress, then the offers you could take, then the history.
    group(R.string.swap_active, active)
    if (book != null && canStart) {
        Column(verticalArrangement = Arrangement.spacedBy(Fx.s2)) {
            Text(caps(stringResource(R.string.swap_book_title)), style = FxType.label)
            if (book.xbtToBtc.isEmpty() && book.btcToXbt.isEmpty()) {
                Text(stringResource(R.string.swap_book_empty), style = FxType.meta, color = Fx.textFaint)
            }
            listOf(R.string.swap_book_get_xbt to book.xbtToBtc, R.string.swap_book_get_btc to book.btcToXbt)
                .forEach { (label, entries) ->
                    if (entries.isNotEmpty()) {
                        Text(stringResource(label), style = FxType.meta, color = Fx.textDim)
                        entries.forEach { e -> BookRow(e, funds(e), refPrice) { onPick(e) } }
                    }
                }
        }
    }
    group(R.string.swap_history, history)
    ErrorText(error)
    GhostButton(stringResource(R.string.action_back), tint = Fx.textDim) { onBack() }
}

/** One listed offer: what you'd get, its price against the market, and whether
 *  you can take it. Only an offer you can take (all or part) opens. */
@Composable
private fun BookRow(e: BookEntry, funds: Funds, refPrice: Long?, onClick: () -> Unit) {
    val send = takerSendChain(e)
    val receive = other(send)
    val canTake = funds is Funds.All || funds is Funds.UpTo
    PressableTile(selected = false, onClick = { if (canTake) onClick() }) {
        Column(Modifier.weight(1f)) {
            Text(
                stringResource(R.string.swap_book_you_get, coin(if (receive == "xbt") e.xbtSat else e.btcSat, receive)),
                style = FxType.amount, color = Fx.text,
            )
            Text(
                stringResource(R.string.swap_book_price, fmtPrice(e.priceSatPerXbt)) +
                    (refPrice?.let { " · " + stringResource(R.string.swap_book_vs_ref, fmtSignedPct(pctVsReference(e.priceSatPerXbt, it))) } ?: ""),
                style = FxType.meta, color = Fx.textDim,
            )
            val (line, color) = when (funds) {
                is Funds.All -> stringResource(R.string.swap_funds_all) to Fx.signal
                is Funds.UpTo -> stringResource(R.string.swap_funds_up_to, coin(funds.maxSat, send)) to Fx.warn
                Funds.NotEnough -> stringResource(R.string.swap_funds_not_enough, unit(send)) to Fx.textDim
                is Funds.NeedReceiveWallet -> stringResource(R.string.swap_funds_need_wallet, unit(funds.chain)) to Fx.textDim
            }
            Text(line, style = FxType.meta, color = color)
        }
    }
}

/** The amount field and, once the most that can be swapped is known, the
 *  two-way slider (sliding fills in the amount; typing moves the slider), with
 *  marks at 25 / 50 / 75 % that set the amount when tapped. [hint] explains
 *  what "all the way" is. */
@Composable
private fun SwapAmount(
    amount: String,
    onAmount: (String) -> Unit,
    chain: String,
    maxSat: Long?,
    @StringRes hint: Int = R.string.swap_help_slider,
    minSat: Long? = null,
) {
    Field(amount, onAmount, stringResource(R.string.swap_amount, unit(chain)), keyboardType = KeyboardType.Decimal)
    if (maxSat != null && maxSat > 0) {
        Column(Modifier.fillMaxWidth().padding(horizontal = Fx.s3)) {
            Slider(
                value = fractionOf(amountToSat(amount, sat = false), maxSat),
                onValueChange = { f -> onAmount(fmtCoin(amountAtFraction(f, maxSat))) },
                colors = SliderDefaults.colors(
                    thumbColor = Fx.signal, activeTrackColor = Fx.signal, inactiveTrackColor = Fx.control,
                ),
            )
            // The track runs between the thumb's centres at either end. Min and
            // Max sit quietly at its two ends; tapping one fills the amount in.
            BoxWithConstraints(Modifier.fillMaxWidth().height(32.dp)) {
                minSat?.takeIf { it in 1..maxSat }?.let { min ->
                    Text(
                        stringResource(R.string.swap_amount_min), style = FxType.meta, color = Fx.textFaint,
                        modifier = Modifier.align(Alignment.BottomStart)
                            .clickable { onAmount(fmtCoin(min)) }.padding(vertical = Fx.s1),
                    )
                }
                Text(
                    stringResource(R.string.swap_amount_max), style = FxType.meta, color = Fx.textFaint,
                    modifier = Modifier.align(Alignment.BottomEnd)
                        .clickable { onAmount(fmtCoin(maxSat)) }.padding(vertical = Fx.s1),
                )
                val track = maxWidth - SLIDER_THUMB_INSET * 2
                SLIDER_MARKS.forEach { f ->
                    val x = SLIDER_THUMB_INSET + track * f
                    Column(
                        Modifier
                            .offset(x = x - 20.dp)
                            .width(40.dp)
                            .clickable { onAmount(fmtCoin(amountAtFraction(f, maxSat))) },
                        horizontalAlignment = Alignment.CenterHorizontally,
                    ) {
                        Box(Modifier.width(1.dp).height(6.dp).background(Fx.control))
                        Text("${(f * 100).toInt()}%", style = FxType.meta, color = Fx.textDim)
                    }
                }
            }
        }
        Text(stringResource(hint, coin(maxSat, chain)), style = FxType.meta, color = Fx.textFaint)
    }
}

/** Where the slider's marks sit. */
private val SLIDER_MARKS = listOf(0.25f, 0.5f, 0.75f)

/** Material 3's slider track starts and ends half a thumb in from its edges. */
private val SLIDER_THUMB_INSET = 10.dp

// ---------------------------------------------------------------------------
// Take a listed offer
// ---------------------------------------------------------------------------

@Composable
fun TakeOfferScreen(vm: WalletViewModel, entry: BookEntry) {
    val ctx = LocalContext.current
    TakeOfferContent(
        entry = entry,
        wallets = vm.seedWallets,
        balances = vm.walletBalances,
        feeReserves = vm.swapFeeReserves,
        minBtcSat = vm.swapQuote?.minBtcSat ?: 0L,
        busy = vm.swapBusy,
        error = vm.error,
        onTake = { from, to, sat ->
            askNotifications(ctx)
            vm.takeListed(entry, from, to, sat)
        },
        onBack = vm::swapBack,
    )
}

@Composable
internal fun TakeOfferContent(
    entry: BookEntry,
    wallets: List<WalletConfig>,
    balances: Map<String, Long>,
    feeReserves: Map<String, Long>,
    minBtcSat: Long,
    busy: Boolean,
    error: String?,
    onTake: (fromId: String, toId: String, amountSat: Long) -> Unit,
    onBack: () -> Unit,
) = SwapColumn {
    val send = takerSendChain(entry)
    val receive = other(send)
    val price = entry.priceSatPerXbt
    val froms = wallets.filter { it.chain == send }
    val tos = wallets.filter { it.chain == receive }
    val best = when (val f = fundsFor(entry, wallets, balances, feeReserves, minBtcSat)) {
        is Funds.All -> f.walletId
        is Funds.UpTo -> f.walletId
        else -> null
    }
    var fromId by remember { mutableStateOf(best ?: froms.firstOrNull()?.id) }
    var toId by remember { mutableStateOf(tos.firstOrNull()?.id) }
    val from = froms.firstOrNull { it.id == fromId } ?: froms.firstOrNull()
    val to = tos.firstOrNull { it.id == toId } ?: tos.firstOrNull()
    var amount by remember { mutableStateOf("") }

    SwapTitle(stringResource(R.string.swap_take_title))
    Text(stringResource(R.string.swap_take_price_fixed, fmtPrice(price)), style = FxType.body, color = Fx.text)
    if (from == null || to == null) {
        Text(stringResource(R.string.swap_need_wallet_on, unit(if (from == null) send else receive)), style = FxType.meta, color = Fx.textFaint)
        GhostButton(stringResource(R.string.action_back), tint = Fx.textDim) { onBack() }
        return@SwapColumn
    }
    WalletPicker(stringResource(R.string.swap_from), froms, from, balances) { fromId = it.id }
    WalletPicker(stringResource(R.string.swap_to), tos, to, balances) { toId = it.id }
    val walletMax = balances[from.id]?.let { b -> feeReserves[from.id]?.let { maxSwapSat(b, it) } }
    val maxSat = walletMax?.let { minOf(it, takerNeedSat(entry)) }
    val cappedByOffer = walletMax != null && takerNeedSat(entry) <= walletMax
    SwapAmount(
        amount, { amount = it }, send, maxSat,
        hint = if (cappedByOffer) R.string.swap_help_slider_offer else R.string.swap_help_slider,
        minSat = minSendSat(minBtcSat, send, price),
    )
    val sat = amountToSat(amount, sat = false)?.takeIf { it > 0 }
    if (sat != null && sat < takerNeedSat(entry)) {
        Text(stringResource(R.string.swap_take_rest_withdrawn), style = FxType.meta, color = Fx.textDim)
    }
    val belowMin = sat != null && btcLeg(sat, send, price) < minBtcSat
    val overOffer = sat != null && sat > takerNeedSat(entry)
    val overBalance = sat != null && walletMax != null && sat > walletMax
    GlassCard {
        kv(stringResource(R.string.swap_you_send), sat?.let { coin(it, send) } ?: "—")
        kv(commissionLabel(sat), sat?.let { coin(commission(it), send) } ?: "—")
        kv(stringResource(R.string.swap_you_receive), sat?.let { "≈ " + coin(youReceive(it, send, price), receive) } ?: "—")
    }
    if (belowMin) ErrorText(stringResource(R.string.swap_error_below_min, fmtCoin(minBtcSat)))
    if (overOffer) ErrorText(stringResource(R.string.swap_error_over_offer, coin(takerNeedSat(entry), send)))
    if (overBalance) ErrorText(stringResource(R.string.swap_error_over_balance, coin(walletMax, send)))
    Text(stringResource(keepOnBeforeStart(send)), style = FxType.meta, color = Fx.warn)
    ErrorText(error)
    PrimaryButton(
        stringResource(R.string.swap_take),
        enabled = sat != null && !belowMin && !overOffer && !overBalance && !busy,
    ) { onTake(from.id, to.id, sat!!) }
    GhostButton(stringResource(R.string.action_back), tint = Fx.textDim) { onBack() }
}

@Composable
private fun SwapRow(r: SwapRecord, onClick: () -> Unit) = PressableTile(selected = false, onClick = onClick) {
    Column(Modifier.weight(1f)) {
        Text(
            stringResource(R.string.swap_direction, unit(r.sendChain), unit(r.recvChain)),
            color = Fx.text, fontWeight = FontWeight.Medium,
        )
        Text(coin(r.amountSat, r.sendChain), style = FxType.amount, color = Fx.text)
        // A running swap pulses green; steps that need the user (or are going wrong) keep their colour.
        val tone = swapTone(r)
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
            if (tone == SwapTone.LIVE) PulseDot(Fx.signal)
            Text(
                stringResource(swapStatusLabel(r)), style = FxType.meta,
                color = if (tone == SwapTone.LIVE) Fx.signal else stepColor(r.step),
            )
        }
    }
}

/** A small dot that breathes: this is happening now. */
@Composable
private fun PulseDot(color: Color) {
    val pulse = androidx.compose.animation.core.rememberInfiniteTransition(label = "live")
    val alpha by pulse.animateFloat(
        initialValue = 1f, targetValue = 0.3f,
        animationSpec = androidx.compose.animation.core.infiniteRepeatable(
            androidx.compose.animation.core.tween(900),
            androidx.compose.animation.core.RepeatMode.Reverse,
        ),
        label = "live-alpha",
    )
    Box(Modifier.size(7.dp).graphicsLayer { this.alpha = alpha }.background(color, CircleShape))
}

// ---------------------------------------------------------------------------
// New swap
// ---------------------------------------------------------------------------

@Composable
fun NewSwapScreen(vm: WalletViewModel) {
    // the live rate
    LaunchedEffect(Unit) {
        while (true) {
            delay(20_000)
            vm.refreshQuote()
        }
    }
    val ctx = LocalContext.current
    NewSwapContent(
        wallets = vm.seedWallets,
        balances = vm.walletBalances,
        quote = vm.swapQuote,
        ownReference = vm.swapReference,
        busy = vm.swapBusy,
        error = vm.error,
        onSubmit = { from, to, sat, price, private ->
            askNotifications(ctx)
            vm.createOffer(from, to, sat, price, private)
        },
        onBack = vm::swapBack,
        feeReserves = vm.swapFeeReserves,
        onFrom = { vm.loadSwapFeeReserve(it) },
    )
}

@Composable
internal fun NewSwapContent(
    wallets: List<WalletConfig>,
    balances: Map<String, Long>,
    quote: Quote?,
    ownReference: NeoxEx.Reference?,
    busy: Boolean,
    error: String?,
    onSubmit: (fromId: String, toId: String, amountSat: Long, priceSatPerXbt: Long, private: Boolean) -> Unit,
    onBack: () -> Unit,
    initialAmount: String = "",
    feeReserves: Map<String, Long> = emptyMap(),
    onFrom: (walletId: String) -> Unit = {},
) = SwapColumn {
    var fromId by remember { mutableStateOf(wallets.firstOrNull()?.id) }
    val from = wallets.firstOrNull { it.id == fromId } ?: wallets.firstOrNull()
    val targets = wallets.filter { from != null && it.chain != from.chain }
    var toId by remember { mutableStateOf<String?>(null) }
    val to = targets.firstOrNull { it.id == toId } ?: targets.firstOrNull()
    var amount by remember { mutableStateOf(initialAmount) }
    // the price is prefilled once, when the first quote arrives and the direction is known
    var priceText by remember { mutableStateOf("") }
    var priceEdited by remember { mutableStateOf(false) }
    var confirm by remember { mutableStateOf<Pair<PostGate.Confirm, Boolean>?>(null) }

    SwapTitle(stringResource(R.string.swap_new))
    if (from == null) {
        Text(stringResource(R.string.swap_need_seed_wallet), style = FxType.meta, color = Fx.textFaint)
        GhostButton(stringResource(R.string.action_back), tint = Fx.textDim) { onBack() }
        return@SwapColumn
    }
    Help(R.string.swap_why_two_wallets)
    WalletPicker(stringResource(R.string.swap_from), wallets, from, balances) { fromId = it.id; toId = null }
    Help(R.string.swap_help_from)
    if (to == null) {
        Text(
            stringResource(R.string.swap_need_wallet_on, unit(other(from.chain))),
            style = FxType.meta, color = Fx.textFaint,
        )
    } else {
        WalletPicker(stringResource(R.string.swap_to), targets, to, balances) { toId = it.id }
        Help(R.string.swap_help_to)
    }
    LaunchedEffect(from.id) { onFrom(from.id) }
    // The most this wallet can swap once the commission and network fee are paid;
    // null until its balance and the fee estimate are known.
    val maxSat = balances[from.id]?.let { b -> feeReserves[from.id]?.let { maxSwapSat(b, it) } }
    // "Min" needs the price for an XBT send: the one typed, else the suggestion
    val minPrice = parsePrice(priceText) ?: defaultPrice(quote, ownReference, from.chain) ?: 0L
    SwapAmount(
        amount, { amount = it }, from.chain, maxSat,
        minSat = quote?.let { minSendSat(it.minBtcSat, from.chain, minPrice) },
    )
    Help(R.string.swap_help_amount)
    LaunchedEffect(quote, ownReference, from.chain) {
        val suggestion = if (quote != null) defaultPrice(quote, ownReference, from.chain) else null
        prefillPrice(priceText, priceEdited, suggestion)?.let { priceText = it }
    }
    // this phone's own NeoxEX reading, else the edge's
    val refPrice = ownReference?.priceSatPerXbt ?: quote?.referencePrice
    val refOk = ownReference?.ok ?: quote?.referenceOk ?: true
    Field(priceText, { priceText = it; priceEdited = true }, stringResource(R.string.swap_price), keyboardType = KeyboardType.Decimal)
    Help(R.string.swap_help_price)
    val price = parsePrice(priceText)
    Text(
        if (refPrice == null) stringResource(R.string.swap_no_ref)
        else {
            val vs = price?.let { fmtSignedPct(pctVsReference(it, refPrice)) } ?: "—"
            stringResource(R.string.swap_price_vs_ref, fmtPrice(refPrice), vs) +
                if (refOk) "" else stringResource(R.string.swap_ref_thin)
        },
        style = FxType.meta, color = if (refPrice == null || !refOk) Fx.warn else Fx.textFaint,
    )
    val sat = amountToSat(amount, sat = false)?.takeIf { it > 0 }
    val problem = if (sat != null && quote != null && price != null) amountError(btcLeg(sat, from.chain, price), quote.minBtcSat, quote.maxBtcSat) else null
    val overBalance = sat != null && maxSat != null && sat > maxSat
    GlassCard {
        kv(
            commissionLabel(sat),
            sat?.let { coin(commission(it), from.chain) } ?: "—",
        )
        kv(
            stringResource(R.string.swap_you_receive),
            if (sat != null && quote != null && price != null) "≈ " + coin(youReceive(sat, from.chain, price), other(from.chain)) else "—",
        )
        Text(stringResource(R.string.swap_commission_note), style = FxType.meta, color = Fx.textFaint)
    }
    if (problem != null && quote != null) {
        ErrorText(stringResource(problem, fmtCoin(amountLimit(problem, quote.minBtcSat, quote.maxBtcSat))))
    }
    if (overBalance) ErrorText(stringResource(R.string.swap_error_over_balance, coin(maxSat!!, from.chain)))
    if (quote == null) Text(stringResource(R.string.swap_unavailable), style = FxType.meta, color = Fx.textFaint)
    ErrorText(error)
    val ok = sat != null && quote != null && price != null && problem == null && !overBalance && to != null && !busy
    fun post(private: Boolean) {
        when (val g = postGate(priceText, refPrice, from.chain)) {
            PostGate.Invalid -> {}
            is PostGate.Ready -> onSubmit(from.id, to!!.id, sat!!, g.price, private)
            is PostGate.Confirm -> confirm = g to private
        }
    }
    GlassCard {
        Text(stringResource(R.string.swap_next_title), color = Fx.text, fontWeight = FontWeight.SemiBold)
        Text(stringResource(R.string.swap_next_1), style = FxType.body, color = Fx.textDim)
        Text(stringResource(R.string.swap_next_2), style = FxType.body, color = Fx.textDim)
        Text(stringResource(R.string.swap_next_3), style = FxType.body, color = Fx.textDim)
        Text(stringResource(R.string.swap_next_note), style = FxType.meta, color = Fx.textFaint)
        Text(stringResource(keepOnBeforeStart(from.chain)), style = FxType.meta, color = Fx.warn)
    }
    PrimaryButton(stringResource(R.string.swap_find_match), enabled = ok) { post(false) }
    Help(R.string.swap_help_find)
    GhostButton(stringResource(R.string.swap_with_person), enabled = ok) { post(true) }
    Help(R.string.swap_help_person)
    GhostButton(stringResource(R.string.action_back), tint = Fx.textDim) { onBack() }

    confirm?.let { (g, private) ->
        AlertDialog(
            onDismissRequest = { confirm = null },
            containerColor = Fx.panel,
            title = { Text(stringResource(R.string.swap_confirm_title), color = Fx.text) },
            text = {
                Text(
                    if (g.worsePct != null) stringResource(R.string.swap_confirm_price, "%.1f%%".format(java.util.Locale.ROOT, g.worsePct))
                    else stringResource(R.string.swap_confirm_no_ref, fmtPrice(g.price)),
                    color = Fx.textDim,
                )
            },
            confirmButton = {
                TextButton({
                    confirm = null
                    onSubmit(from.id, to!!.id, sat!!, g.price, private)
                }) { Text(stringResource(R.string.swap_post_anyway), color = Fx.signal) }
            },
            dismissButton = { TextButton({ confirm = null }) { Text(stringResource(R.string.action_cancel), color = Fx.text) } },
        )
    }
}

// ---------------------------------------------------------------------------
// Take a code
// ---------------------------------------------------------------------------

@Composable
fun TakeCodeScreen(vm: WalletViewModel) {
    val ctx = LocalContext.current
    TakeCodeContent(
        preview = vm.takePreview,
        wallets = vm.seedWallets,
        balances = vm.walletBalances,
        busy = vm.swapBusy,
        error = vm.error,
        onLookUp = vm::takeCode,
        onAccept = { from, to ->
            askNotifications(ctx)
            vm.acceptTaken(from, to)
        },
        onDecline = vm::swapBack,
        onBack = vm::swapBack,
    )
}

@Composable
internal fun TakeCodeContent(
    preview: TakePreview?,
    wallets: List<WalletConfig>,
    balances: Map<String, Long>,
    busy: Boolean,
    error: String?,
    onLookUp: (code: String) -> Unit,
    onAccept: (fromId: String, toId: String) -> Unit,
    onDecline: () -> Unit,
    onBack: () -> Unit,
) = SwapColumn {
    SwapTitle(stringResource(R.string.swap_take_code))
    if (preview == null) {
        var code by remember { mutableStateOf("") }
        var scanning by remember { mutableStateOf(false) }
        Field(code, { code = it.uppercase().filter(Char::isLetterOrDigit) }, stringResource(R.string.swap_code), maxLen = 16)
        GhostButton(stringResource(if (scanning) R.string.action_cancel else R.string.action_scan_qr)) { scanning = !scanning }
        if (scanning) {
            ContinuousQrScanner(
                onFrame = { text ->
                    code = text.trim().uppercase().filter(Char::isLetterOrDigit).take(16)
                    scanning = false
                },
                modifier = Modifier.fillMaxWidth().height(240.dp).clip(RoundedCornerShape(Fx.rSm)),
            )
        }
        ErrorText(error)
        PrimaryButton(stringResource(R.string.swap_look_up), enabled = code.isNotBlank() && !busy) { onLookUp(code) }
        GhostButton(stringResource(R.string.action_back), tint = Fx.textDim) { onBack() }
        return@SwapColumn
    }

    var now by remember { mutableLongStateOf(nowSecs()) }
    LaunchedEffect(Unit) {
        while (true) {
            delay(1_000)
            now = nowSecs()
        }
    }
    val send = preview.sendChain
    val froms = wallets.filter { it.chain == send }
    val tos = wallets.filter { it.chain != send }
    var fromId by remember { mutableStateOf<String?>(null) }
    var toId by remember { mutableStateOf<String?>(null) }
    val from = froms.firstOrNull { it.id == fromId } ?: froms.firstOrNull()
    val to = tos.firstOrNull { it.id == toId } ?: tos.firstOrNull()
    val left = preview.acceptBy - now

    GlassCard {
        Text(stringResource(R.string.swap_terms), color = Fx.text, fontWeight = FontWeight.SemiBold)
        kv(stringResource(R.string.swap_you_send), coin(preview.sendSat, send))
        kv(commissionLabel(preview.sendSat), coin(commission(preview.sendSat), send))
        kv(stringResource(R.string.swap_you_receive), coin(preview.receiveSat, other(send)))
        kv(stringResource(R.string.swap_price), fmtPrice(preview.priceSatPerXbt))
        val ref = preview.reference
        if (ref == null) {
            Text(stringResource(R.string.swap_no_ref), style = FxType.meta, color = Fx.warn)
        } else {
            kv(stringResource(R.string.swap_reference), fmtPrice(ref.priceSatPerXbt) + if (ref.ok) "" else stringResource(R.string.swap_ref_thin))
            val worse = worseByPct(preview.priceSatPerXbt, ref.priceSatPerXbt, send)
            kv(stringResource(R.string.swap_vs_reference), fmtSignedPct(pctVsReference(preview.priceSatPerXbt, ref.priceSatPerXbt)))
            if (worse > 5.0) Text(stringResource(R.string.swap_take_worse, "%.1f".format(java.util.Locale.ROOT, worse)), style = FxType.meta, color = Fx.warn)
        }
        preview.problem?.let { Text(stringResource(R.string.swap_check_failed, it), style = FxType.meta, color = Fx.bad) }
        Text(stringResource(R.string.swap_commission_note), style = FxType.meta, color = Fx.textFaint)
    }
    if (from == null || to == null) {
        Text(
            stringResource(R.string.swap_need_wallet_on, unit(if (from == null) send else other(send))),
            style = FxType.meta, color = Fx.textFaint,
        )
    } else {
        WalletPicker(stringResource(R.string.swap_from), froms, from, balances) { fromId = it.id }
        WalletPicker(stringResource(R.string.swap_to), tos, to, balances) { toId = it.id }
    }
    Text(
        if (left > 0) stringResource(R.string.swap_accept_within, SwapNotifications.formatDeadline(left))
        else stringResource(R.string.swap_accept_expired),
        style = FxType.meta, color = if (left > 0) Fx.warn else Fx.bad,
    )
    from?.let { Text(stringResource(keepOnBeforeStart(it.chain)), style = FxType.meta, color = Fx.warn) }
    ErrorText(error)
    PrimaryButton(
        stringResource(R.string.swap_accept),
        enabled = preview.problem == null && from != null && to != null && left > 0 && !busy,
    ) { onAccept(from!!.id, to!!.id) }
    GhostButton(stringResource(R.string.swap_decline), tint = Fx.bad) { onDecline() }
}

// ---------------------------------------------------------------------------
// Swap detail
// ---------------------------------------------------------------------------

@Composable
fun SwapDetailScreen(vm: WalletViewModel, id: String) {
    LaunchedEffect(id) {
        while (true) {
            vm.refreshSwaps()
            delay(3_000)
        }
    }
    // a provisional record is opened by its client id, which its offer id then replaces
    val r = vm.swaps.firstOrNull { it.id == id } ?: vm.swaps.firstOrNull { it.clientId == id }
    val latest by rememberUpdatedState(r)
    var confs by remember(id) { mutableStateOf<Long?>(null) }
    // "they funded n/6": read their announced funding's confirmations until the engine records it
    LaunchedEffect(id, r?.matchId, r?.theirTxid, r?.step) {
        while (true) {
            val cur = latest ?: break
            if (!cur.step.active || cur.matchId == null || cur.theirTxid != null) break
            vm.theirConfirmations(cur)?.let { confs = it }
            delay(20_000)
        }
    }
    var now by remember { mutableLongStateOf(nowSecs()) }
    LaunchedEffect(Unit) {
        while (true) {
            delay(1_000)
            now = nowSecs()
        }
    }
    if (r == null) {
        SwapColumn {
            Text(stringResource(R.string.swap_loading), style = FxType.meta, color = Fx.textFaint)
            GhostButton(stringResource(R.string.action_back), tint = Fx.textDim) { vm.swapBack() }
        }
        return
    }
    val names = vm.wallets.associate { it.id to it.display }
    SwapDetailContent(
        r = r, now = now, theirConfs = confs,
        fromName = names[r.fromWalletId], toName = names[r.toWalletId],
        network = vm.wallets.firstOrNull { it.id == r.fromWalletId }?.network ?: "mainnet",
        busy = vm.swapBusy, error = vm.error,
        onCancel = { vm.cancelOffer(r.id) },
        onBack = vm::swapBack,
    )
}

@Composable
internal fun SwapDetailContent(
    r: SwapRecord,
    now: Long,
    theirConfs: Long?,
    fromName: String?,
    toName: String?,
    network: String,
    busy: Boolean,
    error: String?,
    onCancel: () -> Unit,
    onBack: () -> Unit,
) = SwapColumn {
    val ctx = LocalContext.current
    SwapTitle(stringResource(R.string.swap_direction, unit(r.sendChain), unit(r.recvChain)))
    Text(stringResource(swapStatusLabel(r)), style = FxType.body, color = stepColor(r.step))

    // A private offer: the code (and its QR) for the other person, until taken.
    val code = r.shareCode
    if (code != null && r.step == Step.WAITING_MATCH) {
        GlassCard {
            Text(caps(stringResource(R.string.swap_share_code)), style = FxType.label)
            Text(code, style = FxType.title, modifier = Modifier.align(Alignment.CenterHorizontally))
            val qr = remember(code) { qrBitmap(code) }
            Image(
                bitmap = qr.asImageBitmap(),
                contentDescription = stringResource(R.string.swap_share_code),
                modifier = Modifier
                    .align(Alignment.CenterHorizontally)
                    .size(224.dp)
                    .clip(RoundedCornerShape(Fx.rSm))
                    .background(Color.White)
                    .padding(10.dp),
            )
            Text(stringResource(R.string.swap_share_hint), style = FxType.meta, color = Fx.textFaint)
            GhostButton(stringResource(R.string.action_copy)) { copyToClipboard(ctx, "code", code) }
        }
    }

    GlassCard {
        val stages = timeline(r)
        val current = stages.indexOfFirst { !it.done }
        stages.forEachIndexed { i, s ->
            // A cancelled match has no step under way: nothing pulses.
            val on = i == current && r.step.active && !cancelledLocked(r)
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(Fx.s3)) {
                // The step we're on breathes: green while it runs on its own, amber when it waits on the user.
                if (on) {
                    PulseDot(if (swapTone(r) == SwapTone.LIVE) Fx.signal else Fx.warn)
                } else {
                    Box(Modifier.size(7.dp).background(if (s.done) Fx.signal else Fx.control, CircleShape))
                }
                val label = stringResource(if (s.done) s.label else s.pendingLabel)
                Text(
                    if (s.confsNeeded != null && !s.done && theirConfs != null) {
                        stringResource(R.string.swap_stage_confs, label, minOf(theirConfs, s.confsNeeded), s.confsNeeded)
                    } else label,
                    style = FxType.body,
                    color = if (s.done || on) Fx.text else Fx.textDim,
                )
            }
            // The plain-words line under the step we're on (or "all done" at the end).
            if (on || (s.label == R.string.swap_stage_done && s.done)) {
                Text(
                    stringResource(s.hint), style = FxType.meta, color = Fx.textDim,
                    modifier = Modifier.padding(start = 7.dp + Fx.s3),
                )
            }
        }
        activeDeadline(r, now)?.let { (label, at) ->
            kv(stringResource(label), SwapNotifications.formatDeadline(at - now))
        }
    }
    fundTurn(r, theirConfs, now)?.let { turn ->
        val until = android.text.format.DateFormat.getTimeFormat(ctx).format(java.util.Date(turn.until * 1000))
        Text(
            if (turn.at > now) stringResource(R.string.swap_fund_turn_soon, SwapNotifications.formatDeadline(turn.at - now), until)
            else stringResource(R.string.swap_fund_turn_now, until),
            style = FxType.meta, color = Fx.warn,
        )
    }
    cancelledNote(r)?.let { n ->
        val at = n.at?.let { java.text.DateFormat.getDateTimeInstance(java.text.DateFormat.MEDIUM, java.text.DateFormat.SHORT).format(java.util.Date(it * 1000)) }
        Text(
            if (at != null) stringResource(n.res, at) else stringResource(n.res), style = FxType.meta,
            color = if (n.res == R.string.swap_cancelled_nothing_spent) Fx.textDim else Fx.warn,
        )
    }
    keepOnNote(r)?.let { k ->
        val until = k.until?.let { android.text.format.DateFormat.getTimeFormat(ctx).format(java.util.Date(it * 1000)) }
        Text(
            if (until != null) stringResource(k.res, until) else stringResource(k.res),
            style = FxType.meta, color = Fx.warn,
        )
    }

    GlassCard {
        val terms = r.termsJson?.let { JSONObject(it) }
        val sendSat = terms?.getLong(if (r.sendChain == "xbt") "xbt_sat" else "btc_sat") ?: r.amountSat
        kv(stringResource(R.string.swap_you_send), coin(sendSat, r.sendChain))
        kv(commissionLabel(sendSat), coin(commission(sendSat), r.sendChain))
        terms?.let {
            kv(stringResource(R.string.swap_you_receive), coin(it.getLong(if (r.recvChain == "xbt") "xbt_sat" else "btc_sat"), r.recvChain))
            kv(stringResource(R.string.swap_rate), stringResource(R.string.swap_rate_value, fmtCoin(it.getLong("price_sat_per_xbt"))))
        }
        fromName?.let { kv(stringResource(R.string.swap_from), it) }
        toName?.let { kv(stringResource(R.string.swap_to), it) }
        kv(stringResource(R.string.swap_your_limit), fmtPrice(r.limitPrice))
    }

    val txs = listOfNotNull(
        r.fundingTxid?.let { Triple(R.string.swap_tx_yours, r.sendChain, it) },
        r.theirTxid?.let { Triple(R.string.swap_tx_theirs, r.recvChain, it) },
        r.redeemTxid?.let { Triple(R.string.swap_tx_redeem, r.recvChain, it) },
        r.refundTxid?.let { Triple(R.string.swap_tx_refund, r.sendChain, it) },
    )
    if (txs.isNotEmpty()) {
        GlassCard {
            txs.forEach { (label, chain, txid) ->
                val url = explorerTxUrl(chain, network, txid)
                Row(
                    Modifier.fillMaxWidth().clickable(enabled = url != null) { url?.let { openInBrowser(ctx, it) } },
                    horizontalArrangement = Arrangement.SpaceBetween,
                ) {
                    Text(stringResource(label), style = FxType.body, color = Fx.textDim)
                    Text(
                        txid.take(12) + "…", style = FxType.amount,
                        color = if (url != null) Fx.signal else Fx.text,
                    )
                }
            }
        }
    }

    r.error?.let {
        Text(
            stringResource(R.string.swap_note, it), style = FxType.meta,
            color = if (r.step == Step.FAILED || r.step == Step.CANCELLED) Fx.bad else Fx.textFaint,
        )
    }
    ErrorText(error)
    if (r.step == Step.WAITING_MATCH || r.step == Step.PROVISIONAL) {
        var asking by remember { mutableStateOf(false) }
        GhostButton(stringResource(R.string.swap_cancel), tint = Fx.bad, enabled = !busy) { asking = true }
        if (asking) AlertDialog(
            onDismissRequest = { asking = false },
            containerColor = Fx.panel,
            title = { Text(stringResource(R.string.swap_cancel_title), color = Fx.text) },
            text = { Text(stringResource(R.string.swap_cancel_body), color = Fx.textDim) },
            confirmButton = {
                TextButton({ asking = false; onCancel() }) { Text(stringResource(R.string.swap_cancel), color = Fx.bad) }
            },
            dismissButton = {
                TextButton({ asking = false }) { Text(stringResource(R.string.swap_cancel_keep), color = Fx.text) }
            },
        )
    }
    GhostButton(stringResource(R.string.action_back), tint = Fx.textDim) { onBack() }
}
