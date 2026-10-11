package com.fortis.wallet.ui.theme

import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class ButtonTokensTest {
    @Test
    fun buttonsAreRoundedOnALip() {
        assertEquals(10.dp, Fx.rBtn)
        assertEquals(3.dp, Fx.lip)
    }

    @Test
    fun buttonsAreFullTouchHeight() {
        assertEquals(48.dp, Fx.btnH)
        assertEquals(40.dp, Fx.btnHDense)
    }

    @Test
    fun primaryIsSignalGreenAndSecondaryIsSlate() {
        assertEquals(Color(0xFF4DFF7A), Fx.primaryTop)
        assertEquals(Color(0xFF00D936), Fx.primaryBottom)
        assertEquals(Color(0xFF3A424A), Fx.ghostTop)
        assertEquals(Color(0xFF2B3238), Fx.ghostBottom)
    }

    @Test
    fun faceIsShadedTopLighterThanBottom() {
        for ((top, bottom) in listOf(Fx.primaryTop to Fx.primaryBottom, Fx.ghostTop to Fx.ghostBottom)) {
            assertTrue("top must be lighter", luminance(top) > luminance(bottom))
            assertTrue("shading too strong", contrastRatio(top, bottom) < 1.5)
        }
    }

    @Test
    fun secondaryFaceStandsOffEveryBackground() {
        // Filled, not outlined: the slate face itself must read as an object on the page.
        for (bg in listOf(Fx.bg0, Fx.panel, Fx.panelRaised)) {
            assertTrue(contrastRatio(Fx.ghostBottom, bg) >= 1.35)
        }
    }

    @Test
    fun walletTileTextReadsOnSlate() {
        // Home wallet tiles are slate keys: their secondary text and the selected outline must read on it.
        for (face in listOf(Fx.ghostTop, Fx.ghostBottom, Fx.ghostPressed)) {
            assertTrue(contrastRatio(Fx.dimOnSlate, face) >= 4.5)
            assertTrue(contrastRatio(Fx.signal, face) >= 4.5)
        }
    }

    @Test
    fun everyLabelReadsOnEveryPartOfItsFace() {
        for (face in listOf(Fx.primaryTop, Fx.primaryBottom, Fx.primaryPressed)) {
            assertTrue(contrastRatio(Fx.onPrimary, face) >= 4.5)
        }
        for (face in listOf(Fx.ghostTop, Fx.ghostBottom, Fx.ghostPressed)) {
            for (tint in listOf(Fx.text, Fx.textDim, Fx.bad)) {
                val label = ghostLabel(tint)
                assertTrue("%s on %s".format(label, face), contrastRatio(label, face) >= 4.5)
            }
        }
    }
}
