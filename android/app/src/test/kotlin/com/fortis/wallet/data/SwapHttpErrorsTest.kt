package com.fortis.wallet.data

import kotlinx.coroutines.runBlocking
import okhttp3.OkHttpClient
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.net.InetAddress
import java.net.ServerSocket
import kotlin.concurrent.thread

/**
 * A network or HTTP failure is not an answer: `NeoxEx.price` and
 * `EdgeSwapApi.pricingAddress` throw (the engine retries) and return `null` only
 * for a definite refusal — so a flaky connection never turns into a decline.
 */
class SwapHttpErrorsTest {
    private fun fixture(name: String): String = javaClass.getResource("/neoxex/$name.json")!!.readText()

    /** `computedAt` (ms) of the ticker fixtures, in seconds. */
    private val computed = 1_791_670_572L

    /** path → (status, body) */
    private val routes = java.util.concurrent.ConcurrentHashMap<String, Pair<Int, String>>()
    /** path → the last request body sent to it */
    private val received = java.util.concurrent.ConcurrentHashMap<String, String>()
    /** A bare HTTP/1.1 server: one response per connection, then close. */
    private val server = ServerSocket(0, 50, InetAddress.getLoopbackAddress())
    private val base = "http://127.0.0.1:${server.localPort}"
    private val http = OkHttpClient()

    init {
        thread(isDaemon = true) {
            while (!server.isClosed) {
                val sock = runCatching { server.accept() }.getOrNull() ?: break
                sock.use { c ->
                    val reader = c.getInputStream().bufferedReader()
                    val path = reader.readLine().orEmpty().split(" ").getOrElse(1) { "/" }.substringBefore('?')
                    var length = 0
                    while (true) {
                        val h = reader.readLine() ?: break
                        if (h.isEmpty()) break
                        if (h.lowercase().startsWith("content-length:")) length = h.substringAfter(':').trim().toInt()
                    }
                    if (length > 0) received[path] = CharArray(length).let { String(it, 0, reader.read(it)) }
                    val (code, body) = routes[path] ?: (404 to "")
                    val bytes = body.toByteArray()
                    c.getOutputStream().apply {
                        write("HTTP/1.1 $code X\r\nContent-Length: ${bytes.size}\r\nConnection: close\r\n\r\n".toByteArray())
                        write(bytes)
                        flush()
                    }
                }
            }
        }
    }

    @After
    fun stop() = server.close()

    private fun neoxex(book: String, code: Int = 200) {
        routes["/api/exchange/orderbook/BTCB2_BTC"] = code to fixture(book)
        routes["/api/exchange/ticker/BTCB2_BTC"] = 200 to fixture("ticker_ok")
        routes["/api/exchange/pairs"] = 200 to fixture("pairs_live")
    }

    @Test
    fun neoxex_price_quotes() = runBlocking {
        neoxex("orderbook_ok")
        assertEquals(1_008_980L, NeoxEx.price(http, base, computed + 10))
    }

    @Test
    fun neoxex_price_is_null_when_it_refuses_to_quote() = runBlocking {
        neoxex("orderbook_wide")
        assertNull(NeoxEx.price(http, base, computed + 10))
    }

    @Test
    fun neoxex_price_throws_when_the_exchange_errors() {
        neoxex("orderbook_ok", code = 502)
        val r = runCatching { runBlocking { NeoxEx.price(http, base, computed + 10) } }
        assertTrue("an HTTP error must throw, not read as a refusal", r.isFailure)
    }

    @Test
    fun neoxex_price_throws_when_unreachable() {
        server.close()
        val r = runCatching { runBlocking { NeoxEx.price(http, base, computed + 10) } }
        assertTrue(r.isFailure)
    }

    @Test
    fun pricing_address_reads_the_address() = runBlocking {
        routes["/pricing"] = 200 to """{"address":"bc1qfee","bps":50}"""
        assertEquals("bc1qfee", EdgeSwapApi(http, base) { "tok" }.pricingAddress())
    }

    @Test
    fun pricing_address_throws_on_http_error() {
        routes["/pricing"] = 503 to """{"error":"busy"}"""
        val r = runCatching { runBlocking { EdgeSwapApi(http, base) { "tok" }.pricingAddress() } }
        assertTrue("an HTTP error must throw, not read as a mismatch", r.isFailure)
    }

    @Test
    fun create_offer_sends_the_client_id() = runBlocking {
        routes["/swap/offers"] = 200 to """{"offer_id":"o1","owner_secret":"s1","share_code":null}"""
        val o = EdgeSwapApi(http, base) { "tok" }.createOffer("btc_to_xbt", 100_000, 100, false, "abc123")
        assertEquals(OfferCreated("o1", "s1", null), o)
        val sent = org.json.JSONObject(received.getValue("/swap/offers"))
        assertEquals("abc123", sent.getString("client_id"))
        assertEquals("btc_to_xbt", sent.getString("direction"))
    }
}
