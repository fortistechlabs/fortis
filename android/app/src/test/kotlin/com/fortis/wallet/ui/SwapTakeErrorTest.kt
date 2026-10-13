package com.fortis.wallet.ui

import com.fortis.wallet.R
import com.fortis.wallet.data.SwapHttpException
import com.fortis.wallet.ui.screens.takeError
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import java.io.IOException

class SwapTakeErrorTest {
    private fun http(code: Int, msg: String) = SwapHttpException(code, "POST /swap/offers/o1/take: $msg")

    @Test
    fun take_errors_map_to_plain_words() {
        assertEquals(R.string.swap_take_gone, takeError(http(409, "offer is no longer available")))
        assertEquals(R.string.swap_take_gone, takeError(http(404, "no such offer")))
        assertEquals(R.string.swap_take_price_changed, takeError(http(409, "price changed")))
        assertEquals(R.string.swap_take_struck, takeError(http(429, "too many missed funding deadlines")))
        assertEquals(R.string.swap_take_network, takeError(IOException("timeout")))
    }

    @Test
    fun amount_refusals_keep_the_edges_own_words() {
        // 400: below the minimum, more than the offer holds — shown as the edge says.
        assertNull(takeError(http(400, "more than the offer holds")))
        assertNull(takeError(IllegalStateException("something else")))
    }
}
