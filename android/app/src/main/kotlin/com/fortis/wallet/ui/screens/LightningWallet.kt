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
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import uniffi.wallet_ffi.LnChannelState
import uniffi.wallet_ffi.LnTarget
import uniffi.wallet_ffi.LnUnifiedLightning

/*
 * The Lightning wallet (chain "ln", one per app): its own tab — balance, Receive /
 * Send / History on the node — its settings card, and the Lightning confirm sheet.
 * BTC wallets are on-chain only; a Lightning target there hands off to this wallet.
 */

private fun fmt(sat: Long) = fmtCoin(sat)

/** Lightning amounts are bitcoin. */
private const val LN_UNIT = "BTC"

/** How long the receive amount must stand still before a Lightning quote is
 *  bought for it. */
private const val QUOTE_DEBOUNCE_MS = 1_500L

/** How long the Send field must stand still before an LNURL / Lightning
 *  address in it is looked up (it contacts that address's server). */
private const val LNURL_DEBOUNCE_MS = 1_000L

/** The Lightning wallet's balance (Lightning plus funds moving on-chain), or null while
 *  the node has not reported one yet. */
@Composable
internal fun lightningWalletBalance(vm: WalletViewModel): HomeBalance? {
    val lnState by vm.lightning.state.collectAsState()
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
    is LnState.Error -> stringResource(lnErrorMessage(st.error))
}

