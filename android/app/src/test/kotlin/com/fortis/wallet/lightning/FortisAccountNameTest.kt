package com.fortis.wallet.lightning

import kotlinx.coroutines.runBlocking
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class FortisAccountNameTest {
    private val signer = object : AccountSigner {
        override fun nodeId() = "02aa"
        override fun sign(body: String) = "sig"
    }

    private class Fake(val status: Int, val text: String) : AccountTransport {
        val bodies = mutableListOf<JSONObject>()
        override suspend fun post(body: String, sig: String): Pair<Int, String> {
            bodies += JSONObject(body)
            return status to text
        }
    }

    private fun acct(t: AccountTransport) = FortisAccount(t) { 1_000L }

    private suspend fun failure(block: suspend () -> Unit): AccountError =
        try { block(); fail("expected an AccountError"); throw AssertionError() } catch (e: AccountError) { e }

    @Test
    fun myNameParsesAndSendsTheOp() = runBlocking {
        val t = Fake(200, """{"name":"donal"}""")
        assertEquals("donal", acct(t).myName(signer, "xbt"))
        assertEquals("my_name", t.bodies.single().getString("op"))
        // The wallet's chain is signed with the request, so it cannot be replayed to the other chain's server.
        assertEquals("xbt", t.bodies.single().getJSONObject("params").getString("chain"))
        assertEquals(1, t.bodies.single().getJSONObject("params").length())
    }

    @Test
    fun myNameNull() = runBlocking {
        assertNull(acct(Fake(200, """{"name":null}""")).myName(signer, "xbt"))
    }

    @Test
    fun claimSendsTheNameAndReturnsIt() = runBlocking {
        val t = Fake(200, """{"name":"donal"}""")
        assertEquals("donal", acct(t).claimName(signer, "xbt", "donal"))
        assertEquals("claim_name", t.bodies.single().getString("op"))
        assertEquals("donal", t.bodies.single().getJSONObject("params").getString("name"))
        assertEquals("xbt", t.bodies.single().getJSONObject("params").getString("chain"))
    }

    @Test
    fun claimTaken() = runBlocking {
        val e = failure { acct(Fake(409, """{"error":"name_taken"}""")).claimName(signer, "xbt", "donal") }
        assertTrue(e is AccountError.NameTaken)
    }

    @Test
    fun claimHasName() = runBlocking {
        val e = failure { acct(Fake(409, """{"error":"has_name"}""")).claimName(signer, "xbt", "donal") }
        assertTrue(e is AccountError.HasName)
    }

    @Test
    fun claimInvalidEachReason() = runBlocking {
        for (r in listOf("length", "characters", "edges", "double_dot", "reserved", "missing")) {
            val e = failure { acct(Fake(400, """{"error":"name_invalid","reason":"$r"}""")).claimName(signer, "xbt", "x") }
            assertEquals(r, (e as AccountError.NameInvalid).reason)
        }
    }

    @Test
    fun claimAuthAndServerErrorsKeepTheirMapping() = runBlocking {
        assertTrue(failure { acct(Fake(401, """{"reason":"expired"}""")).claimName(signer, "xbt", "donal") } is AccountError.Auth)
        assertTrue(failure { acct(Fake(404, "")).claimName(signer, "xbt", "donal") } is AccountError.Rejected)
        assertTrue(failure { acct(Fake(503, "")).claimName(signer, "xbt", "donal") } is AccountError.Network)
    }

    @Test
    fun releaseReturnsTheNameOrNull() = runBlocking {
        val t = Fake(200, """{"released":"donal"}""")
        assertEquals("donal", acct(t).releaseName(signer, "xbt"))
        assertEquals("xbt", t.bodies.single().getJSONObject("params").getString("chain"))
        assertEquals("release_name", t.bodies.single().getString("op"))
        assertNull(acct(Fake(200, """{"released":null}""")).releaseName(signer, "xbt"))
    }
}
