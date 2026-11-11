package com.fortis.wallet.lightning

import kotlinx.coroutines.runBlocking
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Protocol
import okhttp3.Request
import okhttp3.Response
import okhttp3.ResponseBody.Companion.toResponseBody
import org.junit.Assert.assertEquals
import org.junit.Test
import uniffi.wallet_ffi.LnChain

/** The Fortis account goes to `/ln/account` (BTC) or `/ln/account?chain=xbt`. */
class FortisAccountTest {
    private fun capturingHttp(seen: MutableList<Request>) = OkHttpClient.Builder().addInterceptor { c ->
        seen += c.request()
        Response.Builder()
            .request(c.request())
            .protocol(Protocol.HTTP_1_1)
            .code(200)
            .message("OK")
            .body("{}".toResponseBody("application/json".toMediaType()))
            .build()
    }.build()

    @Test
    fun account_url_has_chain_xbt() = runBlocking {
        val seen = mutableListOf<Request>()
        EdgeAccountTransport(capturingHttp(seen), "https://edge.test/", LnChain.XBT) { "tok" }.post("{}", "sig")
        assertEquals("https://edge.test/ln/account?chain=xbt", seen.single().url.toString())
        assertEquals("POST", seen.single().method)
        assertEquals("sig", seen.single().header("X-Fortis-Sig"))
    }

    @Test
    fun account_url_btc_unchanged() = runBlocking {
        val seen = mutableListOf<Request>()
        EdgeAccountTransport(capturingHttp(seen), "https://edge.test", LnChain.BTC) { "tok" }.post("{}", "sig")
        assertEquals("https://edge.test/ln/account", seen.single().url.toString())
        assertEquals("Bearer tok", seen.single().header("Authorization"))
    }
}