/** Wallet tab for the Lightning wallet: its balance, then Receive / Send / History. */
@Composable
fun LightningWalletTab(vm: WalletViewModel) {
    val c = vm.config ?: return
    var tab by remember { mutableStateOf("receive") }
    val lnState by vm.lightning.state.collectAsState()
    val hb = lightningWalletBalance(vm)

    Column(
        Modifier.fillMaxSize().padding(top = Fx.s2),
        verticalArrangement = Arrangement.spacedBy(Fx.s4),
    ) {
        LightningTakeoverBanner(vm)
        GlassCard(hero = true) {
            Column(Modifier.fillMaxWidth()) {
                Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                    Text(caps(c.display), style = FxType.label, modifier = Modifier.weight(1f))
                    TextButton({ vm.lock() }) { Text(stringResource(R.string.action_lock), color = Fx.textDim) }
                }
                Column {
                    FlowRow {
                        val (whole, fraction) = splitAmount(if (hb != null) fmt(countUpSat(hb.totalSat)) else "—")
                        Text(whole, style = FxType.balance, modifier = Modifier.alignByBaseline())
                        if (fraction.isNotEmpty()) Text(fraction, style = FxType.balance, modifier = Modifier.alignByBaseline())
                        Text(LN_UNIT, style = FxType.label, color = Fx.signal, modifier = Modifier.alignByBaseline().padding(start = 6.dp))
                    }
                    hb?.let { bal ->
                        vm.fiatText(bal.totalSat, LN_CHAIN)?.let {
                            Text(stringResource(R.string.fiat_approx, it), style = FxType.fiat)
                        }
                    }
                    // Part of it is still on-chain: a channel opening or closing.
                    if (hb?.moving == true) Text(
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
                }
            }
        }
        val tabKeys = listOf("receive", "send", "history")
        LaunchedEffect(vm.pendingSendText, c.id) {
            if (vm.pendingSendText != null) tab = "send"
        }
        val tabLabels = mapOf(
            "receive" to stringResource(R.string.tab_receive),
            "send" to stringResource(R.string.tab_send),
            "history" to stringResource(R.string.tab_history),
        )
        TextTabs(tabKeys.map { it to tabLabels.getValue(it) }, tab, { tab = it })
        vm.lastSentTxid?.let { id -> SentDialog(vm, id) }
        Column(
            Modifier.weight(1f).fillMaxWidth().verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(Fx.s4),
        ) {
            when (tab) {
                "receive" -> LightningReceiveTab(vm)
                "send" -> key(vm.sentCount) { LightningSendTab(vm) } // a sent payment starts the form over
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
    val lnState by vm.lightning.state.collectAsState()
    val minLnSat = vm.lightning.lastInfo?.minPaymentSat ?: 10_000L
    val maxLnSat = vm.lightning.lastInfo?.maxPaymentSat ?: (DEFAULT_LN_LIMITS.maxPaymentMsat / 1_000uL).toLong()
    val remoteEnabled = vm.lightning.lastInfo?.enabled != false
    val remoteMessage = vm.lightning.lastInfo?.message?.takeIf { it.isNotBlank() }

    var amount by remember { mutableStateOf("") }
    var amountInSat by remember { mutableStateOf(true) }
    val amountSat = amountToSat(amount, amountInSat)
    var quote by remember { mutableStateOf<uniffi.wallet_ffi.LnReceiveQuote?>(null) }
    var quoting by remember { mutableStateOf(false) }

    // Keyed on "is it ready", not on the whole state: a balance update must not
    // buy another quote (each one is an LSPS2 request and a stored payment).
    val lnReady = lnState is LnState.Ready
    LaunchedEffect(amountSat, lnReady, remoteEnabled) {
        quote = null
        quoting = false
        if (amountSat != null && amountSat >= minLnSat && lnReady) {
            quoting = true
            kotlinx.coroutines.delay(QUOTE_DEBOUNCE_MS)
            quote = kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
                runCatching { vm.lightning.currentNode?.receive(amountSat.toULong() * 1_000uL, "") }
                    .getOrNull()
                    ?.takeIf { remoteEnabled || !it.needsChannel }
            }
            quoting = false
        }
    }

    val rxQr = remember(amountSat, quote, minLnSat) { buildLightningReceiveQr(amountSat, quote, minLnSat) }
    val invoice = quote?.invoice?.takeIf { rxQr.lightning }

    val chooserTitle = stringResource(R.string.share_address_chooser)
    GlassCard {
        Text(stringResource(R.string.tab_receive), color = Fx.text, fontWeight = FontWeight.SemiBold)
        val ready = lnState as? LnState.Ready
        if (ready != null && ready.channel == LnChannelState.NONE && ready.balances.lightningSat == 0uL) {
            Text(stringResource(R.string.ln_wallet_first_receive_hint), style = FxType.meta, color = Fx.textDim)
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
            AmountUnitPicker(amountInSat, LN_UNIT) { amountInSat = it }
        }
        if (!lnReady) {
            Text(lightningStatusText(lnState), style = FxType.meta, color = Fx.textDim)
        } else if (quoting) {
            Text(stringResource(R.string.receive_ln_quoting), style = FxType.meta, color = Fx.textDim)
        } else if (rxQr.lightning && rxQr.feeSat != null && rxQr.feeSat > 0L && rxQr.youGetSat != null && amountSat != null) {
            Text(
                stringResource(
                    R.string.receive_ln_fee_summary,
                    fmt(amountSat),
                    LN_UNIT,
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
                    .combinedClickable(onClick = {}, onLongClick = copyQr, onDoubleClick = copyQr)
                    .padding(10.dp),
            )
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
                lnurlError = ctx.getString(lnErrorMessage(e))
            } catch (e: Exception) {
                lnurlError = e.message ?: e.toString()
            } finally {
                resolvingLnurl = false
            }
        }
    }

    val sendPlan = remember(to, lnText, network, lnState, limits, amountSat, resolvedLnurl, lnurlError, isLnurlTarget, onchainOnly) {
        if (to.isBlank() || onchainOnly) null
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
            )
        }
    }

    val scanPrompt = stringResource(R.string.scan_prompt)
    val scan = rememberLauncherForActivityResult(ScanContract()) { r ->
        r.contents?.let { raw -> to = textFromIntent(raw) ?: raw.trim() }
    }
    GlassCard {
        Text(stringResource(R.string.tab_send), color = Fx.text, fontWeight = FontWeight.SemiBold)
        Field(to, { new -> to = new.filterNot(Char::isWhitespace) }, stringResource(R.string.field_to_address), mono = true)
        GhostButton(stringResource(R.string.action_scan_qr)) {
            scan.launch(
                ScanOptions().apply {
                    setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                    setPrompt(scanPrompt)
                    setBeepEnabled(false)
                    setOrientationLocked(false)
                },
            )
        }

        if (lnState is LnState.Off || lnState is LnState.Starting) {
            Text(lightningStatusText(lnState), style = FxType.meta, color = Fx.textDim)
        }
        if (onchainOnly) {
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
                    AmountUnitPicker(amountInSat, LN_UNIT) { amountInSat = it }
                }
            }

            when (val plan = sendPlan) {
                is SendPlan.Lightning -> {
                    val sat = plan.amountMsat?.let { displaySatFromMsat(it) } ?: amountSat ?: 0L
                    kv(stringResource(R.string.confirm_amount), "${fmt(sat)} $LN_UNIT")
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
    val rows = remember(lnPayments) { mergeHistory(emptyList(), lnPayments) }
    HistoryList(vm, rows, LN_UNIT, chain = LN_CHAIN, network = c?.network ?: "bitcoin")
}

/** "Create a Lightning wallet to pay this": a Lightning link arrived and there is none. */
@Composable
internal fun CreateLightningWalletDialog(vm: WalletViewModel) {
    AlertDialog(
        onDismissRequest = { vm.dismissLightningWalletPrompt() },
        containerColor = Fx.panel,
        title = { Text(stringResource(R.string.send_create_lightning_wallet), color = Fx.text) },
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

/** Settings → Lightning (beta), while the app has a Lightning wallet: the node's status,
 *  the takeover from another device, and "Close channel and move to on-chain" (with the
 *  force-close offer when the LSP cannot be reached). */
@Composable
internal fun LightningWalletSettingsCard(vm: WalletViewModel) {
    val c = lightningWalletOf(vm.wallets) ?: return
    val lnState by vm.lightning.state.collectAsState()
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
            LightningTakeoverButton(vm)
        }

        val ready = lnState as? LnState.Ready
        val remoteEnabled = vm.lightning.lastInfo?.enabled != false
        val remoteMessage = vm.lightning.lastInfo?.message?.takeIf { it.isNotBlank() }
        if (!remoteEnabled && remoteMessage != null) {
            Text(remoteMessage, style = FxType.meta, color = Fx.warn)
        }
        val canCloseOrSweep = ready != null && (
            ready.channel != uniffi.wallet_ffi.LnChannelState.NONE ||
                ready.balances.onchainTransitSat > 0uL
            )
        if (canCloseOrSweep) {
            GhostButton(
                stringResource(if (vm.closingLightningChannel) R.string.settings_lightning_closing else R.string.settings_lightning_close_action),
                enabled = !vm.closingLightningChannel,
                dense = true,
            ) { confirmClose = true }
        }
        ErrorText(vm.lightningCloseError)
    }

    if (vm.offerForceClose) {
        AlertDialog(
            onDismissRequest = { vm.dismissForceCloseOffer() },
            containerColor = Fx.panel,
            title = { Text(stringResource(R.string.settings_lightning_force_title), color = Fx.text) },
            text = { Text(stringResource(R.string.settings_lightning_force_body), color = Fx.textDim) },
            confirmButton = {
                TextButton({
                    vm.dismissForceCloseOffer()
                    vm.closeLightningAndMoveToOnchain(force = true)
                }) {
                    Text(stringResource(R.string.settings_lightning_force_action), color = Fx.warn)
                }
            },
            dismissButton = {
                TextButton({ vm.dismissForceCloseOffer() }) {
                    Text(stringResource(R.string.action_cancel), color = Fx.text)
                }
            },
        )
    }

    if (confirmClose) {
        // Name the BTC wallet the funds go to (the first software mainnet BTC wallet).
        val sweepTo = com.fortis.wallet.lightning.lightningSweepWallet(vm.wallets)
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
                    vm.closeLightningAndMoveToOnchain()
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
            val chain = LN_CHAIN
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
    is uniffi.wallet_ffi.LnFfiException.PaymentStateUnknown -> R.string.ln_err_payment_state_unknown
    is uniffi.wallet_ffi.LnFfiException.AmountOutOfRange -> R.string.ln_err_amount_out_of_range
    is uniffi.wallet_ffi.LnFfiException.Other -> R.string.ln_err_other
}

/** True when the Lightning node is stopped because another device holds the VSS lock. */
fun shouldShowTakeoverBanner(state: LnState): Boolean =
    state is LnState.Error && state.error is uniffi.wallet_ffi.LnFfiException.ActiveOnAnotherDevice

@Composable
internal fun LightningTakeoverBanner(vm: WalletViewModel) {
    val lnState by vm.lightning.state.collectAsState()
    if (!shouldShowTakeoverBanner(lnState)) return
    GlassCard {
        Text(
            stringResource(R.string.ln_err_active_on_another_device),
            style = FxType.meta,
            color = Fx.warn,
        )
        LightningTakeoverButton(vm)
    }
}

@Composable
private fun LightningTakeoverButton(vm: WalletViewModel) {
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
                    vm.lightning.takeover()
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
