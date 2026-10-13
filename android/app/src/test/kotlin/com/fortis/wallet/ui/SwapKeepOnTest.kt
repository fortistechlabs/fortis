package com.fortis.wallet.ui

import com.fortis.wallet.R
import com.fortis.wallet.data.Step
import com.fortis.wallet.data.SwapRecord
import com.fortis.wallet.ui.screens.CancelNote
import com.fortis.wallet.ui.screens.KeepOn
import com.fortis.wallet.ui.screens.SwapTone
import com.fortis.wallet.ui.screens.cancelledNote
import com.fortis.wallet.ui.screens.swapStatusLabel
import com.fortis.wallet.ui.screens.swapTone
import com.fortis.wallet.ui.screens.keepOnBeforeStart
import com.fortis.wallet.ui.screens.keepOnNote
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class SwapKeepOnTest {
    private fun rec(step: Step, deadline: Long? = null) = SwapRecord(
        id = "o1", fromWalletId = "w1", toWalletId = "w2", sendChain = "btc", amountSat = 30_000,
        limitPrice = 1_120_000, ownerSecret = "s", swapIndex = 7, wrappedXbt = "x", wrappedBtc = "b",
        redeemAddress = "r", refundAddress = "f", step = step, deadline = deadline,
    )

    @Test
    fun beforeStartingSayHowLongTheCoinsTakeToComeBack() {
        // XBT refunds after ~2 days, BTC within ~a day (swap_terms: 48 h / 24 h legs).
        assertEquals(R.string.swap_keep_on_xbt, keepOnBeforeStart("xbt"))
        assertEquals(R.string.swap_keep_on_btc, keepOnBeforeStart("btc"))
    }

    @Test
    fun anActiveSwapAsksToKeepThePhoneOn() {
        for (s in listOf(Step.WAITING_MATCH, Step.MATCHED, Step.FUNDED, Step.THEY_FUNDED, Step.REDEEMED, Step.REFUNDING)) {
            assertEquals(s.name, KeepOn(R.string.swap_keep_on_active, null), keepOnNote(rec(s)))
        }
    }

    @Test
    fun onOurTurnItNamesTheDeadline() {
        assertEquals(KeepOn(R.string.swap_keep_on_turn, 1_700L), keepOnNote(rec(Step.NEED_FUNDING, deadline = 1_700L)))
        // No deadline known: the plain reminder, never a made-up time.
        assertEquals(KeepOn(R.string.swap_keep_on_active, null), keepOnNote(rec(Step.NEED_FUNDING)))
    }

    @Test
    fun finishedSwapsSayNothing() {
        for (s in listOf(Step.DONE, Step.REFUNDED, Step.CANCELLED, Step.FAILED)) assertNull(s.name, keepOnNote(rec(s)))
    }
}

class SwapCancelledNoteTest {
    private val terms = org.json.JSONObject().put("xbt_locktime", 1_900_000L).put("btc_locktime", 1_800_000L).toString()

    private fun rec(sendChain: String, role: String, step: Step, matchStatus: String?, funded: Boolean) = SwapRecord(
        id = "o1", fromWalletId = "w1", toWalletId = "w2", sendChain = sendChain, amountSat = 30_000,
        limitPrice = 1_120_000, ownerSecret = "s", swapIndex = 7, wrappedXbt = "x", wrappedBtc = "b",
        redeemAddress = "r", refundAddress = "f", step = step, role = role, matchStatus = matchStatus,
        fundingTxid = if (funded) "aa" else null, termsJson = terms, matchId = "m1",
    )

    /** The other side never locked theirs: say so, and when ours come back — on their own. */
    @Test
    fun a_cancelled_match_with_our_coins_locked_says_when_they_come_back() {
        val initiator = rec("xbt", "initiator", Step.FUNDED, "cancelled", true)
        assertEquals(CancelNote(R.string.swap_cancelled_refund_on, 1_900_000L), cancelledNote(initiator))
        assertEquals(R.string.swap_status_cancelled_refund, swapStatusLabel(initiator))
        assertEquals(SwapTone.ATTENTION, swapTone(initiator))
        // the participant's own lock was what the server missed: the swap may still go ahead
        assertEquals(
            CancelNote(R.string.swap_cancelled_locked_participant, 1_800_000L),
            cancelledNote(rec("btc", "participant", Step.THEY_FUNDED, "cancelled", true)),
        )
    }

