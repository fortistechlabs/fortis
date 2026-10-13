package com.fortis.wallet.ui

import com.fortis.wallet.ui.screens.PostGate
import com.fortis.wallet.ui.screens.fmtPrice
import com.fortis.wallet.ui.screens.postGate
import com.fortis.wallet.ui.screens.prefillPrice
import org.junit.Assert.assertNull
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class NewSwapPriceTest {
    @Test
    fun postGateCases() {
        assertEquals(PostGate.Invalid, postGate("", 412_000, "xbt"))
        assertEquals(PostGate.Invalid, postGate("0", 412_000, "xbt"))
        assertEquals(PostGate.Invalid, postGate("1,234567891", 412_000, "xbt"))
        assertEquals(PostGate.Ready(412_000), postGate("0.00412", 412_000, "xbt"))
        // better than the reference for the user: no prompt
        assertEquals(PostGate.Ready(450_000), postGate("0,0045", 412_000, "xbt"))
        val worse = postGate("0.0038", 412_000, "xbt") as PostGate.Confirm
        assertEquals(380_000L, worse.price)
        assertEquals(7.77, worse.worsePct!!, 0.01)
        assertEquals(PostGate.Confirm(380_000, null), postGate("0.0038", null, "xbt"))
        assertTrue(postGate("0.0038", null, "btc") is PostGate.Confirm)
    }

    @Test
    fun prefillNeverOverwritesUserInput() {
        assertEquals(fmtPrice(412_000), prefillPrice("", false, 412_000))
        assertNull(prefillPrice("0.003", false, 412_000))
        assertNull(prefillPrice("", true, 412_000))
        assertNull(prefillPrice("", false, null))
    }
}
