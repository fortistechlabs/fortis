package com.fortis.wallet.lightning

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * 2026-11-16: the XBT edge refused the channel's funding transaction (402) and nothing told
 * the user: the Move said "Moving…" for ever. The app now asks the network whether it has the
 * funding transaction of a pending channel and says so when it does not.
 */
class StuckChannelTest {
    private val t0 = 1_000_000L

    @Test
    fun noPendingFundingMeansNothingIsWatched() {
        assertNull(nextMissingSince(prev = t0, pendingFundingTxid = null, onNetwork = false, nowMs = t0 + 1))
    }

    @Test
    fun aFundingTheNetworkHasClearsTheWatch() {
        assertNull(nextMissingSince(prev = t0, pendingFundingTxid = "ab", onNetwork = true, nowMs = t0 + 1))
    }

    @Test
    fun aFundingTheNetworkLacksStartsTheClockOnceAndKeepsIt() {
        val first = nextMissingSince(prev = null, pendingFundingTxid = "ab", onNetwork = false, nowMs = t0)
        assertEquals(t0, first)
        assertEquals(t0, nextMissingSince(prev = first, pendingFundingTxid = "ab", onNetwork = false, nowMs = t0 + 90_000))
    }

    @Test
    fun anUnknownAnswerNeitherStartsNorClearsTheClock() {
        // Offline, a server error: not evidence either way.
        assertNull(nextMissingSince(prev = null, pendingFundingTxid = "ab", onNetwork = null, nowMs = t0))
        assertEquals(t0, nextMissingSince(prev = t0, pendingFundingTxid = "ab", onNetwork = null, nowMs = t0 + 5))
    }

    @Test
    fun aChannelIsStuckOnlyAfterTheGracePeriod() {
        assertFalse(isChannelStuck(missingSince = null, nowMs = t0))
        assertFalse(isChannelStuck(missingSince = t0, nowMs = t0 + STUCK_AFTER_MS - 1))
        assertTrue(isChannelStuck(missingSince = t0, nowMs = t0 + STUCK_AFTER_MS))
    }

    @Test
    fun theGraceCoversASlowIndexButNotAForeverPendingChannel() {
        // A node's funding reaches the index in seconds; the user wants to be told within ~20 s.
        assertTrue(STUCK_AFTER_MS in 10_000L..120_000L)
    }
}

class FundingStatusTest {
    @Test
    fun theIndexAnswersFoundOrNotFound() {
        assertEquals(true, fundingKnownFromStatus(200))
        assertEquals(false, fundingKnownFromStatus(404))
    }

    @Test
    fun anythingElseIsNoAnswer() {
        // A rate limit, a gateway error, an auth failure: not evidence the network lacks the tx.
        for (code in listOf(0, 400, 401, 402, 429, 500, 502, 503)) {
            assertNull("HTTP $code", fundingKnownFromStatus(code))
        }
    }
}

class CancelledMoveInTest {
    private val into = LnMove(10, 144_190, 144_190, 1_456, "XBT · Wallet", null, "in")

    @Test
    fun cancellingRecordsTheChannelCloseSoTheHistoryAndLedgerAgree() {
        val c = cancelMoveFor(listOf(into), nowSecs = 99, toWallet = "XBT · Wallet")!!
        assertEquals("close", c.kind)
        assertEquals(99L, c.atSecs)
        // Exactly what the "moved in" entry counted comes back out, so the ledger nets to zero.
        assertEquals(144_190L, c.amountSat)
        assertEquals("XBT · Wallet", c.toWallet)
        assertNull(c.txid)
    }

    @Test
    fun aCancelIsRecordedOnce() {
        val c = cancelMoveFor(listOf(into), 99, "XBT · Wallet")!!
        assertNull(cancelMoveFor(listOf(into, c), 100, "XBT · Wallet"))
    }

    @Test
    fun noMoveInMeansNothingToCancel() {
        assertNull(cancelMoveFor(emptyList(), 1, "w"))
        assertNull(cancelMoveFor(listOf(LnMove(1, 5, 5, 0, "w", null, "splice")), 1, "w"))
    }

    @Test
    fun aLaterMoveInIsCancelledEvenAfterAnEarlierCancel() {
        val c = cancelMoveFor(listOf(into), 99, "w")!!
        val again = LnMove(200, 100_000, 100_000, 1_000, "w", null, "in")
        assertEquals(100_000L, cancelMoveFor(listOf(into, c, again), 300, "w")!!.amountSat)
    }
}

class BackfillCancelTest {
    private val into = LnMove(10, 144_190, 144_190, 1_456, "w", null, "in")
    private fun ready(channel: uniffi.wallet_ffi.LnChannelState, ln: ULong = 0uL, transit: ULong = 0uL, close: ULong = 0uL) =
        LnState.Ready(
            uniffi.wallet_ffi.LnBalances(ln, transit, close, 0uL, 0uL, 0uL), channel,
        )

    @Test
    fun aMoveInWithNothingLeftAndNoChannelWasCancelledAndGetsItsCloseEntry() {
        assertTrue(needsCancelBackfill(listOf(into), ready(uniffi.wallet_ffi.LnChannelState.NONE)))
    }

    @Test
    fun notWhileTheChannelOrFundsExist() {
        assertFalse(needsCancelBackfill(listOf(into), ready(uniffi.wallet_ffi.LnChannelState.READY, ln = 143_607uL)))
        assertFalse(needsCancelBackfill(listOf(into), ready(uniffi.wallet_ffi.LnChannelState.NONE, transit = 144_590uL)))
        assertFalse(needsCancelBackfill(listOf(into), ready(uniffi.wallet_ffi.LnChannelState.CLOSING, close = 1uL)))
    }

    @Test
    fun notWhenTheCloseIsAlreadyRecordedOrNodeNotReady() {
        val close = cancelMoveFor(listOf(into), 20, "w")!!
        assertFalse(needsCancelBackfill(listOf(into, close), ready(uniffi.wallet_ffi.LnChannelState.NONE)))
        assertFalse(needsCancelBackfill(listOf(into), LnState.Starting))
    }
}

class NormalizeCancelTest {
    private val into = LnMove(10, 144_190, 144_190, 1_456, "w", null, "in")

    @Test
    fun anEarlierBuildsCloseThatAddedTheFeeIsCorrected() {
        val wrong = LnMove(20, 145_646, 144_190, 0, "w", null, "close")
        val fixed = normalizeCancelMoves(listOf(into, wrong))!!
        assertEquals(144_190L, fixed[1].amountSat)
        assertEquals(144_190L, fixed[1].payoutSat)
        assertNull("nothing to correct twice", normalizeCancelMoves(fixed))
    }

    @Test
    fun otherClosesAreLeftAlone() {
        val real = LnMove(20, 90_000, 89_000, 1_000, "w", "fee", "close")
        assertNull(normalizeCancelMoves(listOf(into, real)))
        assertNull(normalizeCancelMoves(listOf(into)))
    }
}
