package com.fortis.wallet.ui.screens

import android.content.Intent
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.fortis.wallet.HomeBalance
import com.fortis.wallet.R
import com.fortis.wallet.WalletViewModel
import com.fortis.wallet.data.LN_CHAIN
import com.fortis.wallet.homeBalance
import com.fortis.wallet.lightning.LnState
import com.fortis.wallet.lightning.lightningWalletOf
import com.fortis.wallet.ui.*
import com.fortis.wallet.ui.theme.Fx
import com.fortis.wallet.ui.theme.FxType
import com.fortis.wallet.ui.theme.caps
import uniffi.wallet_ffi.LnChannelState
import uniffi.wallet_ffi.LnTarget
import uniffi.wallet_ffi.LnUnifiedLightning

/*
 * The Lightning wallets (chain "ln" for BTC and "ln-xbt" for XBT, one of each per app): the
 * tab — balance, Receive / Send / History on the node — the settings card, and the Lightning
 * confirm sheet. The same screens serve both, labelled by the wallet's chain. On-chain wallets
 * are on-chain only; a Lightning target there hands off to the Lightning wallet of its chain.
 */

private fun fmt(sat: Long) = fmtCoin(sat)
private const val BURST_SETTLE_MS = 2_500L

/** Funds on their way out of the Lightning wallet: a closed or closing channel's money still
 *  confirming, or waiting in the transit wallet for its sweep to the BTC wallet. */
internal fun leavingLightningSat(r: LnState.Ready): Long {
    val b = r.balances
    val open = r.channel == LnChannelState.READY || r.channel == LnChannelState.OPENING
    return if (open) b.pendingCloseSat.toLong() else (b.pendingCloseSat + b.onchainTransitSat).toLong()
}

/** The coin the Lightning wallet in view holds: "BTC" for "ln", "XBT" for "ln-xbt". */
private fun lnUnit(vm: WalletViewModel): String = chainUnit(vm.config?.chain)

/** How long the receive amount must stand still before a Lightning quote is
 *  bought for it. */

/** How long the Send field must stand still before an LNURL / Lightning
 *  address in it is looked up (it contacts that address's server). */
private const val LNURL_DEBOUNCE_MS = 1_000L

/** The Lightning wallet on [walletChain]'s whole balance: its channel and transit funds plus
 *  what Fortis holds for it (the account). `null` while the node is not running. */
@Composable
internal fun lightningWalletTotalSat(vm: WalletViewModel, walletChain: String): Long? {
    val id = lightningWalletOf(vm.wallets, walletChain)?.id
    val live = lightningWalletLiveTotalSat(vm, walletChain)
    if (id != null) {
        androidx.compose.runtime.LaunchedEffect(id) { vm.loadLnCachedTotal(id) }
        androidx.compose.runtime.LaunchedEffect(id, live) { if (live != null) vm.rememberLnTotal(id, live) }
    }
    return live ?: id?.let { vm.lnCachedTotals[it] }
}

/** The live total: only once the node is ready and the Fortis account has answered. */
@Composable
private fun lightningWalletLiveTotalSat(vm: WalletViewModel, walletChain: String): Long? {
    val side = vm.lnSide(walletChain)
    val lnState by side.session.state.collectAsState()
    val ready = lnState as? LnState.Ready ?: return null
    val acct = side.accountBalance ?: return null
    // What the user can use: money already on its way out (a close confirming, its sweep)
    // is shown apart ("on its way to …"), not in the balance.
    return lightningWalletBalance(vm, walletChain)?.let { it.totalSat - leavingLightningSat(ready) + (acct.balanceMsat / 1_000uL).toLong() }
}

/** The Lightning wallet on [walletChain]'s balance (Lightning plus funds moving on-chain), or
 *  null while its node has not reported one yet. */
@Composable
internal fun lightningWalletBalance(vm: WalletViewModel, walletChain: String): HomeBalance? {
    val lnState by vm.lnSide(walletChain).session.state.collectAsState()
    return when (lnState) {
        is LnState.Ready, is LnState.Error -> homeBalance(0L, lnState)
        LnState.Off, LnState.Starting -> null
    }
}

@Composable
private fun lightningStatusText(st: LnState): String = when (st) {
    LnState.Off -> stringResource(R.string.settings_lightning_status_off)
    LnState.Starting -> stringResource(R.string.settings_lightning_status_starting)
    is LnState.Ready -> stringResource(R.string.settings_lightning_status_ready)
    is LnState.Error -> { val ctx = LocalContext.current; lnErrorText(st.error) { res, args -> ctx.getString(res, *args) } }
}

