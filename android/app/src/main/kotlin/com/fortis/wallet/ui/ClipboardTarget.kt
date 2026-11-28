package com.fortis.wallet.ui

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import com.google.zxing.BarcodeFormat
import com.google.zxing.BinaryBitmap
import com.google.zxing.DecodeHintType
import com.google.zxing.LuminanceSource
import com.google.zxing.RGBLuminanceSource
import com.google.zxing.common.GlobalHistogramBinarizer
import com.google.zxing.common.HybridBinarizer
import com.google.zxing.qrcode.QRCodeReader

/**
 * The Send screen's Paste button: what is on the clipboard becomes the target. A text is cleaned like a scan
 * ([targetFromText]); a picture is read for a QR code ([decodeQr]). The result is only ever put in the To
 * field, where the usual checks and the review step still apply before anything is paid.
 */

/** A text from the clipboard as a Send target: the `lightning:` wrapper and any whitespace a copy picked up are
 *  dropped, the rest is left as it was. Null when nothing is left. */
fun targetFromText(raw: String?): String? {
    val unwrapped = textFromIntent(raw) ?: raw?.trim()
    return unwrapped?.filterNot(Char::isWhitespace)?.takeIf { it.isNotEmpty() }
}

/** The text of the QR code in an image of ARGB [pixels], or null when there is none. Tries dark-on-light and
 *  light-on-dark, and two ways of telling dark from light; a screenshot with a small QR in it is fine. */
fun decodeQr(width: Int, height: Int, pixels: IntArray): String? {
    if (width <= 0 || height <= 0 || pixels.size < width * height) return null
    val hints = mapOf(
        DecodeHintType.TRY_HARDER to true,
        DecodeHintType.POSSIBLE_FORMATS to listOf(BarcodeFormat.QR_CODE),
    )
    val source: LuminanceSource = RGBLuminanceSource(width, height, pixels)
    for (s in listOf(source, source.invert())) {
        for (binarize in listOf<(LuminanceSource) -> com.google.zxing.Binarizer>(::HybridBinarizer, ::GlobalHistogramBinarizer)) {
            try {
                return QRCodeReader().decode(BinaryBitmap(binarize(s)), hints).text
            } catch (_: com.google.zxing.ReaderException) {
                // not found with this reading: try the next
            }
        }
    }
    return null
}

/** What the clipboard held, for the Paste button. */
sealed interface ClipResult {
    /** A target to put in the To field. */
    data class Target(val text: String) : ClipResult
    /** Nothing usable: empty, or something that is neither text nor a picture. */
    data object Empty : ClipResult
    /** A picture, but no QR code could be read in it. */
    data object NoQr : ClipResult
}

/** The longest side a pasted picture is read at: bigger ones are shrunk first (a photo is many megapixels). */
private const val MAX_IMAGE_SIDE = 2_000

/** Reads the primary clip: a picture is read for a QR code (its text, if it has one, is the fallback), anything
 *  else is taken as text. Call off the main thread. */
fun readClipboard(ctx: Context): ClipResult {
    val cm = ctx.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
    val clip: ClipData = cm.primaryClip ?: return ClipResult.Empty
    var sawImage = false
    var text: String? = null
    for (i in 0 until clip.itemCount) {
        val item = clip.getItemAt(i)
        val uri: Uri? = item.uri
        if (uri != null && isImage(ctx, uri, clip)) {
            sawImage = true
            decodeImage(ctx, uri)?.let { return ClipResult.Target(it) }
        }
        if (text == null) text = targetFromText(item.text?.toString())
    }
    return when {
        text != null -> ClipResult.Target(text)
        sawImage -> ClipResult.NoQr
        else -> ClipResult.Empty
    }
}

private fun isImage(ctx: Context, uri: Uri, clip: ClipData): Boolean =
    clip.description.hasMimeType("image/*") || (runCatching { ctx.contentResolver.getType(uri) }.getOrNull()?.startsWith("image/") == true)

private fun decodeImage(ctx: Context, uri: Uri): String? = runCatching {
    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
    ctx.contentResolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it, null, bounds) }
    var sample = 1
    while (maxOf(bounds.outWidth, bounds.outHeight) / sample > MAX_IMAGE_SIDE) sample *= 2
    val opts = BitmapFactory.Options().apply { inSampleSize = sample; inPreferredConfig = Bitmap.Config.ARGB_8888 }
    val bmp = ctx.contentResolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it, null, opts) } ?: return@runCatching null
    val px = IntArray(bmp.width * bmp.height)
    bmp.getPixels(px, 0, bmp.width, 0, 0, bmp.width, bmp.height)
    val w = bmp.width
    val h = bmp.height
    bmp.recycle()
    decodeQr(w, h, px)?.let { targetFromText(it) }
}.getOrNull()
