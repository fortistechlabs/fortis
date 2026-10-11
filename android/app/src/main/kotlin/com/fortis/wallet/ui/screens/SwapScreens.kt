package com.fortis.wallet.ui.screens

import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
import androidx.annotation.StringRes
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
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
import com.fortis.wallet.data.Quote
import com.fortis.wallet.data.Step
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

/** The slippage choices the edge accepts. */
val SLIPPAGE_BPS = listOf(50, 100, 200)

private val COIN = BigInteger.valueOf(100_000_000)

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

/** The 0.5 % commission on [sendSat], rounded down (as `ServiceFee` computes it). */
fun commission(sendSat: Long): Long =
    (BigInteger.valueOf(sendSat) * BigInteger.valueOf(SWAP_FEE_BPS.toLong()) / BigInteger.valueOf(10_000)).toLong()

/** Why a BTC leg of [btcLegSat] can't be offered (a string resource taking the
 *  minimum and the cap as coin amounts), or `null` if it can. */
@StringRes
fun amountError(btcLegSat: Long, minSat: Long, maxSat: Long): Int? = when {
    btcLegSat < minSat -> R.string.swap_error_below_min
    btcLegSat > maxSat -> R.string.swap_error_above_cap
    else -> null
}

/** One line of the detail screen's timeline. [confsNeeded] is set on "they funded". */
data class Stage(@StringRes val label: Int, val done: Boolean, val confsNeeded: Long? = null)

private fun isInitiator(r: SwapRecord) = r.role?.let { it == "initiator" } ?: (r.sendChain == "xbt")

/**
 * The swap's timeline: waiting for a match → matched → you funded / they funded
 * (the initiator, who sends XBT, funds first) → redeemed → done.
 */
fun timeline(r: SwapRecord): List<Stage> {
    val terms = r.termsJson?.let { JSONObject(it) }
    val settled = r.step == Step.REDEEMED || r.step == Step.DONE
    val you = Stage(R.string.swap_stage_you_funded, r.fundingTxid != null || settled)
    val they = Stage(
        R.string.swap_stage_they_funded, r.theirTxid != null || settled,
        terms?.optLong(if (r.recvChain == "xbt") "xbt_confs" else "btc_confs"),
    )
    return listOf(
        Stage(R.string.swap_stage_waiting, r.matchId != null),
        Stage(R.string.swap_stage_matched, terms != null),
    ) + (if (isInitiator(r)) listOf(you, they) else listOf(they, you)) + listOf(
        Stage(R.string.swap_stage_redeemed, settled),
        Stage(R.string.swap_stage_done, r.step == Step.DONE),
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

@Composable
private fun SlippagePicker(bps: Int, onChange: (Int) -> Unit) {
    Column(verticalArrangement = Arrangement.spacedBy(Fx.s1)) {
        Text(caps(stringResource(R.string.swap_slippage)), style = FxType.label)
        Segmented(SLIPPAGE_BPS.map { it.toString() to "${pct(it)}%" }, bps.toString(), { onChange(it.toInt()) })
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
    SwapListContent(
        swaps = vm.swaps,
        canStart = vm.swapQuote != null && vm.seedWallets.isNotEmpty(),
        error = vm.error,
        onNew = vm::openNewSwap,
        onTake = vm::openTakeCode,
        onOpen = vm::openSwap,
        onBack = vm::swapBack,
    )
}

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
) = SwapColumn {
    SwapTitle(stringResource(R.string.swap_title))
    if (!canStart) Text(stringResource(R.string.swap_unavailable), style = FxType.meta, color = Fx.textFaint)
    PrimaryButton(stringResource(R.string.swap_new), enabled = canStart) { onNew() }
    GhostButton(stringResource(R.string.swap_take_code), enabled = canStart) { onTake() }
    val (active, history) = swaps.partition { it.step.active }
    if (swaps.isEmpty()) Text(stringResource(R.string.swap_none), style = FxType.meta, color = Fx.textFaint)
    listOf(R.string.swap_active to active, R.string.swap_history to history).forEach { (label, list) ->
        if (list.isNotEmpty()) {
            Column(verticalArrangement = Arrangement.spacedBy(Fx.s2)) {
                Text(caps(stringResource(label)), style = FxType.label)
                list.forEach { r -> SwapRow(r) { onOpen(r.id) } }
            }
        }
    }
    ErrorText(error)
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
        Text(stringResource(stepLabel(r.step)), style = FxType.meta, color = Fx.dimOnSlate)
    }
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
        busy = vm.swapBusy,
        error = vm.error,
        onSubmit = { from, to, sat, slip, private ->
            askNotifications(ctx)
            vm.createOffer(from, to, sat, slip, private)
        },
        onBack = vm::swapBack,
    )
}

