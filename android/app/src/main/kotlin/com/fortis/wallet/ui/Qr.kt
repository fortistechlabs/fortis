package com.fortis.wallet.ui

import android.graphics.Bitmap
import android.graphics.Color
import com.google.zxing.BarcodeFormat
import com.google.zxing.EncodeHintType
import com.google.zxing.qrcode.QRCodeWriter
import com.google.zxing.qrcode.decoder.ErrorCorrectionLevel

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
