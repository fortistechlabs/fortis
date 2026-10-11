package com.fortis.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Test

class AmountSplitTest {
    @Test
    fun splitsAtTheDecimalPointSoAWrapNeverBreaksMidNumber() {
        assertEquals("20999999" to ".99999999", splitAmount("20999999.99999999"))
        assertEquals("0" to ".12345678", splitAmount("0.12345678"))
    }

    @Test
    fun keepsTheSignWithTheWholePart() {
        assertEquals("-12" to ".34567890", splitAmount("-12.34567890"))
    }

    @Test
    fun aFigureWithoutAPointIsAllWhole() {
        assertEquals("—" to "", splitAmount("—"))
    }
}