@Composable
internal fun NewSwapContent(
    wallets: List<WalletConfig>,
    balances: Map<String, Long>,
    quote: Quote?,
    busy: Boolean,
    error: String?,
    onSubmit: (fromId: String, toId: String, amountSat: Long, slippageBps: Int, private: Boolean) -> Unit,
    onBack: () -> Unit,
    initialAmount: String = "",
) = SwapColumn {
    var fromId by remember { mutableStateOf(wallets.firstOrNull()?.id) }
    val from = wallets.firstOrNull { it.id == fromId } ?: wallets.firstOrNull()
    val targets = wallets.filter { from != null && it.chain != from.chain }
    var toId by remember { mutableStateOf<String?>(null) }
    val to = targets.firstOrNull { it.id == toId } ?: targets.firstOrNull()
    var amount by remember { mutableStateOf(initialAmount) }
    var slippage by remember { mutableStateOf(100) }

    SwapTitle(stringResource(R.string.swap_new))
    if (from == null) {
        Text(stringResource(R.string.swap_need_seed_wallet), style = FxType.meta, color = Fx.textFaint)
        GhostButton(stringResource(R.string.action_back), tint = Fx.textDim) { onBack() }
        return@SwapColumn
    }
    WalletPicker(stringResource(R.string.swap_from), wallets, from, balances) { fromId = it.id; toId = null }
    if (to == null) {
        Text(
            stringResource(R.string.swap_need_wallet_on, unit(other(from.chain))),
            style = FxType.meta, color = Fx.textFaint,
        )
    } else {
        WalletPicker(stringResource(R.string.swap_to), targets, to, balances) { toId = it.id }
    }
    Field(
        amount, { amount = it }, stringResource(R.string.swap_amount, unit(from.chain)),
        keyboardType = KeyboardType.Decimal,
    )
    val sat = amountToSat(amount, sat = false)?.takeIf { it > 0 }
    val price = quote?.priceSatPerXbt ?: 0
    val problem = if (sat != null && quote != null) amountError(btcLeg(sat, from.chain, price), quote.minBtcSat, quote.maxBtcSat) else null
    GlassCard {
        kv(
            stringResource(R.string.swap_rate),
            if (quote != null) stringResource(R.string.swap_rate_value, fmtCoin(price)) else "—",
        )
        kv(
            stringResource(R.string.swap_commission, pct(SWAP_FEE_BPS)),
            sat?.let { coin(commission(it), from.chain) } ?: "—",
        )
        kv(
            stringResource(R.string.swap_you_receive),
            if (sat != null && quote != null) "≈ " + coin(youReceive(sat, from.chain, price), other(from.chain)) else "—",
        )
        Text(stringResource(R.string.swap_commission_note), style = FxType.meta, color = Fx.textFaint)
    }
    SlippagePicker(slippage) { slippage = it }
    if (problem != null && quote != null) {
        ErrorText(stringResource(problem, fmtCoin(quote.minBtcSat), fmtCoin(quote.maxBtcSat)))
    }
    if (quote == null) Text(stringResource(R.string.swap_unavailable), style = FxType.meta, color = Fx.textFaint)
    ErrorText(error)
    val ok = sat != null && quote != null && problem == null && to != null && !busy
    PrimaryButton(stringResource(R.string.swap_find_match), enabled = ok) {
        onSubmit(from.id, to!!.id, sat!!, slippage, false)
    }
    GhostButton(stringResource(R.string.swap_with_person), enabled = ok) {
        onSubmit(from.id, to!!.id, sat!!, slippage, true)
    }
    GhostButton(stringResource(R.string.action_back), tint = Fx.textDim) { onBack() }
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
    onLookUp: (code: String, slippageBps: Int) -> Unit,
    onAccept: (fromId: String, toId: String) -> Unit,
    onDecline: () -> Unit,
    onBack: () -> Unit,
) = SwapColumn {
    SwapTitle(stringResource(R.string.swap_take_code))
    if (preview == null) {
        var code by remember { mutableStateOf("") }
        var scanning by remember { mutableStateOf(false) }
        var slippage by remember { mutableStateOf(100) }
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
        SlippagePicker(slippage) { slippage = it }
        ErrorText(error)
        PrimaryButton(stringResource(R.string.swap_look_up), enabled = code.isNotBlank() && !busy) { onLookUp(code, slippage) }
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
        kv(stringResource(R.string.swap_commission, pct(SWAP_FEE_BPS)), coin(commission(preview.sendSat), send))
        kv(stringResource(R.string.swap_you_receive), coin(preview.receiveSat, other(send)))
        kv(stringResource(R.string.swap_rate), stringResource(R.string.swap_rate_value, fmtCoin(preview.priceSatPerXbt)))
        kv(
            stringResource(R.string.swap_own_price),
            preview.ownPrice?.let { stringResource(R.string.swap_rate_value, fmtCoin(it)) } ?: "—",
        )
        if (preview.problem == null) {
            Text(stringResource(R.string.swap_check_ok, "${pct(preview.slippageBps)}%"), style = FxType.meta, color = Fx.signal)
        } else {
            Text(stringResource(R.string.swap_check_failed, preview.problem), style = FxType.meta, color = Fx.bad)
        }
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
    Text(stringResource(stepLabel(r.step)), style = FxType.body, color = stepColor(r.step))

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
            val on = i == current && r.step.active
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(Fx.s3)) {
                Box(
                    Modifier.size(7.dp).background(
                        when {
                            s.done -> Fx.signal
                            on -> Fx.warn
                            else -> Fx.control
                        },
                        CircleShape,
                    ),
                )
                val label = stringResource(s.label)
                Text(
                    if (s.confsNeeded != null && !s.done && theirConfs != null) {
                        stringResource(R.string.swap_stage_confs, label, minOf(theirConfs, s.confsNeeded), s.confsNeeded)
                    } else label,
                    style = FxType.body,
                    color = if (s.done || on) Fx.text else Fx.textDim,
                )
            }
        }
        activeDeadline(r, now)?.let { (label, at) ->
            kv(stringResource(label), SwapNotifications.formatDeadline(at - now))
        }
    }

    GlassCard {
        val terms = r.termsJson?.let { JSONObject(it) }
        val sendSat = terms?.getLong(if (r.sendChain == "xbt") "xbt_sat" else "btc_sat") ?: r.amountSat
        kv(stringResource(R.string.swap_you_send), coin(sendSat, r.sendChain))
        kv(stringResource(R.string.swap_commission, pct(SWAP_FEE_BPS)), coin(commission(sendSat), r.sendChain))
        terms?.let {
            kv(stringResource(R.string.swap_you_receive), coin(it.getLong(if (r.recvChain == "xbt") "xbt_sat" else "btc_sat"), r.recvChain))
            kv(stringResource(R.string.swap_rate), stringResource(R.string.swap_rate_value, fmtCoin(it.getLong("price_sat_per_xbt"))))
        }
        fromName?.let { kv(stringResource(R.string.swap_from), it) }
        toName?.let { kv(stringResource(R.string.swap_to), it) }
        kv(stringResource(R.string.swap_slippage), "${pct(r.slippageBps)}%")
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
        GhostButton(stringResource(R.string.swap_cancel), tint = Fx.bad, enabled = !busy) { onCancel() }
    }
    GhostButton(stringResource(R.string.action_back), tint = Fx.textDim) { onBack() }
}
