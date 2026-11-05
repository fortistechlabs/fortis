package com.fortis.wallet.ui.screens

import androidx.compose.foundation.layout.Arrangement
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
import com.fortis.wallet.ui.require
import com.fortis.wallet.ui.GlassCard
import com.fortis.wallet.ui.PrimaryButton
import com.fortis.wallet.ui.fmtCoin
import com.fortis.wallet.ui.theme.Fx
import com.fortis.wallet.ui.theme.FxType

private fun fmt(sat: Long) = "%,d sat".format(sat)

/** On-chain wallet → Lightning wallet (spec §2). The deposit is reviewed and signed in the
 *  usual confirm sheet. */
@Composable
internal fun TransferToLightningTab(vm: WalletViewModel) {
    val lnState by vm.lightning.state.collectAsState()
    val maxSat = vm.balances?.confirmedSat ?: 0L
    var amount by rememberSaveable { mutableStateOf("") }
    var inSat by rememberSaveable { mutableStateOf(true) }
    val amountSat = amountToSat(amount, inSat) ?: 0L
    GlassCard {
        Column(verticalArrangement = Arrangement.spacedBy(Fx.s3)) {
            Text(stringResource(R.string.transfer_to_lightning_title), color = Fx.text, fontWeight = FontWeight.SemiBold)
            Text(stringResource(R.string.transfer_to_lightning_help), style = FxType.meta, color = Fx.textDim)
            if (maxSat <= 0L) Text(stringResource(R.string.transfer_nothing_to_move), style = FxType.meta, color = Fx.warn)
            SwapAmount(amount, { amount = it }, "btc", maxSat, hint = R.string.transfer_help_slider, inSat = inSat, onUnit = { inSat = it })
            val feerate = vm.feerates[6] ?: 2L
            val q = com.fortis.wallet.lightning.quoteToLightning(amountSat, 150 * feerate, feerate, 0L)
            if (amountSat > maxSat) ErrorText(stringResource(R.string.transfer_more_than_balance))
            else if (amountSat > 0) {
                if (q == null) ErrorText(stringResource(R.string.transfer_too_small))
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

/** Lightning wallet → the first on-chain BTC wallet (spec §2): a splice-out, or a close for
 *  everything. */
@Composable
internal fun TransferToOnchainTab(vm: WalletViewModel) {
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
    GlassCard {
        Column(verticalArrangement = Arrangement.spacedBy(Fx.s3)) {
            Text(stringResource(R.string.transfer_to_onchain_title), color = Fx.text, fontWeight = FontWeight.SemiBold)
            Text(stringResource(R.string.transfer_to_onchain_help), style = FxType.meta, color = Fx.textDim)
            MoveToWalletPicker(vm)
            if (maxSat <= 0L) Text(stringResource(R.string.transfer_nothing_to_move_ln), style = FxType.meta, color = Fx.warn)
            SwapAmount(amount, { amount = it }, "btc", maxSat, hint = R.string.transfer_help_slider, inSat = inSat, onUnit = { inSat = it })
            val fromAccount = amountSat > channelSat
            // Leaving only crumbs behind is moving everything: a splice can't take the last few
            // thousand sat (its fees come out of what stays), a close takes it all.
            val all = !fromAccount && amountSat > 0 && channelSat - amountSat < MOVE_ALL_SLACK_SAT
            val q = vm.quoteMoveToOnchain(amountSat)
            if (amountSat > maxSat) ErrorText(stringResource(R.string.transfer_more_than_balance))
            else if (amountSat > 0) {
                if (q == null && !all) ErrorText(stringResource(R.string.transfer_too_small))
                else if (q != null) Text(
                    stringResource(R.string.transfer_summary_onchain, fmt(amountSat), fmt(q.feeSat), fmt(q.payoutSat)),
                    style = FxType.meta, color = Fx.textDim,
                )
                if (all) Text(stringResource(R.string.transfer_all_closes), style = FxType.meta, color = Fx.textDim)
                if (fromAccount) Text(stringResource(R.string.transfer_from_account), style = FxType.meta, color = Fx.textDim)
            }
            ErrorText(vm.error)
            PrimaryButton(
                stringResource(R.string.transfer_move),
                enabled = (q != null || all) && amountSat <= maxSat && ready != null && !vm.movingToOnchain,
                working = vm.movingToOnchain,
            ) {
                gate.require(vm, gateAct, gateScope) { vm.moveToOnchain(amountSat, all, fromAccount) }
            }
        }
    }
}

/** A move leaving less than this in the channel moves everything (closes it). */
private const val MOVE_ALL_SLACK_SAT = 3_000L

/** "To wallet": which BTC wallet the moved funds go to; a menu when there are several. */
@androidx.compose.runtime.Composable
private fun MoveToWalletPicker(vm: WalletViewModel) {
    val choices = com.fortis.wallet.lightning.lightningSweepWallets(vm.wallets)
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
