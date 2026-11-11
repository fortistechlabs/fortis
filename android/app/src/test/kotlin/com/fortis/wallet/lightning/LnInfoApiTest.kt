package com.fortis.wallet.lightning

import kotlinx.coroutines.runBlocking
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Protocol
import okhttp3.Response
import okhttp3.ResponseBody.Companion.toResponseBody
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.wallet_ffi.LnChain
import uniffi.wallet_ffi.LnFfiException

/** `/ln/info` is the BTC document, plus an optional `xbt` object of the same shape. */
class LnInfoApiTest {
    private val btcNode = "02" + "11".repeat(32)
    private val xbtNode = "03" + "22".repeat(32)

    private fun lsp(node: String, host: String, enabled: Boolean = true, extra: String = "") = """
        {"node_id":"$node","host":"$host","port":9735,"lsps2_token":"t",
         "fee":{"ppm":10000,"min_sat":1000},
         "min_payment_sat":10000,"max_payment_sat":500000,
         "channel_min_sat":100000,"channel_max_sat":500000,
         "enabled":$enabled,"network":"bitcoin"$extra}
    """.trimIndent()

    private val btcOnly = lsp(btcNode, "lsp.fortis.test")

    private fun withXbt(xbt: String) = btcOnly.trimEnd().removeSuffix("}") + ""","xbt":$xbt}"""

    private val xbtObject = lsp(xbtNode, "xbt-lsp.fortis.test", enabled = false, extra = ""","message":"soon","chain":"xbt"""")

    @Test
    fun ln_info_doc_with_xbt() {
        val doc = parseLnInfoDoc(withXbt(xbtObject))
        assertEquals(btcNode, doc.btc.nodeId)
        assertEquals("lsp.fortis.test", doc.btc.host)
        assertTrue(doc.btc.enabled)
        val xbt = doc.xbt
        assertNotNull(xbt)
        assertEquals(xbtNode, xbt!!.nodeId)
        assertEquals("xbt-lsp.fortis.test", xbt.host)
        assertFalse(xbt.enabled)
        assertEquals("soon", xbt.message)
        // The BTC part is what parseLnInfo gives.
        assertEquals(parseLnInfo(btcOnly), doc.btc)
    }

    @Test
    fun ln_info_doc_without_xbt() {
        val doc = parseLnInfoDoc(btcOnly)
        assertEquals(parseLnInfo(btcOnly), doc.btc)
        assertNull(doc.xbt)
        assertNull(parseLnInfoDoc(withXbt("null")).xbt)
    }

    @Test
    fun ln_info_doc_bad_xbt_ignored() {
        val bad = listOf(
            "{}",
            "[]",
            "42",
            "\"x\"",
            xbtObject.replace(xbtNode, "04" + "22".repeat(32)),
            xbtObject.replace("\"port\":9735", "\"port\":0"),
            xbtObject.replace("\"network\":\"bitcoin\"", "\"network\":\"dogecoin\""),
        )
        for (x in bad) {
            val doc = parseLnInfoDoc(withXbt(x))
            assertEquals("BTC part must survive xbt=$x", parseLnInfo(btcOnly), doc.btc)
            assertNull("xbt=$x must be ignored", doc.xbt)
        }
    }

    @Test
    fun ln_info_doc_bad_btc_still_fails() {
        val badBtc = withXbt(xbtObject).replaceFirst("\"port\":9735", "\"port\":0")
        assertTrue(runCatching { parseLnInfoDoc(badBtc) }.exceptionOrNull() is IllegalArgumentException)
    }

    private fun api(body: String, chain: LnChain): EdgeLnInfoApi {
        val http = OkHttpClient.Builder().addInterceptor { chain0 ->
            Response.Builder()
                .request(chain0.request())
                .protocol(Protocol.HTTP_1_1)
                .code(200)
                .message("OK")
                .body(body.toResponseBody("application/json".toMediaType()))
                .build()
        }.build()
        return EdgeLnInfoApi(http, "https://edge.test", chain) { "tok" }
    }

    @Test
    fun edge_info_api_picks_the_chain() = runBlocking {
        val body = withXbt(xbtObject)
        assertEquals(btcNode, api(body, LnChain.BTC).fetch().nodeId)
        assertEquals(xbtNode, api(body, LnChain.XBT).fetch().nodeId)
    }

    @Test
    fun edge_info_api_xbt_missing_is_lsp_unreachable() = runBlocking {
        val e = runCatching { api(btcOnly, LnChain.XBT).fetch() }.exceptionOrNull()
        assertTrue("got $e", e is LnFfiException.LspUnreachable)
        // A bad xbt object never breaks the BTC wallet.
        assertEquals(btcNode, api(withXbt("{}"), LnChain.BTC).fetch().nodeId)
    }
}
