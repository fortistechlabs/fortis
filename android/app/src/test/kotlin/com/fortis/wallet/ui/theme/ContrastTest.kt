package com.fortis.wallet.ui.theme

import androidx.compose.ui.graphics.Color
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class ContrastTest {
    private fun hex(c: Color) = "#%08X".format(c.value.shr(32).toLong())

    @Test
    fun blackOnWhiteIs21() {
        assertEquals(21.0, contrastRatio(Color.Black, Color.White), 0.01)
    }

    @Test
    fun sameColourIs1() {
        assertEquals(1.0, contrastRatio(Fx.text, Fx.text), 0.001)
    }

    @Test
    fun textTokensMeetAA() {
        val texts = mapOf(
            "text" to Fx.text, "textDim" to Fx.textDim, "textFaint" to Fx.textFaint,
            "signal" to Fx.signal, "warn" to Fx.warn, "bad" to Fx.bad,
        )
        for ((bgName, bg) in mapOf("bg0" to Fx.bg0, "panel" to Fx.panel)) {
            for ((name, fg) in texts) {
                val r = contrastRatio(fg, bg)
                assertTrue("$name ${hex(fg)} on $bgName ${hex(bg)} is %.2f:1, needs 4.5".format(r), r >= 4.5)
            }
        }
    }

    @Test
    fun controlBorderMeets3() {
        for (bg in listOf(Fx.bg0, Fx.panel)) {
            val r = contrastRatio(Fx.control, bg)
            assertTrue("control on ${hex(bg)} is %.2f:1, needs 3".format(r), r >= 3.0)
        }
    }

    @Test
    fun primaryButtonTextMeetsAA() {
        assertTrue(contrastRatio(Fx.onPrimary, Fx.primary) >= 4.5)
    }

    @Test
    fun pinnedValues() {
        assertEquals(Color(0xFFE9ECEF), Fx.text)
        assertEquals(Color(0xFF0B0C0D), Fx.bg0)
        assertEquals(Color(0xFF111315), Fx.panel)
        assertEquals(Color(0xFF00FF41), Fx.signal)
    }
}
