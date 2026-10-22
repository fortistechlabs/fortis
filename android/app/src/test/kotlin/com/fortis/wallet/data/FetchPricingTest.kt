package com.fortis.wallet.data

import kotlinx.coroutines.runBlocking
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Protocol
import okhttp3.Response
import okhttp3.ResponseBody.Companion.toResponseBody
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class FetchPricingTest {
    private var reply = 200 to ""

    private val http = OkHttpClient.Builder().addInterceptor { chain ->
        Response.Builder().request(chain.request()).protocol(Protocol.HTTP_1_1).code(reply.first).message("-")
            .body(reply.second.toResponseBody("application/json".toMediaType())).build()
    }.build()

    @Test
    fun the_edges_pricing_is_parsed() = runBlocking {
        reply = 200 to """{"address":"bc1qfee","bps":50,"floor_sat":400,"cap_sat":0}"""
        assertEquals(ServicePricing("bc1qfee", 50, 400, 0), fetchPricing(http, "https://edge/pricing"))
    }

    @Test
    fun no_service_fee_is_null() = runBlocking {
        reply = 404 to """{"error":"no service fee"}"""
        assertNull(fetchPricing(http, "https://edge/pricing"))
    }

    @Test
    fun a_garbled_answer_is_null() = runBlocking {
        reply = 200 to "<html>"
        assertNull(fetchPricing(http, "https://edge/pricing"))
    }
}
