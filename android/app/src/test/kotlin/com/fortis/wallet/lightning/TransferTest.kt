package com.fortis.wallet.lightning

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.assertNull
import org.junit.Test
import uniffi.wallet_ffi.LnChannelState

class TransferTest {
    @Test
    fun `fee is max of half a percent, 1000 sat and the network fee`() {
        assertEquals(1_000L, transferFeeSat(50_000, 300))
        assertEquals(2_000L, transferFeeSat(50_000, 2_000))
        assertEquals(2_500L, transferFeeSat(500_000, 300))
    }

    @Test
    fun `operator keeps the fee less the network fee`() {
        // The user's examples: network 300 -> fee 1,000, operator keeps 700; network 2,000 -> nothing.
        assertEquals(700L, operatorShareSat(50_000, 300))
        assertEquals(0L, operatorShareSat(50_000, 2_000))
        assertEquals("400 sat is below dust", 0L, operatorShareSat(50_000, 600))
        // On-chain the edge's floor always applies.
        assertEquals(546L, operatorShareSat(50_000, 2_000, floorSat = 546))
    }

    @Test
    fun `to lightning quote adds up`() {
        val q = quoteToLightning(100_000, depositFeeSat = 150, feerateSatVb = 1, floorSat = 0)!!
        assertEquals(200L, q.channelFeeSat)
        assertEquals(650L, q.shareSat)
        assertEquals(1_000L, q.feeSat)
        assertEquals(100_000L - 1_000, q.arrivesSat)
        assertEquals(q.amountSat, q.toTransitSat + q.depositFeeSat + q.shareSat)
        // A share below dust is not charged: the user pays only the network fees.
        val small = quoteToLightning(100_000, depositFeeSat = 280, feerateSatVb = 2, floorSat = 0)!!
        assertEquals(0L, small.shareSat)
        assertEquals(680L, small.feeSat)
    }

    @Test
    fun `too small for a channel gives no quote`() {
        assertNull(quoteToLightning(5_500, 200, 1, 0))
    }

    @Test
    fun `to onchain quote pays out the rest`() {
        val q = quoteToOnchain(200_000, feerateSatVb = 3)!!
        assertEquals(600L, q.networkFeeSat)
        assertEquals("0.5% of 200,000", 1_000L, q.shareSat)
        assertEquals(198_400L, q.payoutSat)
    }

    @Test
    fun `a small move out pays the network fee plus a small fixed share not a fixed 1000`() {
        // 0.5% of 1,738 is 8 sat: the 150 sat minimum applies.
        assertEquals(150L, transferOutShareSat(1_738))
        assertEquals(150L, transferOutShareSat(30_000))
        assertEquals(1_000L, transferOutShareSat(200_000))
        val q = quoteToOnchain(5_000, feerateSatVb = 2, networkFeeSat = 1_171)!!
        assertEquals(1_171L, q.networkFeeSat)
        assertEquals(150L, q.shareSat)
        assertEquals(5_000L - 1_171L - 150L, q.payoutSat)
    }

    @Test
    fun `a close of a tiny channel still quotes the real small amount`() {
        // 1,738 sat less 1,171 network and 150 share leaves 417: below dust, so a splice would refuse it...
        assertNull(quoteToOnchain(1_738, feerateSatVb = 2, networkFeeSat = 1_171))
        // ...but a close shows it.
        val q = quoteToOnchain(1_738, feerateSatVb = 2, networkFeeSat = 1_171, allowDust = true)!!
        assertEquals(417L, q.payoutSat)
    }

    @Test
    fun `transit opens, splices or waits`() {
        assertEquals(TransitAction.Open, transitAction(20_000, LnChannelState.NONE, 1))
        assertEquals(TransitAction.Wait, transitAction(5_000, LnChannelState.NONE, 1))
        assertEquals(TransitAction.SpliceIn, transitAction(5_000, LnChannelState.READY, 1))
        assertEquals(TransitAction.Wait, transitAction(20_000, LnChannelState.OPENING, 1))
    }

    @Test
    fun `a move out quote carries its feerate`() {
        val q = quoteToOnchain(200_000, feerateSatVb = 4)!!
        assertEquals(4L, q.feerateSatVb)
        assertFalse(q.fallbackFeerate)
        val f = quoteToOnchain(200_000, feerateSatVb = 2, fallbackFeerate = true)!!
        assertTrue(f.fallbackFeerate)
    }