/** Wallet tab for the Lightning wallet: its balance, then Receive / Send / History. */
@Composable
fun LightningWalletTab(vm: WalletViewModel) {
    val c = vm.config ?: return
    var tab by remember { mutableStateOf("receive") }
    val lnState by vm.lightning.state.collectAsState()
    val unit = chainUnit(c.chain)
    val hb = lightningWalletBalance(vm, c.chain)
    val accountSat = ((vm.accountBalance?.balanceMsat ?: 0uL) / 1_000uL).toLong()
    val lnReadyNow = lnState is LnState.Ready
    // A bolt strikes whenever the wallet's Lightning total (channel + Fortis account) changes:
    // up for a payment in, down for one out. Only once both parts are known, so loading
    // the account is not mistaken for a payment.
    var burst by remember { mutableStateOf<com.fortis.wallet.ui.LightningBurstEvent?>(null) }
    val lnTotal = if (lnReadyNow && hb != null && vm.accountBalance != null) {
        hb.totalSat - leavingLightningSat(lnState as LnState.Ready) + accountSat
    } else null
    // The cached total until the node and the account answer, then the live one.
    val cachedOrLive = lightningWalletTotalSat(vm, c.chain)
    val shownSat = lnTotal ?: cachedOrLive
    var lastLnTotal by remember(c.id) { mutableStateOf<Long?>(null) }
    LaunchedEffect(lnTotal) {
        val t = lnTotal ?: return@LaunchedEffect
        // Let both parts catch up first: a move from the Fortis balance into the channel
        // shows in the channel a poll before it leaves the balance (no +/− flicker).
        kotlinx.coroutines.delay(BURST_SETTLE_MS)
        val prev = lastLnTotal
        lastLnTotal = t
        if (prev != null && t != prev) burst = com.fortis.wallet.ui.LightningBurstEvent(System.nanoTime(), t - prev)
    }
    burst?.let { ev ->
        androidx.compose.ui.window.Popup(alignment = Alignment.Center) {
            com.fortis.wallet.ui.LightningBurst(ev, onDone = { if (burst?.id == ev.id) burst = null })
        }
    }
    // Push notifications wake the user for payments held while the phone is off: ask once.
    val pushCtx = androidx.compose.ui.platform.LocalContext.current
    LaunchedEffect(Unit) {
        (pushCtx as? android.app.Activity)?.let { com.fortis.wallet.swap.SwapNotifications.requestNotificationPermission(it) }
    }
    // The Fortis account's balance and history (spec §7), while this tab shows.
    LaunchedEffect(c.id, lnReadyNow, vm.sentCount) {
        while (lnReadyNow) {
            vm.refreshAccount()
            kotlinx.coroutines.delay(2_000)
        }
    }

    Column(
        Modifier.fillMaxSize().padding(top = Fx.s2),
        verticalArrangement = Arrangement.spacedBy(Fx.s4),
    ) {
        LightningTakeoverBanner(vm.lightning)
        GlassCard(hero = true) {
            Column(Modifier.fillMaxWidth()) {
                Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                    Text(caps(c.display), style = FxType.label, modifier = Modifier.weight(1f))
                    TextButton({ vm.lock() }) { Text(stringResource(R.string.action_lock), color = Fx.textDim) }
                }
                Column {
                    FlowRow {
                        val (whole, fraction) = splitAmount(if (shownSat != null) fmt(countUpSat(shownSat)) else "—")
                        Text(whole, style = FxType.balance, modifier = Modifier.alignByBaseline())
                        if (fraction.isNotEmpty()) Text(fraction, style = FxType.balance, modifier = Modifier.alignByBaseline())
                        Text(unit, style = FxType.label, color = Fx.signal, modifier = Modifier.alignByBaseline().padding(start = 6.dp))
                    }
                    if (vm.accountSending || lnTotal == null) Row(verticalAlignment = Alignment.CenterVertically) {
                        CircularProgressIndicator(Modifier.size(12.dp), color = Fx.signal, strokeWidth = 2.dp)
                        Text(
                            stringResource(R.string.ln_balance_updating),
                            style = FxType.fiatSmall, color = Fx.textDim,
                            modifier = Modifier.padding(start = 6.dp),
                        )
                    }
                    shownSat?.let { sat ->
                        vm.fiatText(sat, c.chain)?.let {
                            Text(stringResource(R.string.fiat_approx, it), style = FxType.fiat)
                        }
                    }
                    // Two places the money can be: the user's own channel, and the Fortis account.
                    if (accountSat > 0L && hb != null) {
                        Text(stringResource(R.string.ln_balance_in_channel, fmt(hb.totalSat)), style = FxType.fiatSmall)
                        Text(stringResource(R.string.ln_balance_held_by_fortis, fmt(accountSat)), style = FxType.fiatSmall)
                        Text(stringResource(R.string.ln_account_explainer), style = FxType.meta, color = Fx.textFaint)
                    }
                    if (vm.lightning.transferPending.collectAsState().value) Text(
                        stringResource(R.string.transfer_pending),
                        style = FxType.fiatSmall, color = Fx.warn,
                    )
                    // Part of it is still on-chain: a channel opening or closing.
                    // Leaving: a close (or its sweep) on its way to an on-chain wallet of its chain. Say how much and where.
                    val leavingSat = (lnState as? LnState.Ready)?.let { leavingLightningSat(it) } ?: 0L
                    val leavingTo = vm.moveTargetWallet()
                    if (leavingSat > 0L) Text(
                        // No wallet of its chain to sweep to: say what to add, not an empty name.
                        if (leavingTo != null) stringResource(R.string.ln_moving_to_wallet, fmt(leavingSat), leavingTo.display)
                        else stringResource(com.fortis.wallet.lightning.noOnchainWalletRes(c.chain)),
                        style = FxType.fiatSmall, color = Fx.warn,
                    ) else if (hb?.moving == true) Text(
                        stringResource(R.string.wallet_split_moving),
                        style = FxType.fiatSmall, color = Fx.warn,
                    )
                    val dotColor = when (lnState) {
                        is LnState.Ready -> Fx.good
                        else -> Fx.warn
                    }
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Box(Modifier.size(7.dp).clip(CircleShape).background(dotColor))
                        Spacer(Modifier.width(6.dp))
                        Text(lightningStatusText(lnState), style = FxType.meta)
                    }
                    if (lnState is LnState.Error) com.fortis.wallet.ui.SendToSupportLink(lightningStatusText(lnState))
                }
            }
        }
        val tabKeys = if (com.fortis.wallet.lightning.lightningSweepWallet(vm.wallets, c.chain) != null)
            listOf("receive", "send", "move", "history") else listOf("receive", "send", "history")
        LaunchedEffect(vm.pendingSendText, c.id) {
            if (vm.pendingSendText != null) tab = "send"
        }
        val tabLabels = mapOf(
            "receive" to stringResource(R.string.tab_receive),
            "send" to stringResource(R.string.tab_send),
            "move" to stringResource(R.string.transfer_tab),
            "history" to stringResource(R.string.tab_history),
        )
        TextTabs(tabKeys.map { it to tabLabels.getValue(it) }, tab, { tab = it })
        vm.lastSentTxid?.let { id -> SentDialog(vm, id) }
        Column(
            Modifier.weight(1f).fillMaxWidth().verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(Fx.s4),
        ) {
            when (tab) {
                "receive" -> {
                    LightningReceiveTab(vm)
                    // The XBT wallet's offer only while its LSP holds custody; BTC's always.
                    if (com.fortis.wallet.lightning.offerShown(c.chain, vm.accountBalance)) ReusableOfferCard(vm)
                }
                "send" -> key(vm.sentCount) { LightningSendTab(vm) } // a sent payment starts the form over
                "move" -> key(vm.sentCount) { TransferToOnchainTab(vm) }
                else -> LightningHistory(vm)
            }
        }
    }
}

