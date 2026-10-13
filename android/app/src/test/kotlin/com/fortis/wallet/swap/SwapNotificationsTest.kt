package com.fortis.wallet.swap

import com.fortis.wallet.data.Step
import com.fortis.wallet.data.SwapRecord
import org.junit.Assert.assertEquals
import org.junit.Test

class SwapNotificationsTest {
    private fun rec(step: Step) = SwapRecord(
        id = "offer-1", fromWalletId = "w1", toWalletId = "w2", sendChain = "xbt",
        amountSat = 1_000_000, limitPrice = 1_120_000, ownerSecret = "s", swapIndex = 7,
        wrappedXbt = "", wrappedBtc = "", redeemAddress = "bc1q", refundAddress = "x1q", step = step,
    )

    @Test
    fun progress_text_counts_the_five_steps() {
        assertEquals("Swap in progress · step 1 of 5", SwapNotifications.progressText(rec(Step.MATCHED)))
        assertEquals("Swap in progress · step 2 of 5", SwapNotifications.progressText(rec(Step.FUNDED)))
        assertEquals("Swap in progress · step 3 of 5", SwapNotifications.progressText(rec(Step.THEY_FUNDED)))
        assertEquals("Swap in progress · step 4 of 5", SwapNotifications.progressText(rec(Step.REDEEMED)))
        assertEquals("Swap in progress · step 5 of 5", SwapNotifications.progressText(rec(Step.DONE)))
    }

    @Test
    fun progress_text_for_the_steps_between() {
        // Matched, waiting for our funding: still step 1.
        assertEquals("Swap in progress · step 1 of 5", SwapNotifications.progressText(rec(Step.NEED_FUNDING)))
        assertEquals("Swap in progress · waiting for a match", SwapNotifications.progressText(rec(Step.WAITING_MATCH)))
        assertEquals("Swap in progress · refunding", SwapNotifications.progressText(rec(Step.REFUNDING)))
    }

    @Test
    fun deadline_format() {
        assertEquals("1h 52m", SwapNotifications.formatDeadline(6720))
        assertEquals("<1m", SwapNotifications.formatDeadline(59))
        assertEquals("<1m", SwapNotifications.formatDeadline(0))
        assertEquals("<1m", SwapNotifications.formatDeadline(-30))
        assertEquals("1m", SwapNotifications.formatDeadline(60))
        assertEquals("59m", SwapNotifications.formatDeadline(3599))
        assertEquals("2h 0m", SwapNotifications.formatDeadline(7200))
    }

    /** The English strings the notifications use, read from res/values/strings.xml. */
    private fun string(name: String): String {
        val doc = javax.xml.parsers.DocumentBuilderFactory.newInstance().newDocumentBuilder()
            .parse(java.io.File("src/main/res/values/strings.xml"))
        val nodes = doc.getElementsByTagName("string")
        for (i in 0 until nodes.length) {
            val n = nodes.item(i) as org.w3c.dom.Element
            if (n.getAttribute("name") == name) return n.textContent.replace("\\'", "'")
        }
        error("no string $name")
    }

    @Test
    fun need_funding_text() {
        assertEquals(
            "Your turn: open to fund your swap · deadline 1h 52m",
            string("swap_event_need_funding").format(SwapNotifications.formatDeadline(6720)),
        )
    }

    @Test
    fun need_funding_text_without_a_deadline_omits_the_clause() {
        val with = string("swap_event_need_funding")
        val plain = string("swap_event_need_funding_plain")
        assertEquals("Your turn: open to fund your swap", SwapNotifications.needFundingText(with, plain, null, 1_000L))
        assertEquals(
            "Your turn: open to fund your swap · deadline 1h 52m",
            SwapNotifications.needFundingText(with, plain, 1_000L + 6720, 1_000L),
        )
    }
}
