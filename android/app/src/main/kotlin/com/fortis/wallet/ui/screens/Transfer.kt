package com.fortis.wallet.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.layout.Column
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import com.fortis.wallet.R
import com.fortis.wallet.WalletViewModel
import com.fortis.wallet.lightning.LnState
import com.fortis.wallet.ui.ErrorText
import com.fortis.wallet.ui.ValidationText
import com.fortis.wallet.ui.require
import com.fortis.wallet.ui.GlassCard
import com.fortis.wallet.ui.PrimaryButton
import com.fortis.wallet.ui.fmtCoin
import com.fortis.wallet.ui.theme.Fx
import com.fortis.wallet.ui.theme.FxType

private fun fmt(sat: Long) = "%,d sat".format(sat)

/** On-chain wallet → the Lightning wallet of its chain (spec §2; XBT → Lightning (XBT)). The
 *  deposit is reviewed and signed in the usual confirm sheet. */
@Composable
internal fun TransferToLightningTab(vm: WalletViewModel) {
    val chain = vm.config?.chain ?: "btc"
    // The wallet in view is on-chain: its Lightning side is its own chain's.
    val lnState by vm.lightning.state.collectAsState()
    val maxSat = vm.balances?.confirmedSat ?: 0L
    var amount by rememberSaveable { mutableStateOf("") }
    var inSat by rememberSaveable { mutableStateOf(true) }
    val amountSat = amountToSat(amount, inSat) ?: 0L
    GlassCard {
        Column(verticalArrangement = Arrangement.spacedBy(Fx.s3)) {
            Text(stringResource(R.string.transfer_to_lightning_title), color = Fx.text, fontWeight = FontWeight.SemiBold)
            Text(stringResource(R.string.transfer_to_lightning_help), style = FxType.meta, color = Fx.textDim)
            val moving by vm.lightning.transferPending.collectAsState()
            when (com.fortis.wallet.lightning.moveInNotice(maxSat, moving)) {
                com.fortis.wallet.lightning.MoveInNotice.NothingToMove ->
                    Text(stringResource(R.string.transfer_nothing_to_move), style = FxType.meta, color = Fx.warn)
                com.fortis.wallet.lightning.MoveInNotice.UnderWay ->
                    Text(stringResource(R.string.transfer_pending), style = FxType.meta, color = Fx.good)
                com.fortis.wallet.lightning.MoveInNotice.None -> {}
            }
            SwapAmount(amount, { amount = it }, chain, maxSat, hint = R.string.transfer_help_slider, inSat = inSat, onUnit = { inSat = it })
            val feerate = vm.feerates[6] ?: 2L
            val q = com.fortis.wallet.lightning.quoteToLightning(amountSat, 150 * feerate, feerate, 0L)
            if (amountSat > maxSat) ValidationText(stringResource(R.string.transfer_more_than_balance))
            else if (amountSat > 0) {
                if (q == null) ValidationText(stringResource(R.string.transfer_too_small))
                else Text(
                    stringResource(R.string.transfer_summary, fmt(amountSat), fmt(q.feeSat), fmt(q.arrivesSat)),
                    style = FxType.meta, color = Fx.textDim,
                )
            }
            if (lnState !is LnState.Ready) Text(stringResource(R.string.transfer_needs_lightning), style = FxType.meta, color = Fx.warn)
            ErrorText(vm.error)
            PrimaryButton(
                stringResource(R.string.transfer_review),
                enabled = q != null && amountSat <= maxSat && lnState is LnState.Ready && !vm.reviewing,
                working = vm.reviewing,
            ) { vm.moveToLightning(amountSat) }
        }
    }
}

/** Lightning wallet → an on-chain wallet of its own chain (spec §2): a splice-out, or a close
 *  for everything. */
