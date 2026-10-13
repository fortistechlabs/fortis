package com.fortis.wallet.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** The same fixtures and the same numbers as fortis-edge's `swap::neoxex` tests:
 *  the phone and the edge must agree on the price. */
class NeoxExTest {
    private fun fixture(name: String): String =
        javaClass.getResource("/neoxex/$name.json")!!.readText()

    private val bookOk = fixture("orderbook_ok")
    private val bookWide = fixture("orderbook_wide")
    private val bookLive = fixture("orderbook_live")
    private val tickerOk = fixture("ticker_ok")
    private val tickerPaused = fixture("ticker_paused")
    private val tickerFar = fixture("ticker_far")
    private val tickerLive = fixture("ticker_live")
    private val pairsLive = fixture("pairs_live")
    private val pairsPaused = fixture("pairs_paused")

    /** `computedAt` (ms) of every ticker fixture, in seconds. */
    private val computed = 1_791_670_572L

    /** The old "refuses to quote" view: a price only where the reference is ok. */
    private fun priceFrom(book: String, ticker: String, pairs: String, now: Long): Long? =
        NeoxEx.referenceFrom(book, ticker, pairs, now)?.takeIf { it.ok }?.priceSatPerXbt

    @Test
    fun wideBookStillGivesAReferenceButNotOk() {
        val r = NeoxEx.referenceFrom(bookWide, tickerOk, pairsLive, computed + 10)
        assertNotNull(r)
        assertFalse(r!!.ok)
        assertTrue(r.priceSatPerXbt > 0)
        // the good fixture is both
        assertEquals(NeoxEx.Reference(1_008_980L, true), NeoxEx.referenceFrom(bookOk, tickerOk, pairsLive, computed + 10))
    }

    @Test
    fun referenceIsNotOkWhenStaleOrPausedButKeepsThePrice() {
        assertEquals(false, NeoxEx.referenceFrom(bookOk, tickerOk, pairsLive, computed + 61)?.ok)
        assertEquals(false, NeoxEx.referenceFrom(bookOk, tickerPaused, pairsLive, computed + 10)?.ok)
        assertEquals(false, NeoxEx.referenceFrom(bookOk, tickerOk, "junk", computed + 10)?.ok)
        assertEquals(1_008_980L, NeoxEx.referenceFrom(bookOk, "junk", pairsLive, computed + 10)?.priceSatPerXbt)
    }

    @Test
    fun weightedMidOfFixture() {
        // bid vwap 0.00999637, ask vwap 0.01018323 -> mid 0.01008980 BTC/XBT.
        assertEquals(1_008_980L, priceFrom(bookOk, tickerOk, pairsLive, computed + 10))
    }

    @Test
    fun wideSpreadRefused() {
        assertNull(priceFrom(bookWide, tickerOk, pairsLive, computed + 10))
    }

    @Test
    fun liveSnapshotIsTooWideToQuote() {
        assertNull(priceFrom(bookLive, tickerLive, pairsLive, computed + 10))
    }

    @Test
    fun pausedTickerRefused() {
        assertNull(priceFrom(bookOk, tickerPaused, pairsLive, computed + 10))
    }

    @Test
    fun pausedPairRefused() {
        assertNull(priceFrom(bookOk, tickerOk, pairsPaused, computed + 10))
        // a pair missing from /pairs fails closed, as on the edge
        assertNull(priceFrom(bookOk, tickerOk, """{"pairs":[]}""", computed + 10))
        assertNull(priceFrom(bookOk, tickerOk, "junk", computed + 10))
    }

    @Test
    fun staleRefusedFromUpstreamComputedAt() {
        assertNull(priceFrom(bookOk, tickerOk, pairsLive, computed + 61))
        assertNotNull(priceFrom(bookOk, tickerOk, pairsLive, computed + 60))
    }

    @Test
    fun missingOrNonNumericComputedAtRefused() {
        val noTs = tickerOk.replace("computedAt", "computedAtX")
        assertNull(priceFrom(bookOk, noTs, pairsLive, computed + 10))
        val bad = tickerOk.replace("1791670572294", "\"soon\"")
        assertNull(priceFrom(bookOk, bad, pairsLive, computed + 10))
    }

    @Test
    fun farFromLastRefused() {
        assertNull(priceFrom(bookOk, tickerFar, pairsLive, computed + 10))
    }

    private fun ticker(last: Double) =
        """{"ticker":{"lastPrice":$last,"computedAt":${computed * 1000}}}"""

    @Test
    fun thinBookUsesWhatExists() {
        // 0.0099 BTC and 0.0202 BTC of depth, both under 0.05: mid = 0.01 exactly.
        val book = """{"bids":[{"price":0.0099,"quantity":1.0}],"asks":[{"price":0.0101,"quantity":2.0}]}"""
        assertEquals(1_000_000L, priceFrom(book, ticker(0.01), pairsLive, computed + 10))
    }

    @Test
    fun emptySideRefused() {
        val noBids = """{"bids":[],"asks":[{"price":0.0101,"quantity":2.0}]}"""
        val noAsks = """{"bids":[{"price":0.0099,"quantity":1.0}],"asks":[]}"""
        assertNull(priceFrom(noBids, ticker(0.01), pairsLive, computed + 10))
        assertNull(priceFrom(noAsks, ticker(0.01), pairsLive, computed + 10))
    }

    @Test
    fun junkJsonRefused() {
        assertNull(priceFrom("{}", tickerOk, pairsLive, computed + 10))
        assertNull(priceFrom(bookOk, """{"ticker":{}}""", pairsLive, computed + 10))
    }
}
