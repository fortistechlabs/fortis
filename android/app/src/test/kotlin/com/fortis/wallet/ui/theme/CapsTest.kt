package com.fortis.wallet.ui.theme

import org.junit.Assert.assertEquals
import org.junit.Test
import java.util.Locale

class CapsTest {
    @Test
    fun turkishKeepsTheDottedCapitalI() {
        assertEquals("GEÇMİŞ", caps("Geçmiş", Locale.forLanguageTag("tr")))
    }

    @Test
    fun azerbaijaniKeepsTheDottedCapitalI() {
        assertEquals("SƏHİFƏ", caps("səhifə", Locale.forLanguageTag("az")))
    }

    @Test
    fun englishIsPlainUppercase() {
        assertEquals("RECEIVE", caps("Receive", Locale.ENGLISH))
    }
}
