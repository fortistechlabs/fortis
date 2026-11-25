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
    fun anOfferFromTheChannelMovesToTheAccountFirstWithinTheCap() {
        // 121,000 msat: the reserve is 1 % plus 50 sat.
        assertEquals(51_210uL, offerReserveMsat(121_000uL))
        val max = 50_000_000uL
        // The channel can pay it, the account cannot: via the account.
        assertEquals(
            PayRoute.ChannelViaAccount,
            payRoute(121_000uL, 5_000uL, 63_000_000uL, 0uL, isOffer = true, accountMaxMsat = max, accountBalanceMsat = 0uL),
        )
        // Not for a plain invoice.
        assertEquals(
            PayRoute.Channel,
            payRoute(121_000uL, 5_000uL, 63_000_000uL, 0uL, isOffer = false, accountMaxMsat = max, accountBalanceMsat = 0uL),
        )
        // Over the account's cap: the channel route (as before).
        assertEquals(
            PayRoute.Channel,
            payRoute(49_800_000uL, 5_000uL, 63_000_000uL, 0uL, isOffer = true, accountMaxMsat = max, accountBalanceMsat = 0uL),
        )
        // The channel cannot cover amount + reserve: not via the account.
        assertEquals(
            PayRoute.Channel,
            payRoute(121_000uL, 5_000uL, 150_000uL, 0uL, isOffer = true, accountMaxMsat = max, accountBalanceMsat = 0uL),
        )
        // The account already covers it: straight from the account.
        assertEquals(
            PayRoute.Account,
            payRoute(121_000uL, 5_000uL, 63_000_000uL, 1_000_000uL, isOffer = true, accountMaxMsat = max, accountBalanceMsat = 1_000_000uL),
        )
    }

    @Test
    fun sendsUseTheChannelFirstThenTheAccount() {
        assertEquals(PayRoute.Account, payRoute(1_000uL, 50_000uL, 0uL, 200_000uL))
        assertEquals(PayRoute.Channel, payRoute(1_000uL, 50_000uL, 100_000uL, 0uL))
        assertEquals(PayRoute.Channel, payRoute(1_000uL, 50_000uL, 100_000uL, 999_999uL))
        assertEquals(PayRoute.Insufficient, payRoute(1_000uL, 50_000uL, 0uL, 10_000uL))
        // A BOLT12 offer goes through the account when it can cover it, even if the channel could too.
        assertEquals(PayRoute.Account, payRoute(1_000uL, 50_000uL, 100_000uL, 200_000uL, isOffer = true))
        assertEquals(PayRoute.Channel, payRoute(1_000uL, 50_000uL, 100_000uL, 0uL, isOffer = true))
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

    private fun rcv(sat: Long, hash: String?) =
        AccountEntry(atSecs = 1, kind = "received", amountMsat = sat * 1_000, feeMsat = 0, paymentHash = hash)

    private fun out(id: String, sat: Long, at: Long, status: uniffi.wallet_ffi.LnPaymentStatus) = uniffi.wallet_ffi.LnPayment(
        id = id, direction = uniffi.wallet_ffi.LnDirection.OUT, amountMsat = (sat * 1_000).toULong(), feeMsat = 0uL,
        status = status, description = "", timestamp = at.toULong(),
    )

    @Test
    fun aTopUpWhoseOutcomeWasUnknownIsFoundByAmountAndStartTime() {
        val pending = uniffi.wallet_ffi.LnPaymentStatus.PENDING
        val ok = uniffi.wallet_ffi.LnPaymentStatus.SUCCEEDED
        val failed = uniffi.wallet_ffi.LnPaymentStatus.FAILED
        val pay = listOf(
            out("older", 172, 50, ok),            // same amount, before this move started
            out("other-amount", 173, 101, ok),
            out("failed", 172, 101, failed),      // moved nothing: not remembered
            out("known", 172, 102, ok),           // already remembered
            out("mine", 172, 103, pending),
        )
        assertEquals("mine", findTopUpPayment(pay, 172_000uL, sinceSecs = 100L, known = setOf("known")))
        assertEquals(null, findTopUpPayment(pay, 999_000uL, sinceSecs = 100L, known = emptySet()))
        // Of several candidates the newest.
        assertEquals("known", findTopUpPayment(pay.dropLast(1), 172_000uL, sinceSecs = 100L, known = emptySet()))
    }

    @Test
    fun onlyTheCreditOfOurOwnMoveIsHiddenNotOtherReceiptsDuringASend() {
        // Our move: known by its payment hash, or by its exact amount while it is being paid.
        assertEquals(true, isOwnTopUpReceipt(rcv(172, "top"), setOf("top"), null))
        assertEquals(true, isOwnTopUpReceipt(rcv(172, "x"), emptySet(), 172_000uL))
        // Someone else paying us, even at that moment, is announced.
        assertEquals(false, isOwnTopUpReceipt(rcv(500, "other"), setOf("top"), 172_000uL))
        assertEquals(false, isOwnTopUpReceipt(rcv(172, "other"), emptySet(), null))
    }
}