/** Receive by invoice: an amount, then a `lightning:` QR once the node has quoted it. */
@OptIn(ExperimentalFoundationApi::class)
@Composable
internal fun LightningReceiveTab(vm: WalletViewModel) {
    val ctx = LocalContext.current
    val unit = lnUnit(vm)
    val lnState by vm.lightning.state.collectAsState()
    val minLnSat = vm.lightning.lastInfo?.minPaymentSat ?: 10_000L
    val maxLnSat = vm.lightning.lastInfo?.maxPaymentSat ?: (DEFAULT_LN_LIMITS.maxPaymentMsat / 1_000uL).toLong()
    val remoteEnabled = vm.lightning.lastInfo?.enabled != false
    val remoteMessage = vm.lightning.lastInfo?.message?.takeIf { it.isNotBlank() }

    var amount by remember { mutableStateOf("") }
    var amountInSat by remember { mutableStateOf(true) }
    val amountSat = amountToSat(amount, amountInSat)
    // The amount the user asked an invoice for (the button); a new amount clears it.
    var requestedSat by remember { mutableStateOf<Long?>(null) }
    if (requestedSat != null && requestedSat != amountSat) requestedSat = null
    var quote by remember { mutableStateOf<uniffi.wallet_ffi.LnReceiveQuote?>(null) }
    var quoting by remember { mutableStateOf(false) }
    // Where this amount is received: own channel, a new (JIT) channel, or the Fortis account.
    var viaAccount by remember { mutableStateOf(false) }
    // Only a receive that opens a new channel (JIT) has a minimum.
    var viaJit by remember { mutableStateOf(false) }
    var accountError by remember { mutableStateOf<String?>(null) }
    val custodyFullText = stringResource(R.string.ln_account_custody_full, fmtSat(minLnSat))

    // Keyed on "is it ready", not on the whole state: a balance update must not
    // buy another quote (each one is an LSPS2 request and a stored payment).
    val lnReady = lnState is LnState.Ready
    LaunchedEffect(requestedSat, lnReady, remoteEnabled) {
        quote = null
        quoting = false
        accountError = null
        val req = requestedSat
        if (req != null && req > 0L && lnReady) {
            quoting = true
            val amountMsat = req.toULong() * 1_000uL
            val node = vm.lightning.currentNode
            val room = node?.let { n -> kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) { n.receivableMsat() } } ?: 0uL
            val route = com.fortis.wallet.lightning.receiveRoute(amountMsat, room, minLnSat.toULong() * 1_000uL)
            viaAccount = route == com.fortis.wallet.lightning.ReceiveRoute.Account
            viaJit = route == com.fortis.wallet.lightning.ReceiveRoute.JitChannel
            quote = if (viaAccount) {
                try {
                    vm.accountInvoice(amountMsat)
                } catch (e: com.fortis.wallet.lightning.AccountError.CustodyFull) {
                    accountError = custodyFullText
                    null
                } catch (e: Exception) {
                    accountError = e.message
                    null
                }
            } else if (route == com.fortis.wallet.lightning.ReceiveRoute.OwnInvoice) {
                // Held by the LSP until this phone takes it: it need not be open when paid.
                runCatching { vm.channelInvoice(amountMsat) }.getOrElse { e ->
                    accountError = e.message
                    null
                }
            } else {
                kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
                    runCatching { node?.receive(amountMsat, "") }
                        .getOrNull()
                        ?.takeIf { remoteEnabled || !it.needsChannel }
                }
            }
            quoting = false
        }
    }

    // An account invoice has no minimum.
    val rxMin = if (viaJit) minLnSat else 1L
    val rxQr = remember(amountSat, quote, rxMin) { buildLightningReceiveQr(amountSat, quote, rxMin) }
    val invoice = quote?.invoice?.takeIf { rxQr.lightning }

    val chooserTitle = stringResource(R.string.share_address_chooser)
    GlassCard {
        Text(stringResource(R.string.tab_receive), color = Fx.text, fontWeight = FontWeight.SemiBold)
        val ready = lnState as? LnState.Ready
        if (ready != null && ready.channel == LnChannelState.NONE && ready.balances.lightningSat == 0uL) {
            Text(stringResource(R.string.ln_wallet_first_receive_hint, fmtSat(minLnSat)), style = FxType.meta, color = Fx.textDim)
        }
        Text(caps(stringResource(R.string.field_amount)), style = FxType.label)
        Row(
            horizontalArrangement = Arrangement.spacedBy(Fx.s2),
            verticalAlignment = Alignment.Top,
        ) {
            Field(
                amount, { amount = it }, null,
                keyboardType = if (amountInSat) KeyboardType.Number else KeyboardType.Decimal,
                modifier = Modifier.weight(1f),
            )
            AmountUnitPicker(amountInSat, unit) { amountInSat = it }
        }
        val focus = androidx.compose.ui.platform.LocalFocusManager.current
        if (invoice == null) PrimaryButton(
            stringResource(R.string.ln_create_invoice),
            modifier = Modifier.fillMaxWidth(),
            enabled = lnReady && amountSat != null && amountSat > 0L,
            working = quoting,
        ) {
            focus.clearFocus()
            requestedSat = amountSat
        }
        accountError?.let { Text(it, style = FxType.meta, color = Fx.warn) }
        if (invoice != null && viaAccount) {
            Text(stringResource(R.string.ln_receive_to_account), style = FxType.meta, color = Fx.textDim)
        }
        if (!lnReady) {
            Text(lightningStatusText(lnState), style = FxType.meta, color = Fx.textDim)
        } else if (rxQr.lightning && rxQr.feeSat != null && rxQr.feeSat > 0L && rxQr.youGetSat != null && amountSat != null) {
            Text(
                stringResource(
                    R.string.receive_ln_fee_summary,
                    fmt(amountSat),
                    unit,
                    fmt(rxQr.feeSat),
                    fmt(rxQr.youGetSat),
                ),
                style = FxType.meta,
                color = Fx.warn,
            )
        }
        if (!remoteEnabled && remoteMessage != null) {
            Text(remoteMessage, style = FxType.meta, color = Fx.warn)
        } else {
            when (rxQr.hint) {
                ReceiveHint.AddAmountForLightning ->
                    Text(stringResource(R.string.receive_ln_needs_amount), style = FxType.meta, color = Fx.textDim)
                ReceiveHint.BelowLightningMin ->
                    Text(
                        stringResource(R.string.ln_err_amount_out_of_range_range, fmtSat(minLnSat), fmtSat(maxLnSat)),
                        style = FxType.meta,
                        color = Fx.textDim,
                    )
                null -> {}
            }
        }
        if (invoice != null) {
            // What the wallet holds (channel + Fortis account) when this invoice was shown:
            // more than that means it was paid.
            val heldNow = (ready?.balances?.lightningSat?.toLong() ?: 0L) +
                ((vm.accountBalance?.balanceMsat ?: 0uL) / 1_000uL).toLong()
            val heldAtInvoice = remember(invoice) { heldNow }
            val gained = heldNow - heldAtInvoice

            Row(verticalAlignment = Alignment.CenterVertically) {
                if (gained > 0L) {
                    Text(stringResource(R.string.ln_receive_received, fmtSat(gained)), style = FxType.body, color = Fx.good)
                } else {
                    CircularProgressIndicator(Modifier.size(14.dp), color = Fx.signal, strokeWidth = 2.dp)
                    Text(
                        stringResource(R.string.ln_receive_waiting),
                        style = FxType.meta, color = Fx.textDim,
                        modifier = Modifier.padding(start = 8.dp),
                    )
                }
            }
            val qr = remember(rxQr.uri) { qrBitmap(rxQr.uri) }
            val copyQr = { copyImageToClipboard(ctx, qr) }
            Image(
                bitmap = qr.asImageBitmap(),
                contentDescription = stringResource(R.string.cd_address_qr),
                modifier = Modifier
                    .align(Alignment.CenterHorizontally)
                    .size(224.dp)
                    .clip(RoundedCornerShape(Fx.rSm))
                    .background(Color.White)
                    .combinedClickable(onClick = copyQr, onLongClick = copyQr, onDoubleClick = copyQr)
                    .padding(10.dp),
            )
            XbtLightningCaption(vm, Modifier.align(Alignment.CenterHorizontally))
            Text(
                stringResource(R.string.receive_qr_hint),
                style = FxType.meta,
                color = Fx.textFaint,
                modifier = Modifier.align(Alignment.CenterHorizontally),
            )
            val copy = { copyToClipboard(ctx, "invoice", invoice) }
            Text(
                if (invoice.length > 48) "${invoice.take(24)}…${invoice.takeLast(16)}" else invoice,
                style = FxType.amount,
                modifier = Modifier
                    .fillMaxWidth()
                    .clip(RoundedCornerShape(Fx.rSm))
                    .background(Fx.panelRaised)
                    .clickable { copy() }
                    .padding(Fx.s4),
            )
            Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
                GhostButton(stringResource(R.string.action_copy), Modifier.weight(1f)) { copy() }
                GhostButton(stringResource(R.string.action_share), Modifier.weight(1f)) {
                    ctx.startActivity(
                        Intent.createChooser(
                            Intent(Intent.ACTION_SEND).apply { type = "text/plain"; putExtra(Intent.EXTRA_TEXT, invoice) },
                            chooserTitle,
                        ),
                    )
                }
            }
        }
    }
}

