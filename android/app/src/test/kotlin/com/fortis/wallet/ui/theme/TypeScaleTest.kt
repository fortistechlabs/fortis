package com.fortis.wallet.ui.theme

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class TypeScaleTest {
    private val values = mapOf(
        "balance" to FxType.balance, "fiat" to FxType.fiat,
        "amount" to FxType.amount, "fiatSmall" to FxType.fiatSmall,
    )

    @Test
    fun valueStylesAreAtLeast13sp() {
        for ((name, s) in values) assertTrue("$name is ${s.fontSize}", s.fontSize.value >= 13f)
    }

    @Test
    fun valueStylesAreMonoAndTabular() {
        for ((name, s) in values) {
            assertEquals(name, Mono, s.fontFamily)
            assertEquals(name, "tnum", s.fontFeatureSettings)
        }
    }

    @Test
    fun valueStylesNeverFaint() {
        for ((name, s) in values) assertNotEquals(name, Fx.textFaint, s.color)
    }

    @Test
    fun recoveryPhraseGridIs13spMonoSoTwelveCharacterCellsFit() {
        assertEquals(13f, FxType.phrase.fontSize.value)
        assertEquals(Mono, FxType.phrase.fontFamily)
        assertEquals(Fx.text, FxType.phrase.color)
    }

    @Test
    fun everyStyleIsJetBrainsMono() {
        val all = mapOf(
            "balance" to FxType.balance, "fiat" to FxType.fiat, "amount" to FxType.amount,
            "fiatSmall" to FxType.fiatSmall, "phrase" to FxType.phrase, "title" to FxType.title,
            "body" to FxType.body, "meta" to FxType.meta, "label" to FxType.label,
            "nav" to FxType.nav, "button" to FxType.button, "brand" to FxType.brand,
        )
        for ((name, st) in all) assertEquals(name, Mono, st.fontFamily)
        with(FortisType) {
            for (st in listOf(displayLarge, displayMedium, displaySmall, headlineLarge, headlineMedium, headlineSmall,
                titleLarge, titleMedium, titleSmall, bodyLarge, bodyMedium, bodySmall, labelLarge, labelMedium, labelSmall)) {
                assertEquals(Mono, st.fontFamily)
            }
        }
    }

    @Test
    fun pinnedSizes() {
        assertEquals(36f, FxType.balance.fontSize.value)
        assertEquals(17f, FxType.fiat.fontSize.value)
        assertEquals(15f, FxType.amount.fontSize.value)
        assertEquals(13f, FxType.fiatSmall.fontSize.value)
        assertEquals(13f, FxType.meta.fontSize.value)
        assertEquals(11f, FxType.label.fontSize.value)
        assertEquals(12f, FxType.button.fontSize.value)
    }
}
