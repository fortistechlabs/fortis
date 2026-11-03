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

/**
 * What a move to Lightning deposits on top of the channel size, [reserveSat] being the anchor
 * reserve `open_channel` needs left in the transit wallet. The rest is headroom for the funding
 * transaction's fee (about 200 vB, doubled for a rise before the deposit confirms); it stays in
 * the transit wallet as change.
 */
fun moveExtraSat(reserveSat: Long, feerateSatPerVb: Long): Long =
    reserveSat + (feerateSatPerVb * 400L).coerceAtLeast(1_000L)

/**
 * [extraSat] (see [moveExtraSat]) is deposited with the channel amount and must be affordable too;
 * [feeSat] is the fee of the deposit transaction itself.
 */
fun planMove(
    amountSat: Long,
    onchainSat: Long,
    channelMinSat: Long,
    feeSat: Long,
    extraSat: Long = 0L,
): MovePlan {
    if (amountSat < channelMinSat) return MovePlan.BelowMin(channelMinSat)
    if (amountSat + extraSat + feeSat > onchainSat) return MovePlan.Insufficient
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
