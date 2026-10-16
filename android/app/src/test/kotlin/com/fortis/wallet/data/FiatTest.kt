package com.fortis.wallet.data

import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import java.util.Currency
import java.util.Locale

class FiatTest {
    private lateinit var saved: Locale

    @Before fun pin() { saved = Locale.getDefault(); Locale.setDefault(Locale.US) }
    @After fun restore() = Locale.setDefault(saved)

    @Test
    fun readsEveryCurrencyTheEdgeSends() {
        val p = parsePrices("""{"USD":655.0,"EUR":584.404,"JPY":103474.5717}""")!!
        assertEquals(mapOf("USD" to 655.0, "EUR" to 584.404, "JPY" to 103474.5717), p)
    }

    @Test
    fun anOldEdgeWithUsdOnlyStillWorks() {
        assertEquals(mapOf("USD" to 655.0), parsePrices("""{"USD":655}"""))
    }

    @Test
    fun junkIsDroppedAndNoUsdMeansNoFeed() {
        val p = parsePrices("""{"USD":1,"EUR":-1,"GBP":0,"eur":5,"EURO":5,"time":17,"CHF":"9","JPY":null}""")
        assertEquals(mapOf("USD" to 1.0), p)
        assertNull(parsePrices("""{"EUR":584.4}"""))
        assertNull(parsePrices("not json"))
        assertNull(parsePrices("""{"error":"missing or invalid token"}"""))
    }

    @Test
    fun theDefaultFollowsTheRegion() {
        assertEquals("EUR", defaultFiat(Locale.GERMANY))
        assertEquals("EUR", defaultFiat(Locale("ga", "IE")))
        assertEquals("GBP", defaultFiat(Locale.UK))
        assertEquals("JPY", defaultFiat(Locale.JAPAN))
        assertEquals("USD", defaultFiat(Locale.US))
        assertEquals("USD", defaultFiat(Locale("en", "NG"))) // NGN isn't offered
        assertEquals("USD", defaultFiat(Locale.ENGLISH)) // no region
    }

    @Test
    fun everyOfferedCodeIsARealCurrency() {
        assertEquals(FIAT_CODES.size, FIAT_CODES.toSet().size)
        for (c in FIAT_CODES) Currency.getInstance(c)
        assertTrue("USD" in FIAT_CODES && "EUR" in FIAT_CODES)
    }

    @Test
    fun formatsInTheCurrencysOwnDecimals() {
        assertEquals("$655.00", fmtFiat(655.0, "USD"))
        assertEquals("€584.40", fmtFiat(584.404, "EUR"))
        assertEquals("¥103,475", fmtFiat(103474.5717, "JPY"))
        Locale.setDefault(Locale.GERMANY)
        assertEquals("584,40 €", fmtFiat(584.404, "EUR").replace('\u00A0', ' '))
    }

    @Test
    fun aCodeIsSpacedFromTheNumber() {
        Locale.setDefault(Locale("en", "IE"))
        assertEquals("SEK 1,234.50", fmtFiat(1234.5, "SEK").replace('\u00A0', ' '))
        assertEquals("<SEK 0.01", fmtFiat(0.001, "SEK").replace('\u00A0', ' '))
        assertEquals("€1,234.50", fmtFiat(1234.5, "EUR"))
    }

    @Test
    fun aTinyAmountIsNotShownAsZero() {
        assertEquals("<$0.01", fmtFiat(0.004, "USD"))
        assertEquals("<¥1", fmtFiat(0.4, "JPY"))
        assertEquals("$0.00", fmtFiat(0.0, "USD"))
    }
}