@Composable
internal fun TransferToOnchainTab(vm: WalletViewModel) {
    val chain = com.fortis.wallet.lightning.onchainChainForLightning(vm.config?.chain ?: "") ?: "btc"
    val lnState by vm.lightning.state.collectAsState()
    val ready = lnState as? LnState.Ready
    val channelSat = ready?.balances?.lightningSat?.toLong() ?: 0L
    // What is held by Fortis can move out too (the LSP sends it), one source at a time.
    val accountSat = ((vm.accountBalance?.freeMsat ?: 0uL) / 1_000uL).toLong()
    val maxSat = maxOf(channelSat, accountSat)
    var amount by rememberSaveable { mutableStateOf("") }
    var inSat by rememberSaveable { mutableStateOf(true) }
    val amountSat = amountToSat(amount, inSat) ?: 0L
    val gate = com.fortis.wallet.ui.rememberIdentityGate()
    val gateAct = com.fortis.wallet.ui.rememberFragmentActivity()
    val gateScope = androidx.compose.runtime.rememberCoroutineScope()
    com.fortis.wallet.ui.ConfirmIdentityDialog(gate, vm)
    // The quote's network fee is the destination chain's (a Lightning tab has no feerates of its own).
    androidx.compose.runtime.LaunchedEffect(vm.moveToWalletId) { vm.refreshMoveFeerate() }
    GlassCard {
        Column(verticalArrangement = Arrangement.spacedBy(Fx.s3)) {
            Text(stringResource(R.string.transfer_to_onchain_title), color = Fx.text, fontWeight = FontWeight.SemiBold)
            Text(stringResource(com.fortis.wallet.lightning.toOnchainHelpRes(vm.config?.chain ?: "")), style = FxType.meta, color = Fx.textDim)
            MoveToWalletPicker(vm)
            vm.lastMoveToOnchain?.let { m ->
                Text(
                    if (m.closing) stringResource(
                        R.string.transfer_moved_closing,
                        // What arrives, not what left before fees: the node's own figure once it has seen the closing
                        // transaction, else the quote's.
                        fmt((ready?.balances?.pendingCloseSat?.toLong()?.takeIf { it > 0L } ?: m.payoutSat.takeIf { it > 0L } ?: m.amountSat)),
                        m.toWallet,
                    )
                    else stringResource(R.string.transfer_moved, fmt(m.amountSat), m.toWallet, fmt(m.payoutSat), fmt(m.feeSat)),
                    style = FxType.meta, color = Fx.good,
                )
            }
            if (maxSat <= 0L && vm.lastMoveToOnchain == null) {
                Text(stringResource(R.string.transfer_nothing_to_move_ln), style = FxType.meta, color = Fx.textDim)
            }
            // The wallet total is more than the channel: the parts, and that they add up to it.
            val changeSat = ready?.balances?.onchainTransitSat?.toLong() ?: 0L
            val arrivingSat = ready?.balances?.pendingTransitSat?.toLong() ?: 0L
            val closingSat = ready?.balances?.pendingCloseSat?.toLong() ?: 0L
            val fortisTotalSat = ((vm.accountBalance?.balanceMsat ?: 0uL) / 1_000uL).toLong()
            if (ready != null && (changeSat > 0L || arrivingSat > 0L || closingSat > 0L || fortisTotalSat > 0L)) {
                kv(stringResource(R.string.transfer_math_channel), fmt(channelSat))
                if (changeSat > 0L) kv(stringResource(R.string.transfer_math_change), "+" + fmt(changeSat))
                if (arrivingSat > 0L) kv(stringResource(R.string.transfer_math_arriving), "+" + fmt(arrivingSat))
                if (closingSat > 0L) kv(stringResource(R.string.transfer_math_closing), "+" + fmt(closingSat))
                if (fortisTotalSat > 0L) kv(stringResource(R.string.transfer_math_fortis), "+" + fmt(fortisTotalSat))
                kv(stringResource(R.string.transfer_math_total), "= " + fmt(channelSat + changeSat + arrivingSat + closingSat + fortisTotalSat))
                Text(stringResource(R.string.transfer_math_note), style = FxType.meta, color = Fx.textDim)
            }

            // Nothing moves until the person has read the ledger and said yes.
            var confirmAll by androidx.compose.runtime.remember { mutableStateOf(false) }
            var confirmPart by androidx.compose.runtime.remember { mutableStateOf(false) }
            val destName = vm.moveTargetWallet(vm.config?.chain ?: "")?.display ?: ""
            // The same parts the header adds, so the ledger's total is the number shown above.
            val walletRows = buildList {
                add(stringResource(R.string.transfer_math_channel) to fmt(channelSat))
                if (changeSat > 0L) add(stringResource(R.string.transfer_math_change) to "+" + fmt(changeSat))
                if (arrivingSat > 0L) add(stringResource(R.string.transfer_math_arriving) to "+" + fmt(arrivingSat))
                if (closingSat > 0L) add(stringResource(R.string.transfer_math_closing) to "+" + fmt(closingSat))
                if (fortisTotalSat > 0L) add(stringResource(R.string.transfer_math_fortis) to "+" + fmt(fortisTotalSat))
            }
            val walletTotalSat = channelSat + changeSat + arrivingSat + closingSat + fortisTotalSat
            val transferPending by vm.lightning.transferPending.collectAsState()
            val channelReady = ready != null && ready.channel == uniffi.wallet_ffi.LnChannelState.READY &&
                channelSat > 0L && !transferPending

            // Option 2: move an amount. A portion moves and the rest stays in the channel. Whatever is left
            // over is the user's choice; if the channel cannot do it, the move fails with its message
            // before anything is paid.
            val sliderMax = maxSat
            if (sliderMax > 0L) {
                SwapAmount(amount, { amount = it }, chain, sliderMax, hint = R.string.transfer_help_slider_part, inSat = inSat, onUnit = { inSat = it })
                val fromAccount = amountSat > channelSat
                val q = vm.quoteMoveToOnchain(amountSat)
                if (amountSat > sliderMax) ValidationText(stringResource(R.string.transfer_more_than_balance))
                else if (amountSat > 0) {
                    if (q == null) ValidationText(stringResource(R.string.transfer_too_small))
                    else Text(
                        stringResource(R.string.transfer_summary_onchain, fmt(amountSat), fmt(q.feeSat), fmt(q.payoutSat)),
                        style = FxType.meta, color = Fx.textDim,
                    )
                    if (fromAccount) Text(stringResource(R.string.transfer_from_account), style = FxType.meta, color = Fx.textDim)
                }
                ErrorText(vm.error)
                PrimaryButton(
                    stringResource(R.string.transfer_move_amount),
                    enabled = q != null && amountSat <= sliderMax && ready != null && !vm.movingToOnchain,
                    working = vm.movingToOnchain,
                ) {
                    confirmPart = true
                }
                if (confirmPart && q != null) MoveConfirmDialog(
                    title = stringResource(R.string.move_confirm_part_title),
                    walletRows = buildList {
                        add(stringResource(R.string.move_confirm_moving_now) to fmt(amountSat))
                        if (!fromAccount) add(stringResource(R.string.move_confirm_stays_channel) to fmt(channelSat - amountSat))
                        if (changeSat > 0L) add(stringResource(R.string.transfer_math_change) to "+" + fmt(changeSat))
                        if (arrivingSat > 0L) add(stringResource(R.string.transfer_math_arriving) to "+" + fmt(arrivingSat))
                        if (closingSat > 0L) add(stringResource(R.string.transfer_math_closing) to "+" + fmt(closingSat))
                        // Held by Fortis moves from the account, not the channel: the channel keeps its balance.
                        if (fromAccount) add(stringResource(R.string.transfer_math_channel) to "+" + fmt(channelSat))
                        val otherFortis = if (fromAccount) fortisTotalSat - amountSat else fortisTotalSat
                        if (otherFortis > 0L) add(stringResource(R.string.transfer_math_fortis) to "+" + fmt(otherFortis))
                    },
                    walletTotal = fmt(walletTotalSat),
                    happensRows = buildList {
                        add(stringResource(R.string.move_confirm_moving_now) to fmt(amountSat))
                        val network = q.feeSat - q.shareSat
                        if (network > 0L) add(stringResource(R.string.move_confirm_network_fee) to "\u2212" + fmt(network))
                        if (q.shareSat > 0L) add(stringResource(R.string.move_confirm_share) to "\u2212" + fmt(q.shareSat))
                    },
                    receiveLabel = stringResource(R.string.move_confirm_receive, destName),
                    receive = fmt(q.payoutSat),
                    after = null,
                    onYes = {
                        confirmPart = false
                        // The quote shown is the one confirmed: it runs at its own feerate.
                        gate.require(vm, gateAct, gateScope) { vm.moveToOnchain(q, amountSat, false, fromAccount) }
                    },
                    onNo = { confirmPart = false },
                )
            } else {
                ErrorText(vm.error)
            }

            // A separate button below: move everything and close the channel. No amount to choose.
            if (channelReady) {
                val qAll = vm.quoteMoveToOnchain(channelSat, closing = true)
                if (qAll != null) Text(
                    stringResource(R.string.transfer_summary_onchain, fmt(channelSat), fmt(qAll.feeSat), fmt(maxOf(0L, qAll.payoutSat))),
                    style = FxType.meta, color = Fx.textDim,
                )
                Text(stringResource(R.string.transfer_all_closes), style = FxType.meta, color = Fx.textDim)
                if (qAll != null && qAll.payoutSat < com.fortis.wallet.lightning.DUST_SAT) Text(stringResource(R.string.transfer_close_little), style = FxType.meta, color = Fx.warn)
                PrimaryButton(
                    stringResource(R.string.transfer_close_all),
                    enabled = !vm.movingToOnchain,
                    working = vm.movingToOnchain,
                ) {
                    confirmAll = true
                }
                if (confirmAll) MoveConfirmDialog(
                    title = stringResource(R.string.move_confirm_close_title),
                    walletRows = walletRows,
                    walletTotal = fmt(walletTotalSat),
                    happensRows = buildList {
                        add(stringResource(R.string.move_confirm_leaves_channel) to fmt(channelSat))
                        if (qAll != null) {
                            val network = qAll.feeSat - qAll.shareSat
                            if (network > 0L) add(stringResource(R.string.move_confirm_network_fee) to "\u2212" + fmt(network))
                            if (qAll.shareSat > 0L) add(stringResource(R.string.move_confirm_share) to "\u2212" + fmt(qAll.shareSat))
                        }
                    },
                    receiveLabel = stringResource(R.string.move_confirm_receive, destName),
                    receive = fmt(maxOf(0L, qAll?.payoutSat ?: channelSat)),
                    after = if (changeSat > 0L) stringResource(R.string.move_confirm_after_close, fmt(changeSat)) else null,
                    onYes = {
                        confirmAll = false
                        gate.require(vm, gateAct, gateScope) { vm.moveToOnchain(qAll, channelSat, true, false) }
                    },
                    onNo = { confirmAll = false },
                )
            }

        }
    }
}