/** A plain sat count with thousands separators, for the Lightning bounds hint. */
private fun fmtSat(sat: Long): String = "%,d".format(java.util.Locale.ROOT, sat)

/** The Lightning part of [target] as text [planSend] plans on Lightning: a BIP21 URI's
 *  `lightning=` invoice or `lno=` offer; anything else unchanged. */
private fun lightningPart(text: String, target: LnTarget?): String = when (target) {
    is LnTarget.Unified -> when (val l = target.lightning) {
        is LnUnifiedLightning.Bolt11 -> l.invoice
        is LnUnifiedLightning.Bolt12Offer -> l.offer
    }
    else -> text
}

/** Pay an invoice, offer, LNURL or Lightning address from the node. */
@Composable
internal fun LightningSendTab(vm: WalletViewModel) {
    var to by remember { mutableStateOf(vm.consumePendingSendText() ?: "") }
    var amount by remember { mutableStateOf("") }
    var amountInSat by remember { mutableStateOf(true) }
    val c = vm.config
    val network = c?.network ?: "bitcoin"
    val walletChain = c?.chain ?: LN_CHAIN
    val unit = lnUnit(vm)
    val amountSat = amountToSat(amount, amountInSat)
    val lnState by vm.lightning.state.collectAsState()
    val limits = remember(vm.lightning.lastInfo) {
        vm.lightning.lastInfo?.toLspLimits() ?: DEFAULT_LN_LIMITS
    }

    LaunchedEffect(vm.pendingSendText) {
        vm.consumePendingSendText()?.let { to = it }
    }

    var resolvedLnurl by remember { mutableStateOf<uniffi.wallet_ffi.LnurlParams?>(null) }
    var lnurlError by remember { mutableStateOf<String?>(null) }
    var resolvingLnurl by remember { mutableStateOf(false) }

    val parsedTarget = remember(to, network) {
        if (to.isBlank()) null
        else runCatching { parseScan(to, network) }.getOrNull()
    }
    // A BIP21 URI is paid by its Lightning part here: this wallet has no on-chain funds.
    val lnText = remember(to, parsedTarget) { lightningPart(to, parsedTarget) }
    val onchainOnly = parsedTarget is LnTarget.OnChain
    val isLnurlTarget = parsedTarget is LnTarget.LnurlPay || parsedTarget is LnTarget.LightningAddress
    // An invoice or offer of the other chain (or of none this app can tell) is refused before
    // anything else. An LNURL / address is checked when its invoice is fetched, at pay time.
    val wrongChain = remember(parsedTarget, walletChain) {
        val t = parsedTarget
        if (t == null || onchainOnly || isLnurlTarget) null
        else com.fortis.wallet.lightning.targetChainRefusal(walletChain, t)?.let { it to com.fortis.wallet.lightning.isOfferTarget(t) }
    }

    LaunchedEffect(parsedTarget) {
        val bip21Sat = (parsedTarget as? LnTarget.Unified)?.amountSat?.toLong()
        if (bip21Sat != null && amount.isEmpty()) {
            amountInSat = true
            amount = bip21Sat.toString()
        }
    }

    val ctx = LocalContext.current
    LaunchedEffect(to, isLnurlTarget) {
        resolvedLnurl = null
        lnurlError = null
        resolvingLnurl = false
        val t = parsedTarget
        if (t is LnTarget.LnurlPay || t is LnTarget.LightningAddress) {
            resolvingLnurl = true
            kotlinx.coroutines.delay(LNURL_DEBOUNCE_MS)
            try {
                resolvedLnurl = kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
                    uniffi.wallet_ffi.lnResolveLnurl(t)
                }
            } catch (e: com.fortis.wallet.lightning.LnFfiError) {
                lnurlError = lnErrorText(e) { res, args -> ctx.getString(res, *args) }
            } catch (e: Exception) {
                lnurlError = e.message ?: e.toString()
            } finally {
                resolvingLnurl = false
            }
        }
    }

    val sendPlan = remember(to, lnText, network, lnState, limits, amountSat, resolvedLnurl, lnurlError, isLnurlTarget, onchainOnly, wrongChain) {
        if (to.isBlank() || onchainOnly || wrongChain != null) null
        else if (isLnurlTarget && resolvedLnurl == null) {
            if (lnurlError != null) SendPlan.Invalid(lnurlError!!) else null
        } else {
            planSend(
                text = lnText,
                onchainSat = 0L,
                ln = lnState,
                network = network,
                limits = limits,
                userAmountMsat = amountSat?.takeIf { it > 0L }?.let { it.toULong() * 1_000uL },
                lnurlResolver = {
                    resolvedLnurl ?: throw IllegalStateException(lnurlError ?: "Resolving LNURL…")
                },
                sendableMsat = vm.lightning.sendableMsat,
                extraSendableMsat = vm.accountBalance?.freeMsat ?: 0uL,
            )
        }
    }

    var scanning by remember { mutableStateOf(false) }
    if (scanning) QrScanDialog(
        onResult = { raw -> scanning = false; to = textFromIntent(raw) ?: raw.trim() },
        onDismiss = { scanning = false },
    )
    GlassCard {
        Text(stringResource(R.string.tab_send), color = Fx.text, fontWeight = FontWeight.SemiBold)
        Field(to, { new -> to = new.filterNot(Char::isWhitespace) }, stringResource(R.string.field_to_address), mono = true)
        GhostButton(stringResource(R.string.action_scan_qr)) { scanning = true }

        if (lnState is LnState.Off || lnState is LnState.Starting) {
            Text(lightningStatusText(lnState), style = FxType.meta, color = Fx.textDim)
        }
        if (wrongChain != null) {
            val (targetChain, isOffer) = wrongChain
            ErrorText(com.fortis.wallet.lightning.wrongChainMessage(walletChain, targetChain, isOffer) { ctx.getString(it) })
            // The other chain's Lightning wallet can pay it: hand it over when the app has one.
            val other = if (walletChain == com.fortis.wallet.data.LN_XBT_CHAIN) LN_CHAIN else com.fortis.wallet.data.LN_XBT_CHAIN
            val otherChain = if (other == LN_CHAIN) uniffi.wallet_ffi.LnTargetChain.BTC else uniffi.wallet_ffi.LnTargetChain.XBT
            if (targetChain == otherChain && lightningWalletOf(vm.wallets, other) != null) {
                PrimaryButton(stringResource(com.fortis.wallet.lightning.payFromOtherWalletRes(other))) {
                    vm.payWithLightningWallet(to, other)
                }
            }
        } else if (onchainOnly) {
            ErrorText(stringResource(R.string.ln_send_onchain_target))
        } else if (resolvingLnurl) {
            Text(stringResource(R.string.send_ln_resolving), style = FxType.meta, color = Fx.textDim)
        } else if (sendPlan != null) {
            val baseNeedsAmount = remember(lnText, network, lnState, limits, resolvedLnurl) {
                if (isLnurlTarget && resolvedLnurl == null) null
                else planSend(
                    text = lnText,
                    onchainSat = 0L,
                    ln = lnState,
                    network = network,
                    limits = limits,
                    userAmountMsat = null,
                    lnurlResolver = { resolvedLnurl ?: throw IllegalStateException() },
                    sendableMsat = vm.lightning.sendableMsat,
                extraSendableMsat = vm.accountBalance?.freeMsat ?: 0uL,
                ) as? SendPlan.NeedsAmount
            }

            if (baseNeedsAmount != null) {
                val minSat = displaySatFromMsat(baseNeedsAmount.minMsat)
                val maxSat = displaySatFromMsat(baseNeedsAmount.maxMsat)
                Text(
                    caps(stringResource(R.string.send_ln_amount_range, fmt(minSat), fmt(maxSat))),
                    style = FxType.label,
                )
                Row(
                    horizontalArrangement = Arrangement.spacedBy(Fx.s2),
                    verticalAlignment = Alignment.Top,
                ) {
                    Field(
                        amount, { amount = it }, null,
                        keyboardType = if (amountInSat) KeyboardType.Number else KeyboardType.Decimal,
                        modifier = Modifier.weight(1f),
                    )
                    AmountUnitPicker(amountInSat, unit) { amountInSat = it }
                }
            }

            when (val plan = sendPlan) {
                is SendPlan.Lightning -> {
                    val sat = plan.amountMsat?.let { displaySatFromMsat(it) } ?: amountSat ?: 0L
                    kv(stringResource(R.string.confirm_amount), "${fmt(sat)} $unit")
                    val desc = when (val t = plan.target) {
                        is LnTarget.Bolt11 -> t.description
                        is LnTarget.Bolt12Offer -> t.description
                        else -> resolvedLnurl?.description.orEmpty()
                    }
                    if (desc.isNotBlank()) {
                        kv(stringResource(R.string.confirm_ln_description), desc)
                    }
                }
                is SendPlan.NeedsAmount -> {}
                // Not enough on Lightning (a transfer from a BTC wallet comes later).
                is SendPlan.NeedsMove -> {
                    Text(
                        stringResource(R.string.ln_err_insufficient_lightning),
                        style = FxType.meta,
                        color = Fx.warn,
                    )
                }
                is SendPlan.Invalid -> {
                    // Off / Starting already shows the node's status above.
                    if (lnState !is LnState.Off && lnState !is LnState.Starting) ErrorText(plan.reason)
                }
                is SendPlan.OnChain -> {}
            }
        }

        ErrorText(vm.error)
        val focus = LocalFocusManager.current
        val lnReadyPlan = sendPlan as? SendPlan.Lightning
        PrimaryButton(
            stringResource(R.string.action_review),
            enabled = lnReadyPlan != null && (lnReadyPlan.amountMsat ?: 0uL) > 0uL,
            working = vm.reviewing,
        ) {
            focus.clearFocus()
            if (lnReadyPlan != null && lnReadyPlan.amountMsat != null) {
                vm.buildLightningPayment(
                    target = lnReadyPlan.target,
                    amountMsat = lnReadyPlan.amountMsat,
                    descriptionOverride = resolvedLnurl?.description,
                )
            }
        }
    }
}

