package com.fortis.wallet.data

import kotlinx.coroutines.runBlocking
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Protocol
import okhttp3.Response
import okhttp3.ResponseBody.Companion.toResponseBody
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.IOException
import java.net.SocketTimeoutException

class OwnServerTest {
    /** "METHOD url" → (code, body); code 0 = connection failure, -1 = timeout. Unlisted: 404. */
    private val routes = mutableMapOf<String, Pair<Int, String>>()
    private val asked = mutableListOf<String>()

    private val http = OkHttpClient.Builder().addInterceptor { chain ->
        val req = chain.request()
        val key = "${req.method} ${req.url}"
        asked += key
        val (code, body) = routes[key] ?: (404 to "Not found")
        if (code == 0) throw IOException("connection refused")
        if (code == -1) throw SocketTimeoutException("timeout")
        Response.Builder().request(req).protocol(Protocol.HTTP_1_1).code(code).message("-")
            .body(body.toResponseBody("text/plain".toMediaType())).build()
    }.build()

    private val btcHash = FORK_CHECK_HASH.getValue("btc")
    private val xbtHash = FORK_CHECK_HASH.getValue("xbt")

    private fun serve(base: String, tip: String = "972599", hash: String = btcHash) {
        routes["GET $base/blocks/tip/height"] = 200 to tip
        routes["GET $base/block-height/961640"] = 200 to hash
    }

    private fun check(chain: String, url: String) = runBlocking { validateOwnServer(http, chain, url) }

    @Test
    fun a_valid_btc_server_is_ok() {
        serve("https://n/api")
        assertEquals(OwnServerCheck.Ok("https://n/api"), check("btc", "https://n/api"))
    }

    @Test
    fun a_valid_xbt_server_is_ok() {
        serve("https://n/api", hash = xbtHash)
        assertEquals(OwnServerCheck.Ok("https://n/api"), check("xbt", "https://n/api"))
    }

    @Test
    fun http_is_rejected_without_a_request() {
        serve("http://n/api")
        assertEquals(OwnServerCheck.NotHttps, check("btc", "http://n/api"))
        assertTrue(asked.isEmpty())
    }

    @Test
    fun the_other_chains_hash_is_wrong_chain() {
        serve("https://n/api", hash = xbtHash)
        assertEquals(OwnServerCheck.WrongChain, check("btc", "https://n/api"))
    }

    @Test
    fun a_tip_below_the_fork_is_not_synced() {
        serve("https://n/api", tip = "961000")
        assertEquals(OwnServerCheck.NotSynced(961_000), check("btc", "https://n/api"))
    }

    @Test
    fun an_html_tip_is_not_esplora() {
        serve("https://n", tip = "<html>hi</html>")
        serve("https://n/api", tip = "<html>hi</html>")
        assertEquals(OwnServerCheck.NotEsplora, check("btc", "https://n"))
    }

    @Test
    fun a_connection_failure_is_unreachable() {
        routes["GET https://n/blocks/tip/height"] = 0 to ""
        routes["GET https://n/api/blocks/tip/height"] = 0 to ""
        assertEquals(OwnServerCheck.Unreachable, check("btc", "https://n"))
    }

    @Test
    fun a_bare_host_gets_api_appended() {
        serve("https://n/api")
        assertEquals(OwnServerCheck.Ok("https://n/api"), check("btc", "https://n"))
        assertEquals("GET https://n/blocks/tip/height", asked.first())
    }

    @Test
    fun trims_whitespace_and_trailing_slash() {
        serve("https://n/api")
        assertEquals(OwnServerCheck.Ok("https://n/api"), check("btc", "  https://n/api/ "))
    }

    @Test
    fun tip_with_trailing_newline_is_accepted() {
        serve("https://n/api", tip = "972599\n")
        assertEquals(OwnServerCheck.Ok("https://n/api"), check("btc", "https://n/api"))
    }

    @Test
    fun hash_compare_ignores_case_and_whitespace() {
        serve("https://n/api", hash = btcHash.uppercase() + "\n")
        assertEquals(OwnServerCheck.Ok("https://n/api"), check("btc", "https://n/api"))
    }

    @Test
    fun timeout_is_unreachable() {
        routes["GET https://n/api/blocks/tip/height"] = -1 to ""
        assertEquals(OwnServerCheck.Unreachable, check("btc", "https://n/api"))
    }

    /** Saved in OkHttp's canonical form, so every later parser of it agrees. */
    @Test
    fun the_saved_url_is_canonical() {
        serve("https://node.example/api")
        assertEquals(OwnServerCheck.Ok("https://node.example/api"), check("btc", "https://Node.Example/api"))
    }

    @Test
    fun a_host_with_an_underscore_is_read() {
        assertEquals("my_node.lan", ownServerHost("https://my_node.lan/api"))
    }

    @Test
    fun a_url_that_isnt_one_has_no_host() {
        assertEquals(null, ownServerHost("not a url"))
    }

    @Test
    fun an_unknown_chain_is_wrong_chain() {
        serve("https://n/api")
        assertEquals(OwnServerCheck.WrongChain, check("ltc", "https://n/api"))
    }
}
