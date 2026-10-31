package com.fortis.wallet.ui

import android.graphics.Bitmap
import android.graphics.Color
import com.google.zxing.BarcodeFormat
import com.google.zxing.EncodeHintType
import com.google.zxing.qrcode.QRCodeWriter
import com.google.zxing.qrcode.decoder.ErrorCorrectionLevel
import uniffi.wallet_ffi.LnReceiveQuote

/** Render `text` as a QR code bitmap. Black-on-white so any scanner reads it. */
fun qrBitmap(text: String, sizePx: Int = 640): Bitmap {
    val hints = mapOf(
        EncodeHintType.ERROR_CORRECTION to ErrorCorrectionLevel.M,
        EncodeHintType.MARGIN to 1,
    )
    val matrix = QRCodeWriter().encode(text, BarcodeFormat.QR_CODE, sizePx, sizePx, hints)
    val w = matrix.width
    val h = matrix.height
    val pixels = IntArray(w * h)
    for (y in 0 until h) {
        val row = y * w
        for (x in 0 until w) pixels[row + x] = if (matrix[x, y]) Color.BLACK else Color.WHITE
    }
    return Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888).apply { setPixels(pixels, 0, w, 0, 0, w, h) }
}

/** Bare address from a scanned string — tolerates a `bitcoin:` URI with params. */
fun addressFromScan(raw: String): String =
    raw.trim()
        .removePrefix("bitcoin:")
        .removePrefix("BITCOIN:")
        .substringBefore("?")
        .trim()

enum class ReceiveHint {
    AddAmountForLightning,
    BelowLightningMin,
}

data class ReceiveQr(
    val uri: String,
    val lightning: Boolean,
    val feeSat: Long?,
    val youGetSat: Long?,
    val hint: ReceiveHint?,
)

/** Converts millisatoshis to satoshis for UI display, rounding sub-satoshi
 *  amounts up so a non-zero sub-sat fee or amount never displays as 0. */
fun displaySatFromMsat(msat: ULong): Long =
    if (msat == 0uL) 0L else (((msat - 1uL) / 1_000uL) + 1uL).toLong()

fun buildReceiveQr(
    address: String,
    amountSat: Long?,
    quote: LnReceiveQuote?,
    minLnSat: Long,
    lightningEnabled: Boolean = minLnSat > 0L,
): ReceiveQr {
    if (!lightningEnabled || minLnSat <= 0L) {
        val uri = if (amountSat != null && amountSat > 0L) {
            "bitcoin:$address?amount=${fmtCoin(amountSat)}"
        } else {
            address
        }
        return ReceiveQr(uri = uri, lightning = false, feeSat = null, youGetSat = null, hint = null)
    }

    if (amountSat == null || amountSat <= 0L) {
        return ReceiveQr(
            uri = address,
            lightning = false,
            feeSat = null,
            youGetSat = null,
            hint = ReceiveHint.AddAmountForLightning,
        )
    }

    if (amountSat < minLnSat) {
        return ReceiveQr(
            uri = "bitcoin:$address?amount=${fmtCoin(amountSat)}",
            lightning = false,
            feeSat = null,
            youGetSat = null,
            hint = ReceiveHint.BelowLightningMin,
        )
    }

    if (quote == null) {
        return ReceiveQr(
            uri = "bitcoin:$address?amount=${fmtCoin(amountSat)}",
            lightning = false,
            feeSat = null,
            youGetSat = null,
            hint = null,
        )
    }

    val feeSat = displaySatFromMsat(quote.feeMsat)
    val displayTotalSat = maxOf(amountSat, displaySatFromMsat(quote.amountMsat))
    val youGetSat = (displayTotalSat - feeSat).coerceAtLeast(0L)
    return ReceiveQr(
        uri = "bitcoin:$address?amount=${fmtCoin(amountSat)}&lightning=${quote.invoice}",
        lightning = true,
        feeSat = feeSat,
        youGetSat = youGetSat,
        hint = null,
    )
}
