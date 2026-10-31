package com.fortis.wallet.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.KeyboardType
import com.fortis.wallet.R
import com.fortis.wallet.WalletViewModel
import com.fortis.wallet.data.HistoryEntry
import com.fortis.wallet.homeBalance
import com.fortis.wallet.lightning.LnState
import com.fortis.wallet.ui.Field
import com.fortis.wallet.ui.displaySatFromMsat
import com.fortis.wallet.ui.theme.Fx
import com.fortis.wallet.ui.theme.FxType
import com.fortis.wallet.ui.theme.caps
import uniffi.wallet_ffi.LnDirection
import uniffi.wallet_ffi.LnPayment
import uniffi.wallet_ffi.LnPaymentStatus

typealias TxRow = HistoryEntry

sealed interface MovePlan {
    data object Ok : MovePlan
    data class BelowMin(val min: Long) : MovePlan
    data object Insufficient : MovePlan
}

fun planMove(
    amountSat: Long,
    onchainSat: Long,
    channelMinSat: Long,
    feeSat: Long,
): MovePlan {
    if (amountSat < channelMinSat) return MovePlan.BelowMin(channelMinSat)
    if (amountSat + feeSat > onchainSat) return MovePlan.Insufficient
    return MovePlan.Ok
}

data class HistoryRow(
    val id: String,
    val lightning: Boolean,
    val send: Boolean,
    val amountSat: Long,
    val feeSat: Long,
    val description: String,
    val time: Long,
    val confirmations: Long = 0L,
    val internal: Boolean = false,
    val pending: Boolean = false,
    val failed: Boolean = false,
)

fun mergeHistory(onchain: List<TxRow>, ln: List<LnPayment>): List<HistoryRow> {
    val rows = ArrayList<HistoryRow>(onchain.size + ln.size)
    for (h in onchain) {
        rows += HistoryRow(
            id = h.txid,
            lightning = false,
            send = h.send,
            amountSat = h.amountSat,
            feeSat = h.feeSat,
            description = "",
            time = h.time,
            confirmations = h.confirmations,
            internal = h.internal,
            pending = h.confirmations < 1L,
            failed = false,
        )
    }
    for (p in ln) {
        val isSend = p.direction == LnDirection.OUT
        val rawSat = displaySatFromMsat(p.amountMsat)
        val signedSat = if (isSend) -rawSat else rawSat
        val feeSat = displaySatFromMsat(p.feeMsat)
        rows += HistoryRow(
            id = p.id,
            lightning = true,
            send = isSend,
            amountSat = signedSat,
            feeSat = feeSat,
            description = p.description,
            time = p.timestamp.toLong(),
            confirmations = if (p.status == LnPaymentStatus.SUCCEEDED) 6L else 0L,
            internal = false,
            pending = p.status == LnPaymentStatus.PENDING,
            failed = p.status == LnPaymentStatus.FAILED,
        )
    }
    return rows.sortedByDescending { it.time }
}

fun deleteWalletLightningWarningSat(lightningSat: Long): Long? =
    lightningSat.takeIf { it > 0L }

fun deleteWalletLightningWarningSat(
    ln: LnState,
    lastKnownLightningSat: Long = 0L,
): Long? = homeBalance(0L, ln, lastKnownLightningSat).lightningSat.takeIf { it > 0L }

@Composable
fun MoveToLightningDialog(
    vm: WalletViewModel,
    initialShortfallSat: Long? = null,
    onDismiss: () -> Unit,
) {
    val channelMinSat = vm.lightning.lastInfo?.channelMinSat ?: 100_000L
    val defaultSat = initialShortfallSat?.let { maxOf(it, channelMinSat) } ?: channelMinSat
    var amount by remember(defaultSat) { mutableStateOf(defaultSat.toString()) }
    var amountInSat by remember { mutableStateOf(true) }
    val amountSat = amountToSat(amount, amountInSat)
    val onchainSat = vm.balances?.confirmedSat ?: 0L
    val estFeeSat = (vm.feerates[6] ?: 2L) * 150L
    val plan = amountSat?.let { planMove(it, onchainSat, channelMinSat, estFeeSat) }

    AlertDialog(
        onDismissRequest = onDismiss,
        containerColor = Fx.panel,
        title = { Text(stringResource(R.string.move_to_ln_title), color = Fx.text) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(Fx.s3)) {
                Text(
                    stringResource(R.string.move_to_ln_body, "%,d".format(channelMinSat)),
                    style = FxType.meta,
                    color = Fx.textDim,
                )
                Text(caps(stringResource(R.string.field_amount)), style = FxType.label)
                Row(
                    horizontalArrangement = Arrangement.spacedBy(Fx.s2),
                    verticalAlignment = Alignment.Top,
                ) {
                    Field(
                        amount,
                        { amount = it },
                        null,
                        keyboardType = if (amountInSat) KeyboardType.Number else KeyboardType.Decimal,
                        modifier = Modifier.weight(1f),
                    )
                    AmountUnitPicker(amountInSat, "BTC") { amountInSat = it }
                }
                when (plan) {
                    is MovePlan.BelowMin -> Text(
                        stringResource(R.string.move_to_ln_below_min, "%,d".format(plan.min)),
                        style = FxType.meta,
                        color = Fx.warn,
                    )
                    MovePlan.Insufficient -> Text(
                        stringResource(R.string.move_to_ln_insufficient),
                        style = FxType.meta,
                        color = Fx.warn,
                    )
                    MovePlan.Ok, null -> {}
                }
            }
        },
        confirmButton = {
            TextButton(
                onClick = {
                    val sat = amountSat ?: return@TextButton
                    onDismiss()
                    vm.moveToLightning(sat)
                },
                enabled = plan == MovePlan.Ok,
            ) {
                Text(stringResource(R.string.action_review), color = if (plan == MovePlan.Ok) Fx.signal else Fx.textDim)
            }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) {
                Text(stringResource(R.string.action_cancel), color = Fx.text)
            }
        },
    )
}
