package com.fortis.wallet.ui

import com.fortis.wallet.lightning.LnState
import com.fortis.wallet.ui.screens.MovePlan
import com.fortis.wallet.ui.screens.moveExtraSat
import com.fortis.wallet.ui.screens.planMove
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.wallet_ffi.LnBalances
import uniffi.wallet_ffi.LnChannelState
import uniffi.wallet_ffi.LnLspLimits

/** Regressions: sends are not bounded by the LSP's receive limits, and are judged against what can really be sent. */
class SendBoundsFixesTest {
    private val bolt11NoAmt =
        "lnbc1pvjluezsp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygspp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdpl2pkx2ctnv5sxxmmwwd5kgetjypeh2ursdae8g6twvus8g6rfwvs8qun0dfjkxaq9qrsgq357wnc5r2ueh7ck6q93dj32dlqnls087fxdwk8qakdyafkq3yap2r09nt4ndd0unm3z9u5t48y6ucv4r5sg7lk98c77ctvjczkspk5qprc90gx"
    private val bolt11 =
        "lnbc25m1pvjluezpp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdq5vdhkven9v5sxyetpdeessp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygs9q5sqqqqqqqqqqqqqqqpqsq67gye39hfg3zd8rgc80k32tvy9xk2xunwm5lzexnvpx6fd77en8qaq424dxgt56cag2dpt359k3ssyhetktkpqh24jqnjyw6uqd08sgptq44qu"
    private val now = 1_496_314_668uL

    // The LSP takes receives of 51 000–250 000 sat only.
    private val limits = LnLspLimits(
        minPaymentMsat = 51_000_000uL,
        maxPaymentMsat = 250_000_000uL,
        channelMinSat = 100_000uL,
        channelMaxSat = 500_000uL,
    )

    private fun ready(lightningSat: ULong): LnState = LnState.Ready(
        LnBalances(lightningSat, 0uL, 0uL, 0uL, 0uL, 500_000uL),
        LnChannelState.READY,
    )

    private fun plan(amountSat: Long, balanceSat: ULong, sendableMsat: ULong? = null) = planSend(
        text = bolt11NoAmt,
        onchainSat = 0L,
        ln = ready(balanceSat),
        network = "bitcoin",
        nowSecs = now,
        limits = limits,
        userAmountMsat = amountSat.toULong() * 1_000uL,
        sendableMsat = sendableMsat,
    )

    @Test
    fun a_small_send_is_not_held_to_the_lsps_receive_minimum() {
        val p = plan(5_000L, 100_000uL)
        assertTrue("got $p", p is SendPlan.Lightning)
    }

    @Test
    fun a_large_send_is_not_held_to_the_lsps_receive_maximum() {
        val p = plan(300_000L, 400_000uL)
        assertTrue("got $p", p is SendPlan.Lightning)
    }

    @Test
    fun a_send_above_what_can_be_sent_is_a_shortfall_not_out_of_range() {
        val p = plan(500_000L, 100_000uL)
        assertEquals(SendPlan.NeedsMove(400_000L), p)
    }

    @Test
    fun the_outbound_limit_below_the_balance_decides() {
        // 100 000 sat claimable, but only 95 000 can be sent after the channel reserve.
        val p = plan(98_000L, 100_000uL, sendableMsat = 95_000_000uL)
        assertEquals(SendPlan.NeedsMove(3_000L), p)
        val ok = plan(90_000L, 100_000uL, sendableMsat = 95_000_000uL)
        assertTrue("got $ok", ok is SendPlan.Lightning)
    }

    @Test
    fun a_fixed_amount_invoice_is_judged_by_the_outbound_limit_too() {
        val p = planSend(
            text = bolt11, // 2 500 000 sat
            onchainSat = 0L,
            ln = ready(3_000_000uL),
            network = "bitcoin",
            nowSecs = now,
            limits = limits,
            sendableMsat = 2_400_000_000uL,
        )
        assertEquals(SendPlan.NeedsMove(100_000L), p)
    }

    @Test
    fun the_move_deposit_carries_the_anchor_reserve_and_fee_headroom() {
        // 25 000 sat reserve + 2 sat/vB * 400 vB, but never less than 1 000 sat of headroom.
        assertEquals(25_000L + 1_000L, moveExtraSat(25_000L, 1L))
        assertEquals(25_000L + 4_000L, moveExtraSat(25_000L, 10L))
    }

    @Test
    fun a_move_that_cannot_also_pay_the_reserve_is_insufficient() {
        val extra = moveExtraSat(25_000L, 2L)
        // 100 000 + extra + deposit fee fits exactly...
        assertEquals(MovePlan.Ok, planMove(100_000L, 100_000L + extra + 300L, 100_000L, 300L, extra))
        // ...and one sat less does not.
        assertEquals(MovePlan.Insufficient, planMove(100_000L, 100_000L + extra + 299L, 100_000L, 300L, extra))
    }
}
