package com.fortis.wallet.data

import kotlinx.coroutines.runBlocking
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Protocol
import okhttp3.Response
import okhttp3.ResponseBody.Companion.toResponseBody
import okio.Buffer
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.IOException

class ExplorerChainCheckTest {
    private val txid = "aa".repeat(32)
    private val spender = "bb".repeat(32)

    /** "METHOD url" → (code, body); code 0 = connection failure. Unlisted: 404. */
    private val routes = mutableMapOf<String, Pair<Int, String>>()
    private val asked = mutableListOf<String>()
    private val posted = mutableListOf<String>()

    private val http = OkHttpClient.Builder().addInterceptor { chain ->
        val req = chain.request()
        val key = "${req.method} ${req.url}"
        asked += key
        req.body?.let { b -> posted += Buffer().also { b.writeTo(it) }.readUtf8() }
        val (code, body) = routes[key] ?: (404 to "Transaction not found")
        if (code == 0) throw IOException("connection refused")
        Response.Builder().request(req).protocol(Protocol.HTTP_1_1).code(code).message("-")
            .body(body.toResponseBody("text/plain".toMediaType())).build()
    }.build()

    private val check = ExplorerChainCheck(http, mapOf("btc" to listOf("https://one/api", "https://two/api")))

    private fun serve(base: String, confirmedAt: Long? = 100, spent: Boolean = false, tip: Long = 105) {
        val status = if (confirmedAt == null) """{"confirmed":false}""" else """{"confirmed":true,"block_height":$confirmedAt}"""
        routes["GET $base/tx/$txid"] = 200 to
            """{"txid":"$txid","vout":[{"scriptpubkey":"0014ff","value":5},{"scriptpubkey":"0020ab","value":250000}],"status":$status}"""
        routes["GET $base/tx/$txid/outspend/1"] = 200 to
            (if (spent) """{"spent":true,"txid":"$spender","vin":0}""" else """{"spent":false}""")
        routes["GET $base/blocks/tip/height"] = 200 to "$tip"
    }

    @Test
    fun an_unspent_output_carries_confirmations_from_the_explorers_own_tip() = runBlocking {
        serve("https://one/api", confirmedAt = 100, tip = 105)
        assertEquals(TxOutInfo(250_000, "0020ab", 6), check.unspent("btc", txid, 1))
    }

    /** The output itself, spent or not (a recovered swap checks its contract by it). */
    @Test
    fun an_output_is_read_whether_or_not_it_is_spent() = runBlocking {
        serve("https://one/api", spent = true)
        assertEquals(TxOutInfo(250_000, "0020ab", 0), check.output("btc", txid, 1))
        assertNull(check.output("btc", txid, 7))
        assertNull(check.output("btc", "cc".repeat(32), 0))
    }

    @Test
    fun an_unconfirmed_output_has_no_confirmations() = runBlocking {
        serve("https://one/api", confirmedAt = null)
        assertEquals(0L, check.unspent("btc", txid, 1)!!.confirmations)
    }

    @Test
    fun a_spent_output_or_a_missing_one_is_null() = runBlocking {
        serve("https://one/api", spent = true)
        assertNull(check.unspent("btc", txid, 1))
        assertNull(check.unspent("btc", txid, 7))
    }

    /** A 404 is the explorer's answer ("not there"), not an outage. */
    @Test
    fun an_unknown_transaction_is_null_without_asking_elsewhere() = runBlocking {
        assertNull(check.unspent("btc", txid, 1))
        assertTrue(asked.none { it.contains("//two/") })
    }

    @Test
    fun the_second_explorer_answers_when_the_first_is_down() = runBlocking {
        routes["GET https://one/api/tx/$txid"] = 0 to ""
        serve("https://two/api")
        assertEquals(250_000L, check.unspent("btc", txid, 1)!!.valueSat)
    }

    @Test
    fun a_server_error_also_moves_on_to_the_next_explorer() = runBlocking {
        routes["GET https://one/api/tx/$txid"] = 503 to "busy"
        serve("https://two/api")
        assertEquals(250_000L, check.unspent("btc", txid, 1)!!.valueSat)
    }

    @Test
    fun an_answer_about_another_transaction_is_not_believed() = runBlocking {
        serve("https://one/api")
        routes["GET https://one/api/tx/$txid"] = 200 to """{"txid":"$spender","vout":[],"status":{"confirmed":false}}"""
        serve("https://two/api")
        assertEquals(250_000L, check.unspent("btc", txid, 1)!!.valueSat)
        assertTrue(asked.any { it.contains("//two/") })
    }

    @Test(expected = IOException::class)
    fun with_every_explorer_down_it_throws_rather_than_answer() {
        routes["GET https://one/api/tx/$txid"] = 0 to ""
        routes["GET https://two/api/tx/$txid"] = 0 to ""
        runBlocking { check.unspent("btc", txid, 1) }
    }

    @Test(expected = IllegalStateException::class)
    fun a_chain_without_explorers_is_an_error() {
        runBlocking { check.unspent("xbt", txid, 1) }
    }

    @Test
    fun the_spending_transaction_is_read_from_the_same_explorer() = runBlocking {
        serve("https://one/api", spent = true)
        routes["GET https://one/api/tx/$spender/hex"] = 200 to "0200feed\n"
        assertEquals("0200feed", check.spendingTxHex("btc", txid, 1))
    }

    @Test
    fun a_spend_counts_as_confirmed_only_once_mined() = runBlocking {
        routes["GET https://one/api/tx/$txid/outspend/1"] = 200 to """{"spent":true,"txid":"$spender","status":{"confirmed":false}}"""
        assertEquals(false, check.spendConfirmed("btc", txid, 1))
        routes["GET https://one/api/tx/$txid/outspend/1"] = 200 to """{"spent":true,"txid":"$spender","status":{"confirmed":true,"block_height":101}}"""
        assertEquals(true, check.spendConfirmed("btc", txid, 1))
        routes["GET https://one/api/tx/$txid/outspend/1"] = 200 to """{"spent":false}"""
        assertEquals(false, check.spendConfirmed("btc", txid, 1))
    }

    @Test
    fun no_spender_is_null() = runBlocking {
        serve("https://one/api", spent = false)
        assertNull(check.spendingTxHex("btc", txid, 1))
    }

    @Test
    fun a_broadcast_posts_the_hex_and_falls_back_when_refused() = runBlocking {
        routes["POST https://one/api/tx"] = 400 to "sendrawtransaction RPC error"
        routes["POST https://two/api/tx"] = 200 to "$txid\n"
        assertEquals(txid, check.broadcast("btc", "0200beef"))
        assertEquals(listOf("0200beef", "0200beef"), posted)
    }

    @Test
    fun the_default_explorers_are_independent_of_the_edge() {
        assertEquals(listOf("https://mempool.space/api", "https://blockstream.info/api"), EXPLORERS["btc"])
        assertEquals(listOf("https://mempool.kilombino.com/api", "https://mempool.guide/api"), EXPLORERS["xbt"])
    }
}
