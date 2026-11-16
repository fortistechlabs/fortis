package com.fortis.wallet.ui

import com.fortis.wallet.ui.screens.kvStacked
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** A long value (an invoice description) took the whole row and squeezed its label ("Description")
 *  into one character-wide column: long values go under their label instead. */
class KvLayoutTest {
    @Test
    fun shortValuesStayOnTheLabelsRow() {
        assertFalse(kvStacked("0.00001000 XBT"))
        assertFalse(kvStacked("1,000 sat"))
        assertFalse(kvStacked(""))
    }

    @Test
    fun longValuesGoUnderTheirLabel() {
        assertTrue(kvStacked("Payment for invoice 2026-1142, thanks for the coffee"))
        assertTrue(kvStacked("x".repeat(60)))
    }
}