/** A move leaving less than this in the channel moves everything (closes it). */
private const val MOVE_ALL_SLACK_SAT = 3_000L

/** "To wallet": which on-chain wallet (of the Lightning wallet's own chain) the moved funds go
 *  to; a menu when there are several. */
@androidx.compose.runtime.Composable
private fun MoveToWalletPicker(vm: WalletViewModel) {
    val choices = vm.moveTargetChoices
    val current = vm.moveTargetWallet() ?: return
    var open by androidx.compose.runtime.remember { androidx.compose.runtime.mutableStateOf(false) }
    androidx.compose.foundation.layout.Row(verticalAlignment = androidx.compose.ui.Alignment.CenterVertically) {
        Text(
            stringResource(R.string.transfer_to_wallet),
            style = FxType.meta, color = Fx.textDim,
            modifier = androidx.compose.ui.Modifier.weight(1f),
        )
        if (choices.size < 2) {
            Text(current.display, color = Fx.text)
        } else androidx.compose.foundation.layout.Box {
            androidx.compose.material3.TextButton({ open = true }) { Text(current.display + " ▾", color = Fx.signal) }
            androidx.compose.material3.DropdownMenu(open, { open = false }, containerColor = Fx.panel) {
                choices.forEach { w ->
                    androidx.compose.material3.DropdownMenuItem(
                        text = { Text(w.display, color = if (w.id == current.id) Fx.signal else Fx.text) },
                        onClick = { vm.setMoveToWallet(w.id); open = false },
                    )
                }
            }
        }
    }
}