/** The node's payments, newest first. */
@Composable
internal fun LightningHistory(vm: WalletViewModel) {
    val c = vm.config
    val lnState by vm.lightning.state.collectAsState()
    // Reads node files: loaded off the main thread, the rows fill in when it returns.
    val lnPayments by produceState(emptyList<uniffi.wallet_ffi.LnPayment>(), lnState, vm.sentCount) {
        value = vm.lightning.payments()
    }
    LaunchedEffect(lnState is LnState.Ready, vm.sentCount) { vm.loadLnMoves() }
    val rows = remember(lnPayments, vm.accountHistory, vm.lnMoves) { mergeLightningHistory(lnPayments, vm.accountHistory, vm.lnMoves) }
    // A ledger: each row's balance after it, worked back from the balance now.
    // Money already on its way out (a close confirming, then its sweep) was moved by the
    // ledger's move row: it is not in the balance the rows add up to, and that row is pending.
    val leaving = (lnState as? LnState.Ready)?.let { leavingLightningSat(it) } ?: 0L
    val total = c?.let { lightningWalletTotalSat(vm, it.chain) }
    val shown = remember(rows, leaving) {
        if (leaving <= 0L) rows else {
            val newestClose = rows.firstOrNull { it.noteRes == R.string.history_moved_onchain_closed }
            rows.map { if (it === newestClose) it.copy(pending = true) else it }
        }
    }
    val (after, unaccounted) = remember(shown, total) {
        if (total == null) emptyMap<String, Long>() to 0L else ledgerBalances(shown, total)
    }
    val unit = lnUnit(vm)
    HistoryList(vm, shown, unit, chain = c?.chain ?: LN_CHAIN, network = c?.network ?: "bitcoin", balanceAfter = after)
    if (total != null && rows.isNotEmpty() && unaccounted != 0L) GlassCard {
        Text(stringResource(R.string.history_unaccounted, fmt(unaccounted), unit), style = FxType.meta, color = Fx.warn)
    }
}

