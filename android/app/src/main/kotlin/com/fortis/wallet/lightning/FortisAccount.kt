package com.fortis.wallet.lightning

import com.fortis.wallet.HOSTED_EDGE
import com.fortis.wallet.data.send
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray
import org.json.JSONObject
import java.security.SecureRandom

/**
 * The Fortis account (spec §7): small balances the LSP holds for this wallet,
 * keyed by its node id. Every request is signed by the node key; the edge
 * passes it to the LSP (`POST /ln/account`).
 */

data class AccountBalance(val balanceMsat: ULong, val heldMsat: ULong, val maxMsat: ULong) {
    val freeMsat: ULong get() = if (balanceMsat > heldMsat) balanceMsat - heldMsat else 0uL
}

data class AccountEntry(
    val atSecs: Long,
    /** "received", "sent", "internal", "converted" or "withdrawn". */
    val kind: String,
    /** Signed: positive into the account. */
    val amountMsat: Long,
    val feeMsat: Long,
    val paymentHash: String?,
)

data class AccountPayResult(val internal: Boolean, val paymentHash: String)

sealed class AccountError(msg: String) : Exception(msg) {
    /** Fortis holds as much as it will; a receive of [minChannelSat]+ opens a channel instead. */
    class CustodyFull(val minChannelSat: Long) : AccountError("custody_full")
    class Insufficient : AccountError("insufficient")
    class Auth(reason: String) : AccountError("auth: $reason")
    class Rejected(reason: String) : AccountError(reason)
    class Network(msg: String) : AccountError(msg)
}

/** The node's identity and signature over a request body. */
interface AccountSigner {
    fun nodeId(): String
    fun sign(body: String): String
}

fun interface AccountTransport {
    /** Posts [body] with [sig]; the status and the response body. */
    suspend fun post(body: String, sig: String): Pair<Int, String>
}

class EdgeAccountTransport(
    private val http: OkHttpClient,
    base: String = HOSTED_EDGE,
    private val token: suspend () -> String,
) : AccountTransport {
    private val url = base.trimEnd('/') + "/ln/account"

    override suspend fun post(body: String, sig: String): Pair<Int, String> {
        val req = Request.Builder()
            .url(url)
            .header("Authorization", "Bearer ${token()}")
            .header("X-Fortis-Sig", sig)
            .post(body.toRequestBody("application/json".toMediaType()))
            .build()
        return try {
            http.send(req) { r -> r.code to (r.body?.string().orEmpty()) }
        } catch (e: java.io.IOException) {
            throw AccountError.Network(e.message ?: "network")
        }
    }
}

private val rng = SecureRandom()
private const val NONCE_CHARS = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"

fun newNonce(): String = buildString { repeat(32) { append(NONCE_CHARS[rng.nextInt(NONCE_CHARS.length)]) } }

/** The request body the node signs (and the LSP checks, byte for byte). */
fun buildAccountRequest(nodeId: String, op: String, params: JSONObject, nonce: String, expiresAt: Long): String =
    JSONObject()
        .put("node_id", nodeId)
        .put("op", op)
        .put("params", params)
        .put("nonce", nonce)
        .put("expires_at", expiresAt)
        .toString()

class FortisAccount(
    private val transport: AccountTransport,
    private val nowSecs: () -> Long = { System.currentTimeMillis() / 1000 },
) {
    private suspend fun call(signer: AccountSigner, op: String, params: JSONObject): String {
        val body = buildAccountRequest(signer.nodeId(), op, params, newNonce(), nowSecs() + 120)
        val (status, text) = transport.post(body, signer.sign(body))
        if (status in 200..299) return text
        val o = runCatching { JSONObject(text) }.getOrNull()
        throw when {
            status == 409 && o?.optString("error") == "custody_full" -> AccountError.CustodyFull(o.optLong("min_channel_sat"))
            status == 402 -> AccountError.Insufficient()
            status == 401 -> AccountError.Auth(o?.optString("reason").orEmpty())
            status in 400..499 -> AccountError.Rejected(o?.optString("error")?.ifBlank { null } ?: "http $status")
            else -> AccountError.Network("http $status")
        }
    }

    suspend fun balance(signer: AccountSigner): AccountBalance {
        val o = JSONObject(call(signer, "balance", JSONObject()))
        return AccountBalance(
            o.optLong("balance_msat").toULong(),
            o.optLong("held_msat").toULong(),
            o.optLong("max_msat").toULong(),
        )
    }

    /** A BOLT11 invoice paying into this account. */
    suspend fun invoice(signer: AccountSigner, amountMsat: ULong, description: String): String {
        val p = JSONObject().put("amount_msat", amountMsat.toLong()).put("description", description)
        return JSONObject(call(signer, "invoice", p)).getString("invoice")
    }

    /** Pays a BOLT11 invoice from this account (`amountMsat` for an amountless one). */
    suspend fun pay(signer: AccountSigner, invoice: String, amountMsat: ULong?): AccountPayResult {
        val p = JSONObject().put("invoice", invoice)
        if (amountMsat != null) p.put("amount_msat", amountMsat.toLong())
        val o = JSONObject(call(signer, "pay", p))
        return AccountPayResult(o.optBoolean("internal"), o.optString("payment_hash"))
    }

    /** An invoice into this wallet's own channel that the LSP holds until the phone can take it. */
    suspend fun channelInvoice(signer: AccountSigner, amountMsat: ULong, description: String): String {
        val p = JSONObject().put("amount_msat", amountMsat.toLong()).put("description", description)
        return JSONObject(call(signer, "channel_invoice", p)).getString("invoice")
    }

    /** Registers this phone's push [token] so the LSP can wake it for a payment held while it is offline. */
    suspend fun pushRegister(signer: AccountSigner, token: String) {
        call(signer, "push_register", JSONObject().put("token", token))
    }

    /** The account's reusable BOLT12 offer (`lno1…`), hosted by the LSP. */
    suspend fun offer(signer: AccountSigner): String =
        JSONObject(call(signer, "offer", JSONObject())).getString("offer")

    /** Moves [amountMsat] (fee included) out of the account to the on-chain [address]; the txid. */
    suspend fun withdraw(signer: AccountSigner, amountMsat: ULong, address: String, feerateSatVb: Long): String {
        val p = JSONObject().put("amount_msat", amountMsat.toLong()).put("address", address).put("feerate_sat_vb", feerateSatVb)
        return JSONObject(call(signer, "withdraw", p)).getString("txid")
    }

    /** An invoice for the operator's share of a transfer fee (paid to the LSP, credited to no account). */
    suspend fun feeInvoice(signer: AccountSigner, amountMsat: ULong): String =
        JSONObject(call(signer, "fee_invoice", JSONObject().put("amount_msat", amountMsat.toLong()))).getString("invoice")

    suspend fun history(signer: AccountSigner, limit: Int = 50): List<AccountEntry> {
        val a = JSONArray(call(signer, "history", JSONObject().put("limit", limit)))
        return (0 until a.length()).map { i ->
            val e = a.getJSONObject(i)
            AccountEntry(
                atSecs = e.optLong("at_secs"),
                kind = e.optString("kind"),
                amountMsat = e.optLong("amount_msat"),
                feeMsat = e.optLong("fee_msat"),
                paymentHash = e.optString("payment_hash").ifBlank { null }.takeIf { !e.isNull("payment_hash") },
            )
        }
    }
}
