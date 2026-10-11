package com.fortis.wallet.ui

import com.fortis.wallet.R
import com.fortis.wallet.data.Step
import com.fortis.wallet.data.SwapRecord
import com.fortis.wallet.ui.screens.activeDeadline
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
        slippageBps = 100, ownerSecret = "s", swapIndex = 7, wrappedXbt = "x", wrappedBtc = "b",
        redeemAddress = "r", refundAddress = "f", step = step,
    )

    private fun ids(r: SwapRecord) = timeline(r).map { it.label }
    private fun done(r: SwapRecord) = timeline(r).map { it.done }

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
}
