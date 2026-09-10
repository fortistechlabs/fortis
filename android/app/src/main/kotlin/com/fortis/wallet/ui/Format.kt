package com.fortis.wallet.ui

import java.text.NumberFormat
import java.util.Currency
import java.util.Locale

/** A coin amount (BTC / XBT): always 8 dp with a `.` decimal — the universal
 *  convention for on-chain values, independent of the device locale. */
fun fmtCoin(sat: Long): String = "%.8f".format(Locale.ROOT, sat / 1e8)

private val USD = Currency.getInstance("USD")

/** An approximate USD value — grouped and punctuated per the device locale but
 *  always in dollars. "$12.34" / "1.234,56 $" / "<$0.01". */
fun fmtUsd(v: Double): String {
    val nf = NumberFormat.getCurrencyInstance().apply { currency = USD }
    return if (v > 0.0 && v < 0.01) "<" + nf.format(0.01) else nf.format(v)
}
