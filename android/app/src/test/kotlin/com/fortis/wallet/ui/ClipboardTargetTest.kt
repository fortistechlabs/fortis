package com.fortis.wallet.ui

import com.google.zxing.BarcodeFormat
import com.google.zxing.EncodeHintType
import com.google.zxing.qrcode.QRCodeWriter
import com.google.zxing.qrcode.decoder.ErrorCorrectionLevel
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** The Send screen's Paste button: what is on the clipboard (a text, or a picture of a QR code) becomes the target. */
class ClipboardTargetTest {
    private val lnurl = "LNURL1DP68GURN8GHJ7UM9WFMXJCM99E3K7MF0V9CXJ0M385EKVCENXC6R2V3C8QUNQDCCAG7"
    private val offer = "lno1pg257enxv4ezqcneype82um50ynhxgrwdajx283qfwdpl28qqmc78ymlvhmxcsywdk5wrjnj36jryg488qwlrnzyjczlqs85ck65ycmkdv9xr3j6uc9f3ymqrd2rfg2cmukf0x9yxjzl5ksqx23e6s5spzvgm6hu2f5mpq4nq3gntcm3gl28x6mm8acz3nvfxhyv5fvsmlgd7jzvfvhk8cpx4f8nlx5mkq3wnpcxgcehmggvvd38xa3c2vmmpdtqfhkcvdn9rj0a6k0xsl32wq3wkmmhgv0g5wag7x4fzz83xpz7l3v6h6p3"

    /** The QR of [text] as ARGB pixels, [scale] pixels per module, [border] white pixels around it. */
    private fun qrPixels(text: String, scale: Int = 4, border: Int = 0, invert: Boolean = false): Triple<Int, Int, IntArray> {
        val hints = mapOf(EncodeHintType.MARGIN to 4, EncodeHintType.ERROR_CORRECTION to ErrorCorrectionLevel.L)
        val m = QRCodeWriter().encode(text, BarcodeFormat.QR_CODE, 0, 0, hints)
        val side = m.width * scale + 2 * border
        val dark = if (invert) 0xFFFFFFFF.toInt() else 0xFF000000.toInt()
        val light = if (invert) 0xFF000000.toInt() else 0xFFFFFFFF.toInt()
        val px = IntArray(side * side) { light }
        for (y in 0 until m.height) for (x in 0 until m.width) if (m.get(x, y)) {
            for (dy in 0 until scale) for (dx in 0 until scale) px[(border + y * scale + dy) * side + border + x * scale + dx] = dark
        }
        return Triple(side, side, px)
    }

    @Test
    fun readsAnLnurlAndAnOfferFromAPictureOfTheirQr() {
        for (text in listOf(lnurl, offer)) {
            val (w, h, px) = qrPixels(text)
            assertEquals(text, decodeQr(w, h, px))
        }
    }

    @Test
    fun readsAQrThatIsSmallInsideALargeScreenshot() {
        val (w, h, px) = qrPixels(lnurl, scale = 3, border = 400)
        assertEquals(lnurl, decodeQr(w, h, px))
    }

    @Test
    fun readsALightOnDarkQr() {
        val (w, h, px) = qrPixels(lnurl, invert = true)
        assertEquals(lnurl, decodeQr(w, h, px))
    }

    @Test
    fun aPictureWithoutAQrIsNothingNotAGuess() {
        assertNull(decodeQr(200, 200, IntArray(200 * 200) { 0xFFFFFFFF.toInt() }))
        assertNull(decodeQr(200, 200, IntArray(200 * 200) { if ((it * 7919) % 5 == 0) 0xFF000000.toInt() else 0xFFFFFFFF.toInt() }))
        assertNull(decodeQr(0, 0, IntArray(0)))
    }

    @Test
    fun clipboardTextBecomesATargetWithoutTheWrapping() {
        // The scheme and any whitespace a copy picked up go; the payload stays as it was.
        assertEquals(lnurl, targetFromText("lightning:$lnurl"))
        assertEquals(lnurl, targetFromText("  $lnurl \n"))
        assertEquals("lno1abcdef", targetFromText("lno1 abc\ndef"))
        assertEquals("name@fortis.cash", targetFromText("name@fortis.cash"))
        assertNull(targetFromText("   \n"))
        assertNull(targetFromText(null))
    }
    @Test
    fun aBigPictureIsShrunkByWholeStepsToFitAndASmallOneIsLeftAlone() {
        assertEquals(1, sampleSizeFor(1000, 800))
        assertEquals(1, sampleSizeFor(2000, 2000))
        assertEquals(2, sampleSizeFor(2001, 100))
        assertEquals(2, sampleSizeFor(4000, 3000))
        assertEquals(4, sampleSizeFor(8000, 6000))
        // A picture whose size could not be read is not shrunk (decoding it then simply finds nothing).
        assertEquals(1, sampleSizeFor(0, 0))
        assertEquals(1, sampleSizeFor(-1, -1))
    }
}
