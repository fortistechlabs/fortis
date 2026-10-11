package com.fortis.wallet.ui.theme

import androidx.compose.ui.graphics.Color

/** WCAG 2.x contrast ratio of [fg] on [bg] (1.0 … 21.0). Alpha is ignored. */
fun contrastRatio(fg: Color, bg: Color): Double {
    val a = luminance(fg)
    val b = luminance(bg)
    return (maxOf(a, b) + 0.05) / (minOf(a, b) + 0.05)
}

/** WCAG relative luminance of an sRGB colour. */
internal fun luminance(c: Color): Double {
    fun lin(v: Float): Double = if (v <= 0.03928f) v / 12.92 else Math.pow((v + 0.055) / 1.055, 2.4)
    return 0.2126 * lin(c.red) + 0.7152 * lin(c.green) + 0.0722 * lin(c.blue)
}
