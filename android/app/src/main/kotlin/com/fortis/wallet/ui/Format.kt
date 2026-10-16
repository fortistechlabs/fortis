package com.fortis.wallet.ui

import java.util.Locale

/** A coin amount (BTC / XBT): always 8 dp with a `.` decimal — the universal
 *  convention for on-chain values, independent of the device locale. */
fun fmtCoin(sat: Long): String = "%.8f".format(Locale.ROOT, sat / 1e8)

/** A formatted coin amount split before its decimal point — `"20999999"` and
 *  `".99999999"` — so a figure too wide for its line wraps at the point rather
 *  than mid-number. No point → all whole part. */
fun splitAmount(formatted: String): Pair<String, String> {
    val i = formatted.indexOf('.')
    return if (i < 0) formatted to "" else formatted.substring(0, i) to formatted.substring(i)
}
