package com.fortis.wallet.ui

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.widget.Toast

/** Public block-explorer URL for a transaction, or null when there isn't one
 *  (a non-mainnet network, or an unknown chain). */
fun explorerTxUrl(chain: String, network: String, txid: String): String? {
    if (network != "mainnet") return null
    val base = when (chain) {
        "btc" -> "https://mempool.space"
        "xbt" -> "https://mempool.guide"
        else -> return null
    }
    return "$base/tx/$txid"
}

fun openInBrowser(ctx: Context, url: String) {
    try {
        ctx.startActivity(
            Intent(Intent.ACTION_VIEW, Uri.parse(url)).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
        )
    } catch (_: ActivityNotFoundException) {
        Toast.makeText(ctx, "No browser to open the explorer", Toast.LENGTH_SHORT).show()
    }
}