/**
 * The last thing seen before money moves: a ledger of where the wallet's sats are (its total is the number
 * shown on the wallet), then what happens to them, the fees, and what arrives. "Yes" runs the move.
 */
@androidx.compose.runtime.Composable
private fun MoveConfirmDialog(
    title: String,
    walletRows: List<Pair<String, String>>,
    walletTotal: String,
    happensRows: List<Pair<String, String>>,
    receiveLabel: String,
    receive: String,
    after: String?,
    onYes: () -> Unit,
    onNo: () -> Unit,
) {
    androidx.compose.material3.AlertDialog(
        onDismissRequest = onNo,
        containerColor = Fx.panel,
        title = { Text(title, color = Fx.text) },
        text = {
            Column(
                androidx.compose.ui.Modifier.verticalScroll(androidx.compose.foundation.rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(Fx.s2),
            ) {
                Text(stringResource(R.string.move_confirm_wallet_header), style = FxType.label, color = Fx.textDim)
                walletRows.forEach { (k, v) -> kv(k, v) }
                kv(stringResource(R.string.transfer_math_total), "= $walletTotal")
                Text(stringResource(R.string.move_confirm_happens_header), style = FxType.label, color = Fx.textDim)
                happensRows.forEach { (k, v) -> kv(k, v) }
                kv(receiveLabel, "= $receive")
                if (after != null) Text(after, style = FxType.meta, color = Fx.textDim)
                Text(stringResource(R.string.move_confirm_fee_note), style = FxType.meta, color = Fx.textDim)
                Text(stringResource(R.string.move_confirm_sure), color = Fx.text, fontWeight = FontWeight.SemiBold)
            }
        },
        confirmButton = {
            androidx.compose.material3.TextButton(onYes) { Text(stringResource(R.string.move_confirm_yes), color = Fx.signal) }
        },
        dismissButton = {
            androidx.compose.material3.TextButton(onNo) { Text(stringResource(R.string.move_confirm_no), color = Fx.textDim) }
        },
    )
}