/** "Create a Lightning wallet to pay this": a Lightning link arrived and there is none. */
@Composable
internal fun CreateLightningWalletDialog(vm: WalletViewModel) {
    AlertDialog(
        onDismissRequest = { vm.dismissLightningWalletPrompt() },
        containerColor = Fx.panel,
        title = {
            Text(
                stringResource(
                    if (vm.lightningPromptChain == com.fortis.wallet.data.LN_XBT_CHAIN) R.string.send_create_lightning_xbt_wallet
                    else R.string.send_create_lightning_wallet,
                ),
                color = Fx.text,
            )
        },
        confirmButton = {
            TextButton({ vm.addLightningWallet() }) {
                Text(stringResource(R.string.action_add_wallet), color = Fx.signal)
            }
        },
        dismissButton = {
            TextButton({ vm.dismissLightningWalletPrompt() }) {
                Text(stringResource(R.string.action_cancel), color = Fx.text)
            }
        },
    )
}

/** Settings → Lightning (beta), one card per Lightning wallet the app has ([walletChain]
 *  "ln" or "ln-xbt"): the node's status, the takeover from another device, and "Close channel
 *  and move to on-chain" to a wallet of its own chain (with the force-close offer when the LSP
 *  cannot be reached). */
@Composable
internal fun LightningWalletSettingsCard(vm: WalletViewModel, walletChain: String) {
    val c = lightningWalletOf(vm.wallets, walletChain) ?: return
    val side = vm.lnSide(walletChain)
    val session = side.session
    val lnState by session.state.collectAsState()
    var confirmClose by remember { mutableStateOf(false) }

    GlassCard {
        Row(
            Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(Modifier.weight(1f)) {
                Text(stringResource(R.string.settings_lightning_title), color = Fx.text, fontWeight = FontWeight.SemiBold)
                Text(c.display, style = FxType.meta, color = Fx.textDim)
            }
        }
        Text(stringResource(R.string.settings_lightning_body), style = FxType.meta, color = Fx.textFaint)
        val statusText = lightningStatusText(lnState)
        val statusColor = when (lnState) {
            is LnState.Ready -> Fx.good
            is LnState.Error -> Fx.warn
            else -> Fx.textDim
        }
        Text(statusText, style = FxType.meta, color = statusColor)

        if (shouldShowTakeoverBanner(lnState)) {
            LightningTakeoverButton(session)
        }

        val ready = lnState as? LnState.Ready
        val remoteEnabled = session.lastInfo?.enabled != false
        val remoteMessage = session.lastInfo?.message?.takeIf { it.isNotBlank() }
        if (!remoteEnabled && remoteMessage != null) {
            Text(remoteMessage, style = FxType.meta, color = Fx.warn)
        }
        val canCloseOrSweep = ready != null && (
            ready.channel != uniffi.wallet_ffi.LnChannelState.NONE ||
                ready.balances.onchainTransitSat > 0uL
            )
        if (canCloseOrSweep) {
            GhostButton(
                stringResource(if (side.closing) R.string.settings_lightning_closing else R.string.settings_lightning_close_action),
                enabled = !side.closing,
                dense = true,
            ) { confirmClose = true }
        }
        ErrorText(side.closeError)
    }

    if (side.offerForceClose) {
        AlertDialog(
            onDismissRequest = { vm.dismissForceCloseOffer(walletChain) },
            containerColor = Fx.panel,
            title = { Text(stringResource(R.string.settings_lightning_force_title), color = Fx.text) },
            text = { Text(stringResource(R.string.settings_lightning_force_body), color = Fx.textDim) },
            confirmButton = {
                TextButton({
                    vm.dismissForceCloseOffer(walletChain)
                    vm.closeLightningAndMoveToOnchain(force = true, walletChain = walletChain)
                }) {
                    Text(stringResource(R.string.settings_lightning_force_action), color = Fx.warn)
                }
            },
            dismissButton = {
                TextButton({ vm.dismissForceCloseOffer(walletChain) }) {
                    Text(stringResource(R.string.action_cancel), color = Fx.text)
                }
            },
        )
    }

    if (confirmClose) {
        // Name the on-chain wallet (of this wallet's chain) the funds go to (the one chosen on
        // the Move tab, else the first).
        val sweepTo = vm.moveTargetWallet(walletChain)
        val closeBody = if (sweepTo != null) {
            stringResource(R.string.settings_lightning_close_body_to, sweepTo.display)
        } else {
            stringResource(R.string.settings_lightning_close_body)
        }
        AlertDialog(
            onDismissRequest = { confirmClose = false },
            containerColor = Fx.panel,
            title = { Text(stringResource(R.string.settings_lightning_close_title), color = Fx.text) },
            text = { Text(closeBody, color = Fx.textDim) },
            confirmButton = {
                TextButton({
                    confirmClose = false
                    vm.closeLightningAndMoveToOnchain(walletChain = walletChain)
                }) {
                    Text(stringResource(R.string.action_confirm), color = Fx.signal)
                }
            },
            dismissButton = {
                TextButton({ confirmClose = false }) {
                    Text(stringResource(R.string.action_cancel), color = Fx.text)
                }
            },
        )
    }
}