    /** The other side released our coins: they're on their way back now. */
    @Test
    fun a_released_swap_says_the_coins_are_coming_back_now() {
        val r = rec("xbt", "initiator", Step.REFUNDING, "cancelled", true).copy(marks = setOf("released"))
        assertEquals(CancelNote(R.string.swap_cancelled_released, null), cancelledNote(r))
    }

    /** Cancelled before any of ours was locked: nothing left our wallet. */
    @Test
    fun a_cancelled_swap_with_nothing_locked_says_nothing_was_spent() {
        assertEquals(CancelNote(R.string.swap_cancelled_nothing_spent, null), cancelledNote(rec("btc", "participant", Step.CANCELLED, "cancelled", false)))
    }

    @Test
    fun nothing_to_say_when_not_cancelled_or_finished() {
        assertNull(cancelledNote(rec("xbt", "initiator", Step.FUNDED, "initiator_funded", true)))
        assertNull(cancelledNote(rec("xbt", "initiator", Step.MATCHED, "cancelled", false)))
        assertNull(cancelledNote(rec("xbt", "initiator", Step.REFUNDED, "cancelled", true)))
        assertNull(cancelledNote(rec("xbt", "initiator", Step.DONE, "closed", true)))
    }
}

class SwapFundTurnTest {
    private val matchedAt = 1_000_000L
    private val terms = org.json.JSONObject()
        .put("matched_at", matchedAt).put("btc_locktime", matchedAt + 24 * 3600).put("xbt_locktime", matchedAt + 48 * 3600)
        .put("xbt_confs", 6).put("btc_confs", 2).toString()

    private fun participant(matchStatus: String = "initiator_funded", step: Step = Step.MATCHED, deadline: Long? = null) = SwapRecord(
        id = "o1", fromWalletId = "w1", toWalletId = "w2", sendChain = "btc", amountSat = 30_000,
        limitPrice = 1_120_000, ownerSecret = "s", swapIndex = 7, wrappedXbt = "x", wrappedBtc = "b",
        redeemAddress = "r", refundAddress = "f", step = step, role = "participant", matchStatus = matchStatus,
        termsJson = terms, matchId = "m1", deadline = deadline,
    )

    /** Waiting for their confirmations: about when our turn comes (10 min a
     *  block) and, from then, the server's 2 hours. */
    @Test
    fun the_participant_is_told_when_its_turn_comes_and_how_long_it_has() {
        val now = matchedAt + 600
        val n = com.fortis.wallet.ui.screens.fundTurn(participant(), theirConfs = 2, now = now)!!
        assertEquals(now + 4 * 600, n.at)
        assertEquals(now + 4 * 600 + 7200, n.until)
        // nothing known of their confirmations yet: all six to go
        assertEquals(now + 6 * 600, com.fortis.wallet.ui.screens.fundTurn(participant(), null, now)!!.at)
    }

    /** Once the server set the deadline, it is the exact one. */
    @Test
    fun the_servers_deadline_is_used_once_set() {
        val n = com.fortis.wallet.ui.screens.fundTurn(participant(deadline = matchedAt + 9_000), theirConfs = 6, now = matchedAt + 5_000)!!
        assertEquals(matchedAt + 5_000, n.at)
        assertEquals(matchedAt + 9_000, n.until)
    }

    /** Never later than the latest the participant may fund (12 h before the BTC locktime). */
    @Test
    fun the_estimate_never_passes_the_latest_funding_time() {
        val late = matchedAt + 11 * 3600
        val n = com.fortis.wallet.ui.screens.fundTurn(participant(), theirConfs = 0, now = late)!!
        assertEquals(matchedAt + 12 * 3600, n.until)
    }

    @Test
    fun nothing_to_say_before_they_fund_or_once_it_is_out_of_our_hands() {
        assertNull(com.fortis.wallet.ui.screens.fundTurn(participant(matchStatus = "accepted"), null, matchedAt))
        assertNull(com.fortis.wallet.ui.screens.fundTurn(participant(step = Step.NEED_FUNDING), 6, matchedAt), )
        assertNull(com.fortis.wallet.ui.screens.fundTurn(participant(matchStatus = "cancelled"), 6, matchedAt))
        assertNull(com.fortis.wallet.ui.screens.fundTurn(participant().copy(fundingTxid = "aa"), 6, matchedAt))
        assertNull(com.fortis.wallet.ui.screens.fundTurn(participant().copy(role = "initiator", sendChain = "xbt"), 6, matchedAt))
    }
}
