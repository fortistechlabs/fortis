package com.fortis.wallet.swap

import com.fortis.wallet.data.SwapKeystore
import com.fortis.wallet.data.SwapRecord
import uniffi.wallet_ffi.FfiPresignedRedeem
import uniffi.wallet_ffi.HtlcInfo
import uniffi.wallet_ffi.SwapPubkeys
import uniffi.wallet_ffi.SwapSession
import uniffi.wallet_ffi.participantFundLatest as ffiParticipantFundLatest
import uniffi.wallet_ffi.validateSwapTerms

/**
 * The part of wallet-ffi's `SwapSession` the engine uses — a seam, so the engine's
 * JVM tests (which can't load the native library) run against a fake. Methods
 * throw on refusal, as the FFI does.
 */
interface SwapCrypto : AutoCloseable {
    /** `wallet_core::validate_terms` against our own price. Throws when rejected. */
    fun validateTerms(termsJson: String, ourPrice: Long, slippageBps: Int, role: String, now: Long)

    /** Latest unix time the participant may still fund (`participant_fund_latest`). */
    fun participantFundLatest(termsJson: String): Long

    fun ourPubkeys(): SwapPubkeys

    /** Hex hashlock; the initiator's only. */
    fun hashlock(): String?

    /** Hex preimage; the initiator's only. */
    fun preimage(): String?
    fun setCounterparty(their: SwapPubkeys, hashlockHex: String)
    fun ourHtlc(): HtlcInfo
    fun theirHtlc(): HtlcInfo

    /** Exactly their HTLC's script and amount, or throws. */
    fun verifyTheirFunding(scriptPubkeyHex: String, valueSat: Long)
    fun recordTheirOutpoint(txid: String, vout: Int)
    fun recordOurOutpoint(txid: String, vout: Int)
    fun signedRefund(feerates: List<Long>): List<String>
    fun signedRedeem(preimageHex: String, feerates: List<Long>): List<String>
    fun presignedRedeem(feerates: List<Long>): List<FfiPresignedRedeem>
    fun preimageFromTx(txHex: String): String?

    /** Everything but the secrets, for `SwapRecord.sessionJson`. */
    fun toJson(): String

    /** Release native resources; the engine calls it after each step. */
    override fun close() {}
}

/**
 * [SwapCrypto] over the real `SwapSession`. The session is opened on first use:
 * resumed from `sessionJson` if the record has one, otherwise built from its
 * terms, role and addresses. The secrets are unwrapped from [SwapKeystore] only
 * then.
 */
class FfiSwapCrypto(
    private val r: SwapRecord,
    private val secrets: () -> Pair<ByteArray, ByteArray> = {
        SwapKeystore.unwrap(r.wrappedXbt) to SwapKeystore.unwrap(r.wrappedBtc)
    },
) : SwapCrypto {
    private val lazySession = lazy {
        val (xbt, btc) = secrets()
        try {
            r.sessionJson?.let { SwapSession.fromJson(it, xbt, btc) }
                ?: SwapSession(
                    r.termsJson ?: error("no terms yet"), r.role ?: error("no role yet"),
                    xbt, btc, r.redeemAddress, r.refundAddress,
                )
        } finally {
            xbt.fill(0); btc.fill(0)
        }
    }
    private val session: SwapSession by lazySession

    override fun close() {
        if (lazySession.isInitialized()) session.close()
    }

    override fun validateTerms(termsJson: String, ourPrice: Long, slippageBps: Int, role: String, now: Long) =
        validateSwapTerms(termsJson, ourPrice.toULong(), slippageBps.toUInt(), role, now.toUInt())

    override fun participantFundLatest(termsJson: String): Long = ffiParticipantFundLatest(termsJson).toLong()

    override fun ourPubkeys(): SwapPubkeys = session.ourPubkeys()
    override fun hashlock(): String? = session.hashlock()
    override fun preimage(): String? = session.preimage()
    override fun setCounterparty(their: SwapPubkeys, hashlockHex: String) = session.setCounterparty(their, hashlockHex)
    override fun ourHtlc(): HtlcInfo = session.ourHtlc()
    override fun theirHtlc(): HtlcInfo = session.theirHtlc()

    override fun verifyTheirFunding(scriptPubkeyHex: String, valueSat: Long) =
        session.verifyTheirFunding(scriptPubkeyHex, valueSat.toULong())

    override fun recordTheirOutpoint(txid: String, vout: Int) = session.recordTheirOutpoint(txid, vout.toUInt())
    override fun recordOurOutpoint(txid: String, vout: Int) = session.recordOurOutpoint(txid, vout.toUInt())
    override fun signedRefund(feerates: List<Long>): List<String> = session.signedRefund(feerates.map { it.toULong() })

    override fun signedRedeem(preimageHex: String, feerates: List<Long>): List<String> =
        session.signedRedeem(preimageHex, feerates.map { it.toULong() })

    override fun presignedRedeem(feerates: List<Long>): List<FfiPresignedRedeem> =
        session.presignedRedeem(feerates.map { it.toULong() })

    override fun preimageFromTx(txHex: String): String? = session.preimageFromTx(txHex)
    override fun toJson(): String = session.toJson()
}