@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun LnConfirmSheet(vm: WalletViewModel, p: com.fortis.wallet.LnPlanPreview, unit: String) {
    val gate = com.fortis.wallet.ui.rememberIdentityGate()
    val gateAct = com.fortis.wallet.ui.rememberFragmentActivity()
    val gateScope = rememberCoroutineScope()
    com.fortis.wallet.ui.ConfirmIdentityDialog(gate, vm)
    val sheet = rememberModalBottomSheetState(
        skipPartiallyExpanded = true,
        confirmValueChange = { it != SheetValue.Hidden || !vm.sending },
    )
    ModalBottomSheet(
        onDismissRequest = { if (!vm.sending) vm.cancelPendingLn() },
        sheetState = sheet,
        containerColor = Fx.panel,
    ) {
        Column(
            Modifier.verticalScroll(rememberScrollState()).padding(horizontal = Fx.s5).padding(top = Fx.s2, bottom = Fx.s6),
            verticalArrangement = Arrangement.spacedBy(Fx.s4),
        ) {
            Text(
                stringResource(R.string.confirm_payment_title),
                style = MaterialTheme.typography.titleLarge, color = Fx.text,
            )
            val chain = p.walletChain
            val amountSat = displaySatFromMsat(p.amountMsat)
            val feeEstimateSat = displaySatFromMsat(p.feeEstimateMsat)

            @Composable
            fun amountLine(sat: Long): String {
                val fiat = vm.fiatText(sat, chain)
                return if (fiat != null) stringResource(R.string.value_with_fiat, fmt(sat), unit, fiat)
                else "${fmt(sat)} $unit"
            }

            Column(verticalArrangement = Arrangement.spacedBy(Fx.s1)) {
                Text(stringResource(R.string.confirm_to), style = FxType.body, color = Fx.textDim)
                Text(
                    if (p.recipientLabel.length > 48) "${p.recipientLabel.take(24)}…${p.recipientLabel.takeLast(16)}" else p.recipientLabel,
                    style = FxType.amount,
                    color = Fx.text,
                )
            }
            kv(stringResource(R.string.confirm_amount), amountLine(amountSat))
            if (p.viaAccount) kv(stringResource(R.string.ln_confirm_from), stringResource(R.string.ln_confirm_from_account))
            if (p.description.isNotBlank()) {
                kv(stringResource(R.string.confirm_ln_description), p.description)
            }
            p.expirySecs?.let { exp ->
                val expLabel = if (exp >= 60uL) {
                    stringResource(R.string.confirm_ln_expiry_mins, (exp / 60uL).toInt())
                } else {
                    stringResource(R.string.confirm_ln_expiry_secs, exp.toInt())
                }
                kv(stringResource(R.string.confirm_ln_expiry), expLabel)
            }
            kv(
                stringResource(R.string.confirm_ln_fee_estimate),
                stringResource(R.string.confirm_ln_fee_estimate_value, fmt(feeEstimateSat), unit),
            )
            if (vm.sending) {
                Text(stringResource(R.string.send_ln_sending), style = FxType.meta, color = Fx.textDim)
            }
            ErrorText(vm.error)
            Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
                GhostButton(stringResource(R.string.action_cancel), Modifier.weight(1f), enabled = !vm.sending) {
                    if (!vm.sending) vm.cancelPendingLn()
                }
                PrimaryButton(
                    stringResource(if (vm.sending) R.string.send_ln_sending else R.string.action_sign_send),
                    Modifier.weight(1f),
                    working = vm.sending,
                ) {
                    gate.require(vm, gateAct, gateScope) { vm.confirmLightningSend() }
                }
            }
        }
    }
}


/** Map every [com.fortis.wallet.lightning.LnFfiError] variant to a user-facing string resource. */
@androidx.annotation.StringRes
fun lnErrorMessage(e: com.fortis.wallet.lightning.LnFfiError): Int = when (e) {
    is uniffi.wallet_ffi.LnFfiException.BackupUnavailable -> R.string.ln_err_backup_unavailable
    is uniffi.wallet_ffi.LnFfiException.ActiveOnAnotherDevice -> R.string.ln_err_active_on_another_device
    is uniffi.wallet_ffi.LnFfiException.LspUnreachable -> R.string.ln_err_lsp_unreachable
    is uniffi.wallet_ffi.LnFfiException.InsufficientLightning -> R.string.ln_err_insufficient_lightning
    is uniffi.wallet_ffi.LnFfiException.NoRoute -> R.string.ln_err_no_route
    is uniffi.wallet_ffi.LnFfiException.InvoiceExpired -> R.string.ln_err_invoice_expired
    is uniffi.wallet_ffi.LnFfiException.WrongNetwork -> R.string.ln_err_wrong_network
    is uniffi.wallet_ffi.LnFfiException.WrongChain -> R.string.ln_err_wrong_chain
    is uniffi.wallet_ffi.LnFfiException.PaymentStateUnknown -> R.string.ln_err_payment_state_unknown
    is uniffi.wallet_ffi.LnFfiException.AmountOutOfRange -> R.string.ln_err_amount_out_of_range
    is uniffi.wallet_ffi.LnFfiException.Other -> R.string.ln_err_other
}

