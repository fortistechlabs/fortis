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
    /** A Lightning payment received or sent by the Fortis account (held by Fortis), not the channel. */
    val viaFortis: Boolean = false,
    /** Extra note for the row (a string resource), or 0. */
    val noteRes: Int = 0,
    /** In a ledger (the Lightning wallet): what this row changed the wallet's balance by, fees
     *  included (the amount shown is this); null where there is no ledger. */
    val netSat: Long? = null,
    /** A move on-chain: what arrived in the destination wallet, once known. */
    val arrivedSat: Long? = null,
    /** An on-chain receive that a move from the Lightning wallet delivered. */
    val fromLightning: Boolean = false,
    /** Of [feeSat]: the transfer fee paid to Fortis (the rest is the network fee). */
    val transferFeeSat: Long = 0L,
) {
    /** A real payment or transaction id (worth showing and copying), not one made up here to
     *  tell rows apart (a move on-chain, an account entry without a payment). */
    val hasRealId: Boolean get() = !id.startsWith(LOCAL_ID_MOVE) && !id.startsWith(LOCAL_ID_FORTIS)
}

/** Row ids made up by the app, never shown. */
const val LOCAL_ID_MOVE = "move-"
const val LOCAL_ID_FORTIS = "fortis-"

/**
 * The balance after each row of a ledger (rows newest first), worked back from [currentSat],
 * and what is left over before the oldest row. A complete ledger starts from 0, so anything
 * else is money the rows don't account for: shown, never hidden.
 */
fun ledgerBalances(rows: List<HistoryRow>, currentSat: Long): Pair<Map<String, Long>, Long> {
    val after = HashMap<String, Long>()
    var bal = currentSat
    for (r in rows) {
        val net = r.netSat ?: continue
        after[r.id] = bal
        bal -= net
    }
    return after to bal
}

/**
 * The Lightning wallet's whole history in time order: its channel's payments and the
 * Fortis account's, as one list. Moving the account's balance into the channel is one
 * wallet's money changing place, not a payment: neither side of it is listed (only a
 * fee it cost). Moves recorded before the LSP stored their payment are matched by
 * amount and time.
 */
fun mergeLightningHistory(
    ln: List<LnPayment>,
    account: List<com.fortis.wallet.lightning.AccountEntry>,
    moves: List<com.fortis.wallet.lightning.LnMove> = emptyList(),
): List<HistoryRow> {
    val folded = moves.mapNotNull { it.feePaymentId }.toSet()
    val moveRows = moves.map { m ->
        val into = m.kind == "in"
        HistoryRow(
            id = "$LOCAL_ID_MOVE${m.atSecs}",
            lightning = true,
            send = !into,
            amountSat = if (into) m.amountSat else -m.amountSat,
            feeSat = m.feeSat,
            description = m.toWallet,
            time = m.atSecs,
            confirmations = 6L,
            noteRes = when (m.kind) {
                "in" -> com.fortis.wallet.R.string.history_moved_in
                "close" -> com.fortis.wallet.R.string.history_moved_onchain_closed
                else -> com.fortis.wallet.R.string.history_moved_onchain
            },
            netSat = if (into) m.amountSat else -m.amountSat,
            arrivedSat = if (!into && m.txid != null) m.payoutSat else null,
            transferFeeSat = ln.firstOrNull { it.id == m.feePaymentId }?.let { displaySatFromMsat(it.amountMsat) } ?: 0L,
        )
    }
    return (mergeLightningHistoryInner(ln.filterNot { it.id in folded }, account) + moveRows).sortedByDescending { it.time }
}

private fun mergeLightningHistoryInner(
    ln: List<LnPayment>,
    account: List<com.fortis.wallet.lightning.AccountEntry>,
): List<HistoryRow> {
    val moves = account.filter { it.kind == "converted" }
    val movedIds = moves.mapNotNull { it.paymentHash }.toSet()
    val unmatched = moves.filter { it.paymentHash == null }.toMutableList()
    val channel = mergeHistory(emptyList(), ln).map { r ->
        // A send's fee leaves the wallet too: the amount shown is the whole of it.
        val net = when {
            r.failed -> 0L
            r.send -> r.amountSat - r.feeSat
            else -> r.amountSat
        }
        r.copy(amountSat = if (r.send && !r.failed) net else r.amountSat, netSat = net)
    }.filterNot { row ->
        if (row.id in movedIds) return@filterNot true
        if (row.send) return@filterNot false
        val m = unmatched.firstOrNull { c ->
            -c.amountMsat / 1_000 == row.amountSat && kotlin.math.abs(c.atSecs - row.time) <= MOVE_MATCH_SECS
        }
        if (m != null) {
            unmatched.remove(m)
            true
        } else {
            false
        }
    }
    val fortis = account.mapIndexedNotNull { i, e ->
        val id = e.paymentHash ?: "$LOCAL_ID_FORTIS${e.atSecs}-$i"
        when (e.kind) {
            "received", "sent", "withdrawn", "internal" -> HistoryRow(
                id = id,
                lightning = true,
                send = e.amountMsat < 0,
                amountSat = e.amountMsat / 1_000,
                feeSat = e.feeMsat / 1_000,
                description = "",
                time = e.atSecs,
                confirmations = 6L,
                viaFortis = true,
                noteRes = if (e.kind == "withdrawn") com.fortis.wallet.R.string.ln_account_withdrawn else 0,
                netSat = e.amountMsat / 1_000,
            )
            // Only what the move cost is a payment: the fee for the channel it opened.
            "converted" -> if (e.feeMsat >= 1_000) HistoryRow(
                id = id,
                lightning = true,
                send = true,
                amountSat = -(e.feeMsat / 1_000),
                feeSat = 0L,
                description = "",
                time = e.atSecs,
                confirmations = 6L,
                viaFortis = true,
                noteRes = com.fortis.wallet.R.string.history_move_fee,
                netSat = -(e.feeMsat / 1_000),
            ) else null
            else -> null
        }
    }
    return (channel + fortis).sortedByDescending { it.time }
}

/** How far apart (s) a move's ledger entry and its channel payment may be, to be paired. */
private const val MOVE_MATCH_SECS = 120L

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