    /** A move out executes the quote the user confirmed, at its feerate: a fresh rate read
     *  just before is only a reason to stop and show new amounts (well above it, or the quote
     *  had no rate yet), never to change what is paid unseen. */
    @Test
    fun `a move out executes the confirmed quote or asks again`() {
        val q = quoteToOnchain(200_000, feerateSatVb = 4)!!
        assertEquals(MoveOutStep.Execute(q, 4), moveOutStep(q, 200_000, all = false, freshFeerate = 5))
        assertEquals(MoveOutStep.Execute(q, 4), moveOutStep(q, 200_000, all = false, freshFeerate = 6))
        assertEquals(MoveOutStep.Execute(q, 4), moveOutStep(q, 200_000, all = false, freshFeerate = 1))
        assertEquals(MoveOutStep.Execute(q, 4), moveOutStep(q, 200_000, all = false, freshFeerate = null))
        assertEquals(MoveOutStep.Requote(7), moveOutStep(q, 200_000, all = false, freshFeerate = 7))
        assertEquals(MoveOutStep.Requote(40), moveOutStep(q, 200_000, all = true, freshFeerate = 40))
        // Quoted at the 2 sat/vB stand-in: any real rate that differs is shown first.
        val f = quoteToOnchain(200_000, feerateSatVb = 2, fallbackFeerate = true)!!
        assertEquals(MoveOutStep.Requote(3), moveOutStep(f, 200_000, all = false, freshFeerate = 3))
        assertEquals(MoveOutStep.Requote(1), moveOutStep(f, 200_000, all = false, freshFeerate = 1))
        assertEquals(MoveOutStep.Execute(f, 2), moveOutStep(f, 200_000, all = false, freshFeerate = 2))
        assertEquals(MoveOutStep.Execute(f, 2), moveOutStep(f, 200_000, all = false, freshFeerate = null))
        // No quote: everything (a close takes what is there) at the fresh rate, else too small.
        val crumbs = moveOutStep(null, 4_000, all = true, freshFeerate = 9)
        assertEquals(MoveOutStep.Execute(ToOnchainQuote(4_000, 0, 0, feerateSatVb = 9), 9), crumbs)
        assertEquals(MoveOutStep.Execute(ToOnchainQuote(4_000, 0, 0, feerateSatVb = 2), 2), moveOutStep(null, 4_000, all = true, freshFeerate = null))
        assertEquals(MoveOutStep.TooSmall, moveOutStep(null, 4_000, all = false, freshFeerate = 9))
        // A quote for another amount is no approval of this one.
        assertEquals(MoveOutStep.Requote(4), moveOutStep(q, 300_000, all = false, freshFeerate = 4))
    }
}

/**
 * 2026-11-16: moving a whole 148,832 sat balance failed with "insufficient funds: need 148950,
 * have 148832": the Move guessed 150 sat of network fee, the planner needs 268 for two coins and
 * the guess was never raised.
 */
class FeeRetryTest {
    private class Short(msg: String) : Exception(msg)

    @Test
    fun theGuessRisesUntilThePlanFits() {
        val tried = mutableListOf<Long>()
        val got = withFeeRetry(startFee = 150, step = 50, attempts = 12) { fee ->
            tried += fee
            if (fee < 268) throw Short("need more") else fee
        }
        assertEquals(300L, got)
        assertEquals(listOf(150L, 200L, 250L, 300L), tried)
    }

    @Test
    fun aPlanThatFitsFirstTimeIsNotRetried() {
        var calls = 0
        assertEquals(150L, withFeeRetry(150, 50, 12) { calls++; it })
        assertEquals(1, calls)
    }

    @Test
    fun aPlanThatNeverFitsFailsWithTheLastErrorAfterTheLimit() {
        var calls = 0
        val e = runCatching { withFeeRetry(150, 50, 5) { calls++; throw Short("attempt $calls") } }.exceptionOrNull()
        assertEquals(5, calls)
        assertEquals("attempt 5", e?.message)
    }
}

/** After moving everything on-chain → Lightning the wallet's confirmed balance is 0: the Move tab
 *  must say the move is under way, not "Nothing to move yet". */
class MoveInNoticeTest {
    @Test
    fun withBalanceThereIsNoNotice() {
        assertEquals(MoveInNotice.None, moveInNotice(maxSat = 148_832, transferPending = false))
        assertEquals(MoveInNotice.None, moveInNotice(maxSat = 148_832, transferPending = true))
    }

    @Test
    fun noBalanceAndNothingUnderWayIsNothingToMove() {
        assertEquals(MoveInNotice.NothingToMove, moveInNotice(maxSat = 0, transferPending = false))
    }

    @Test
    fun noBalanceBecauseItJustMovedIsUnderWay() {
        assertEquals(MoveInNotice.UnderWay, moveInNotice(maxSat = 0, transferPending = true))
    }
}

class LightningMoveLabelTest {
    @Test
    fun aMoveInProgressIntoLightningHasItsOwnLabel() {
        assertEquals(LnMoveLabel.MovingIn, lightningMoveLabel(transferPending = true, opening = false, closing = false))
    }

    @Test
    fun anOpeningChannelSaysSo() {
        assertEquals(LnMoveLabel.ChannelOpening, lightningMoveLabel(transferPending = false, opening = true, closing = false))
    }

    @Test
    fun aClosingChannelSaysSo() {
        assertEquals(LnMoveLabel.ChannelClosing, lightningMoveLabel(transferPending = false, opening = false, closing = true))
    }

    @Test
    fun nothingGoingOnShowsNoLabelNeverABareMoving() {
        assertNull(lightningMoveLabel(transferPending = false, opening = false, closing = false))
    }
}