/**
 * The message for [e]: a specific one for the common causes, and for anything else the
 * generic one with its short code ("Lightning error (splice_failed)…"), so a user can
 * say which it was.
 */
fun lnErrorText(e: com.fortis.wallet.lightning.LnFfiError, get: (Int, Array<out Any>) -> String): String {
    if (e is uniffi.wallet_ffi.LnFfiException.Other) {
        val r = e.reason
        val res = when {
            r == "not_started" || r == "lightning not ready" || r == "start_in_progress" || r == "cancelled" -> R.string.ln_err_not_ready
            r.startsWith("chain_sync_failed") || r == "chain_source_timeout" -> R.string.ln_err_chain_sync
            r == "insufficient_channel" -> R.string.ln_err_insufficient_channel
            r == "no_channel" -> R.string.ln_err_no_channel
            r == "channel_opening" -> R.string.ln_err_channel_opening
            r == "Payment failed" -> R.string.ln_err_payment_failed
            r == "splice_failed" -> R.string.transfer_splice_busy
            else -> null
        }
        return if (res != null) get(res, arrayOf()) else get(R.string.ln_err_other_code, arrayOf(r.take(40)))
    }
    return get(lnErrorMessage(e), arrayOf())
}

/** True when the Lightning node is stopped because another device holds the VSS lock. */
fun shouldShowTakeoverBanner(state: LnState): Boolean =
    state is LnState.Error && state.error is uniffi.wallet_ffi.LnFfiException.ActiveOnAnotherDevice

/** The node of [session] stopped because another device holds its lock: offer the takeover. */
@Composable
internal fun LightningTakeoverBanner(session: com.fortis.wallet.lightning.LightningSession) {
    val lnState by session.state.collectAsState()
    if (!shouldShowTakeoverBanner(lnState)) return
    GlassCard {
        Text(
            stringResource(R.string.ln_err_active_on_another_device),
            style = FxType.meta,
            color = Fx.warn,
        )
        LightningTakeoverButton(session)
    }
}

@Composable
private fun LightningTakeoverButton(session: com.fortis.wallet.lightning.LightningSession) {
    var confirm by remember { mutableStateOf(false) }
    GhostButton(
        stringResource(R.string.ln_takeover_action),
        dense = true,
    ) { confirm = true }
    if (confirm) {
        AlertDialog(
            onDismissRequest = { confirm = false },
            containerColor = Fx.panel,
            title = { Text(stringResource(R.string.ln_takeover_title), color = Fx.text) },
            text = { Text(stringResource(R.string.ln_takeover_body), color = Fx.textDim) },
            confirmButton = {
                TextButton({
                    confirm = false
                    session.takeover()
                }) { Text(stringResource(R.string.action_confirm), color = Fx.signal) }
            },
            dismissButton = {
                TextButton({ confirm = false }) {
                    Text(stringResource(R.string.action_cancel), color = Fx.text)
                }
            },
        )
    }
}


/** The wallet's reusable BOLT12 address: one QR to share, paid any number of times, even while
 *  this phone is off (the LSP receives it into the Fortis balance). */
@Composable
internal fun ReusableOfferCard(vm: WalletViewModel) {
    val ctx = LocalContext.current
    val offer = vm.accountOffer
    GlassCard {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            com.fortis.wallet.ui.LightningBoltIcon()
            Text(stringResource(R.string.ln_offer_title), color = Fx.text, fontWeight = FontWeight.SemiBold)
        }
        Text(stringResource(R.string.ln_offer_help), style = FxType.meta, color = Fx.textDim)
        vm.accountOfferError?.let { Text(it, style = FxType.meta, color = Fx.warn) }
        if (offer == null) {
            GhostButton(stringResource(R.string.ln_offer_show)) { vm.loadAccountOffer() }
        } else {
            val qr = remember(offer) { qrBitmap(offer.uppercase()) }
            // Like every QR in the app: a tap (or long press) copies the QR image.
            val copyQr = { copyImageToClipboard(ctx, qr) }
            Image(
                bitmap = qr.asImageBitmap(),
                contentDescription = stringResource(R.string.cd_address_qr),
                modifier = Modifier
                    .align(Alignment.CenterHorizontally)
                    .size(224.dp)
                    .clip(RoundedCornerShape(Fx.rSm))
                    .background(Color.White)
                    .combinedClickable(onClick = copyQr, onLongClick = copyQr, onDoubleClick = copyQr)
                    .padding(10.dp),
            )
            XbtLightningCaption(vm, Modifier.align(Alignment.CenterHorizontally))
            val copy = { copyToClipboard(ctx, "offer", offer) }
            Text(
                "${offer.take(24)}…${offer.takeLast(16)}",
                style = FxType.amount,
                modifier = Modifier
                    .fillMaxWidth()
                    .clip(RoundedCornerShape(Fx.rSm))
                    .background(Fx.panelRaised)
                    .clickable { copy() }
                    .padding(Fx.s4),
            )
            Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
                GhostButton(stringResource(R.string.action_copy), Modifier.weight(1f)) { copy() }
                GhostButton(stringResource(R.string.action_share), Modifier.weight(1f)) {
                    ctx.startActivity(
                        Intent.createChooser(
                            Intent(Intent.ACTION_SEND).apply { type = "text/plain"; putExtra(Intent.EXTRA_TEXT, offer) },
                            null,
                        ),
                    )
                }
            }
        }
    }
}

/** "XBT Lightning" under the XBT wallet's QR codes: they look like any Lightning QR, and only
 *  an XBT Lightning wallet can pay them. Nothing for the BTC wallet. */
@Composable
private fun XbtLightningCaption(vm: WalletViewModel, modifier: Modifier = Modifier) {
    if (vm.config?.chain != com.fortis.wallet.data.LN_XBT_CHAIN) return
    Text(stringResource(R.string.ln_qr_caption_xbt), style = FxType.label, color = Fx.signal, modifier = modifier)
}
