package com.fortis.wallet.ui

import com.fortis.wallet.R
import com.fortis.wallet.data.Step
import com.fortis.wallet.data.SwapRecord
import com.fortis.wallet.ui.screens.SwapTone
import com.fortis.wallet.ui.screens.activeDeadline
import com.fortis.wallet.ui.screens.swapTone
import com.fortis.wallet.ui.screens.swapStatusLabel
import com.fortis.wallet.ui.screens.timeline
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class SwapTimelineTest {
    private val terms = JSONObject()
        .put("match_id", "m1").put("network", "bitcoin")
        .put("xbt_sat", 10_000_000L).put("btc_sat", 112_000L).put("price_sat_per_xbt", 1_120_000L)
        .put("fee_bps", 50).put("fee_address", "bc1qfee")
        .put("matched_at", 1_000L).put("xbt_locktime", 1_000L + 144 * 3600).put("btc_locktime", 1_000L + 24 * 3600)
        .put("xbt_confs", 6).put("btc_confs", 2).put("accept_by", 1_120L).put("initiator_fund_by", 2_920L)
        .toString()

    private fun rec(sendChain: String = "xbt", step: Step = Step.WAITING_MATCH) = SwapRecord(
        id = "o1", fromWalletId = "w1", toWalletId = "w2", sendChain = sendChain, amountSat = 10_000_000,
        limitPrice = 1_120_000, ownerSecret = "s", swapIndex = 7, wrappedXbt = "x", wrappedBtc = "b",
        redeemAddress = "r", refundAddress = "f", step = step,
    )

    private fun ids(r: SwapRecord) = timeline(r).map { it.label }
    private fun done(r: SwapRecord) = timeline(r).map { it.done }

    @Test
    fun every_step_explains_itself() {
        val hints = timeline(rec("xbt").copy(role = "initiator", fundingTxid = "aa", theirTxid = "bb")).associate { it.label to it.hint }
        assertEquals(
            mapOf(
                R.string.swap_stage_waiting to R.string.swap_hint_waiting,
                R.string.swap_stage_matched to R.string.swap_hint_matched,
                R.string.swap_stage_you_funded to R.string.swap_hint_you_funded,
                R.string.swap_stage_they_funded to R.string.swap_hint_they_funded,
                R.string.swap_stage_redeemed to R.string.swap_hint_redeemed,
                R.string.swap_stage_done to R.string.swap_hint_done,
            ),
            hints,
        )
    }

    /** A stage not reached yet never reads as if it happened: the current one
     *  said "Redeemed" while the participant was still waiting. */
    @Test
    fun a_stage_not_done_yet_is_worded_as_still_to_come() {
        assertEquals(
            mapOf(
                R.string.swap_stage_waiting to R.string.swap_stage_waiting,
                R.string.swap_stage_matched to R.string.swap_pending_matched,
                R.string.swap_stage_you_funded to R.string.swap_pending_you_fund,
                R.string.swap_stage_they_funded to R.string.swap_pending_they_fund,
                R.string.swap_stage_redeemed to R.string.swap_pending_redeem,
                R.string.swap_stage_done to R.string.swap_pending_done,
            ),
            timeline(rec("xbt")).associate { it.label to it.pendingLabel },
        )
    }

    /** The swap's one-line status (Home, Swaps list) says what the detail
     *  screen's current stage says. */
    @Test
    fun the_status_line_matches_the_detail_screens_current_stage() {
        val participant = rec("btc", Step.THEY_FUNDED).copy(
            role = "participant", matchId = "m", termsJson = terms, fundingTxid = "bb", theirTxid = "aa",
        )
        assertEquals(R.string.swap_pending_redeem, swapStatusLabel(participant))
        val initiator = rec("xbt", Step.FUNDED).copy(role = "initiator", matchId = "m", termsJson = terms, fundingTxid = "aa")
        assertEquals(R.string.swap_pending_they_fund, swapStatusLabel(initiator))
        for (r in listOf(participant, initiator)) {
            val current = timeline(r).first { !it.done }.pendingLabel
            assertEquals(current, swapStatusLabel(r))
        }
        // States the timeline doesn't spell out keep their own words.
        assertEquals(R.string.swap_step_need_funding, swapStatusLabel(initiator.copy(step = Step.NEED_FUNDING, fundingTxid = null)))
        assertEquals(R.string.swap_step_waiting, swapStatusLabel(rec("xbt", Step.WAITING_MATCH)))
        assertEquals(R.string.swap_step_refunding, swapStatusLabel(initiator.copy(step = Step.REFUNDING)))
        assertEquals(R.string.swap_step_done, swapStatusLabel(initiator.copy(step = Step.DONE)))
        assertEquals(R.string.swap_step_cancelled, swapStatusLabel(initiator.copy(step = Step.CANCELLED)))
    }

    /** The participant collects only after the initiator has: that reveals the key. */
    @Test
    fun the_participant_waits_for_them_to_collect_first() {
        fun hint(r: SwapRecord) = timeline(r).single { it.label == R.string.swap_stage_redeemed }.hint
        val participant = rec("btc", Step.THEY_FUNDED).copy(role = "participant", fundingTxid = "bb", theirTxid = "aa")
        assertEquals(R.string.swap_hint_redeem_after_them, hint(participant))
        assertEquals(R.string.swap_hint_redeemed, hint(participant.copy(step = Step.REDEEMED)))
        val initiator = rec("xbt", Step.THEY_FUNDED).copy(role = "initiator", fundingTxid = "aa", theirTxid = "bb")
        assertEquals(R.string.swap_hint_redeemed, hint(initiator))
    }

    @Test
    fun the_initiator_funds_first_the_participant_second() {
        val i = rec("xbt").copy(role = "initiator")
        assertEquals(
            listOf(
                R.string.swap_stage_waiting, R.string.swap_stage_matched, R.string.swap_stage_you_funded,
                R.string.swap_stage_they_funded, R.string.swap_stage_redeemed, R.string.swap_stage_done,
            ),
            ids(i),
        )
        val p = rec("btc").copy(role = "participant")
        assertEquals(R.string.swap_stage_they_funded, ids(p)[2])
        assertEquals(R.string.swap_stage_you_funded, ids(p)[3])
    }

    @Test
    fun stages_follow_the_record() {
        assertEquals(listOf(false, false, false, false, false, false), done(rec()))
        val matched = rec().copy(matchId = "m1", termsJson = terms, role = "initiator", step = Step.MATCHED)
        assertEquals(listOf(true, true, false, false, false, false), done(matched))
        val funded = matched.copy(fundingTxid = "aa", step = Step.FUNDED)
        assertEquals(listOf(true, true, true, false, false, false), done(funded))
        val theyFunded = funded.copy(theirTxid = "bb", step = Step.THEY_FUNDED)
        assertEquals(listOf(true, true, true, true, false, false), done(theyFunded))
        assertEquals(listOf(true, true, true, true, true, false), done(theyFunded.copy(step = Step.REDEEMED)))
        assertEquals(listOf(true, true, true, true, true, true), done(theyFunded.copy(step = Step.DONE)))
    }

    @Test
    fun they_funded_counts_the_confirmations_their_chain_needs() {
        val i = rec("xbt").copy(role = "initiator", termsJson = terms)
        assertEquals(2L, timeline(i).single { it.label == R.string.swap_stage_they_funded }.confsNeeded)
        val p = rec("btc").copy(role = "participant", termsJson = terms)
        assertEquals(6L, timeline(p).single { it.label == R.string.swap_stage_they_funded }.confsNeeded)
    }

    @Test
    fun the_active_deadline() {
        assertNull(activeDeadline(rec(), 1_000))
        val matched = rec().copy(matchId = "m1", termsJson = terms, role = "initiator", step = Step.MATCHED)
        assertEquals(R.string.swap_deadline_accept to 1_120L, activeDeadline(matched, 1_050))
        // past accept_by, the initiator is up against its funding deadline
        assertEquals(R.string.swap_deadline_fund to 2_920L, activeDeadline(matched, 1_200))
        // the engine's own deadline (NEED_FUNDING) wins
        assertEquals(
            R.string.swap_deadline_fund to 2_500L,
            activeDeadline(matched.copy(step = Step.NEED_FUNDING, deadline = 2_500), 1_200),
        )
        // funded: the refund opens at our own leg's locktime
        val funded = matched.copy(fundingTxid = "aa", step = Step.FUNDED)
        assertEquals(R.string.swap_deadline_refund to 1_000L + 144 * 3600, activeDeadline(funded, 2_000))
        val pFunded = rec("btc").copy(role = "participant", termsJson = terms, matchId = "m1", fundingTxid = "aa", step = Step.FUNDED)
        assertEquals(R.string.swap_deadline_refund to 1_000L + 24 * 3600, activeDeadline(pFunded, 2_000))
        assertNull(activeDeadline(funded.copy(step = Step.DONE), 2_000))
    }

    @Test
    fun share_code_and_created_at_round_trip() {
        val r = rec().copy(shareCode = "ABCD2345", createdAt = 1_700_000_000)
        val back = SwapRecord.fromJson(JSONObject(r.toJson().toString()))
        assertEquals("ABCD2345", back.shareCode)
        assertEquals(1_700_000_000L, back.createdAt)
        assertEquals(r, back)
    }

    @Test
    fun they_funded_says_we_are_waiting_until_their_coins_are_actually_locked() {
        fun hint(r: SwapRecord) = timeline(r).single { it.label == R.string.swap_stage_they_funded }.hint
        // The initiator has funded; the participant locks only once that has its confirmations.
        val initiator = rec("xbt").copy(role = "initiator", termsJson = terms, fundingTxid = "aa", step = Step.FUNDED)
        assertEquals(R.string.swap_hint_they_funding_after_you, hint(initiator))
        // The participant waits for the initiator, who always locks first.
        val participant = rec("btc").copy(role = "participant", termsJson = terms, step = Step.MATCHED)
        assertEquals(R.string.swap_hint_they_funding_first, hint(participant))
        // Once their funding tx is seen, it's the "locked too, confirming" hint.
        assertEquals(R.string.swap_hint_they_funded, hint(initiator.copy(theirTxid = "bb")))
        assertEquals(R.string.swap_hint_they_funded, hint(participant.copy(theirTxid = "aa")))
    }

    /** Their funding is on chain (the edge has it) but not confirmed enough:
     *  their coins are locked, and we wait for confirmations — not for them. */
    @Test
    fun their_announced_funding_reads_as_locked_and_confirming() {
        fun stage(r: SwapRecord) = timeline(r).single { it.label == R.string.swap_stage_they_funded }
        val participant = rec("btc").copy(role = "participant", matchId = "m1", termsJson = terms, step = Step.MATCHED, matchStatus = "initiator_funded")
        assertEquals(R.string.swap_pending_they_confirming, stage(participant).pendingLabel)
        assertEquals(R.string.swap_hint_they_confirming_first, stage(participant).hint)
        assertEquals(R.string.swap_pending_they_confirming, swapStatusLabel(participant))
        val initiator = rec("xbt").copy(
            role = "initiator", termsJson = terms, fundingTxid = "aa", step = Step.FUNDED, matchStatus = "participant_funded",
        )
        assertEquals(R.string.swap_pending_they_confirming, stage(initiator).pendingLabel)
        assertEquals(R.string.swap_hint_they_confirming_after_you, stage(initiator).hint)
        // the initiator's own funding isn't theirs
        assertEquals(R.string.swap_pending_they_fund, stage(initiator.copy(matchStatus = "initiator_funded")).pendingLabel)
        assertEquals(R.string.swap_hint_they_funding_after_you, stage(initiator.copy(matchStatus = "initiator_funded")).hint)
    }
    /** A running swap reads as live (the pulsing green dot); one that needs
     *  the user, or is going wrong, keeps its own colour; a finished one is plain. */
    @Test
    fun a_running_swap_reads_as_live() {
        for (st in listOf(Step.PROVISIONAL, Step.WAITING_MATCH, Step.MATCHED, Step.FUNDED, Step.THEY_FUNDED, Step.REDEEMED)) {
            assertEquals(st.name, SwapTone.LIVE, swapTone(rec(step = st)))
        }
        assertEquals(SwapTone.ATTENTION, swapTone(rec(step = Step.NEED_FUNDING)))
        assertEquals(SwapTone.ATTENTION, swapTone(rec(step = Step.REFUNDING)))
        assertEquals(SwapTone.DONE, swapTone(rec(step = Step.DONE)))
        assertEquals(SwapTone.ATTENTION, swapTone(rec(step = Step.REFUNDED)))
        assertEquals(SwapTone.BAD, swapTone(rec(step = Step.FAILED)))
        assertEquals(SwapTone.BAD, swapTone(rec(step = Step.CANCELLED)))
    }
    /** Before our coins are in, "Your funding" never says they are locked: the
     *  app puts them in (while unlocked), or — our turn and it's locked — asks
     *  to be opened. */
    @Test
    fun our_funding_step_says_what_happens_before_our_coins_are_in() {
        fun hint(r: SwapRecord) = timeline(r).single { it.label == R.string.swap_stage_you_funded }.hint
        val p = rec("btc").copy(role = "participant", matchId = "m1", termsJson = terms, matchStatus = "initiator_funded", theirTxid = "aa")
        assertEquals(R.string.swap_hint_you_funding, hint(p.copy(step = Step.THEY_FUNDED)))
        assertEquals(R.string.swap_hint_open_to_fund, hint(p.copy(step = Step.NEED_FUNDING)))
        assertEquals(R.string.swap_hint_you_funded, hint(p.copy(step = Step.THEY_FUNDED, fundingTxid = "bb")))
    }
}
