package com.fortis.wallet.lightning

import kotlinx.coroutines.runBlocking
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class AccountRoutingTest {
    @Test
    fun receivesGoToTheChannelThenAJitChannelThenTheAccount() {
        assertEquals(ReceiveRoute.Account, receiveRoute(100_000uL, 0uL, 51_000_000uL))
        assertEquals(ReceiveRoute.JitChannel, receiveRoute(60_000_000uL, 0uL, 51_000_000uL))
        assertEquals(ReceiveRoute.OwnInvoice, receiveRoute(100_000uL, 500_000uL, 51_000_000uL))
        // A full channel: small amounts go to the account again.
        assertEquals(ReceiveRoute.Account, receiveRoute(600_000uL, 500_000uL, 51_000_000uL))
    }

    @Test
    fun sendsUseTheChannelFirstThenTheAccount() {
        assertEquals(PayRoute.Account, payRoute(1_000uL, 50_000uL, 0uL, 200_000uL))
        assertEquals(PayRoute.Channel, payRoute(1_000uL, 50_000uL, 100_000uL, 0uL))
        assertEquals(PayRoute.Channel, payRoute(1_000uL, 50_000uL, 100_000uL, 999_999uL))
        assertEquals(PayRoute.Insufficient, payRoute(1_000uL, 50_000uL, 0uL, 10_000uL))
    }

    private class Signer : AccountSigner {
        var signed: String? = null
        override fun nodeId() = "02" + "11".repeat(32)
        override fun sign(body: String): String { signed = body; return "zbase32sig" }
    }

    @Test
    fun requestsAreSignedAndSentByteForByte() = runBlocking {
        var sentBody: String? = null
        var sentSig: String? = null
        val acct = FortisAccount({ body, sig -> sentBody = body; sentSig = sig; 200 to """{"balance_msat":5000,"held_msat":1000,"max_msat":50000000}""" }, { 1_800_000_000L })
        val s = Signer()
        val b = acct.balance(s)
        assertEquals(AccountBalance(5_000uL, 1_000uL, 50_000_000uL), b)
        assertEquals(4_000uL, b.freeMsat)
        assertEquals(s.signed, sentBody)
        assertEquals("zbase32sig", sentSig)
        val o = JSONObject(sentBody!!)
        assertEquals("balance", o.getString("op"))
        assertEquals(s.nodeId(), o.getString("node_id"))
        assertEquals(1_800_000_120L, o.getLong("expires_at"))
        assertTrue(o.getString("nonce").length == 32)
    }

    @Test
    fun errorsMapToTheirMeaning() = runBlocking {
        fun acct(status: Int, body: String) = FortisAccount({ _, _ -> status to body })
        try {
            acct(409, """{"error":"custody_full","min_channel_sat":11000}""").invoice(Signer(), 1_000uL, "")
            fail()
        } catch (e: AccountError.CustodyFull) {
            assertEquals(11_000L, e.minChannelSat)
        }
        try { acct(402, """{"error":"insufficient"}""").pay(Signer(), "lnbc1", null); fail() } catch (_: AccountError.Insufficient) {}
        try { acct(401, """{"error":"auth","reason":"replayed"}""").balance(Signer()); fail() } catch (_: AccountError.Auth) {}
        try { acct(502, "{}").balance(Signer()); fail() } catch (_: AccountError.Network) {}
    }
}
