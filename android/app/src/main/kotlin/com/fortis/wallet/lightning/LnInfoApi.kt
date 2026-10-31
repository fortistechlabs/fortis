package com.fortis.wallet.lightning

import com.fortis.wallet.HOSTED_EDGE
import com.fortis.wallet.data.send
import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONObject
import uniffi.wallet_ffi.LnFfiException
import uniffi.wallet_ffi.LnLspInfo
import uniffi.wallet_ffi.LnLspLimits

data class LnInfo(
    val nodeId: String,
    val host: String,
    val port: Int,
    val lsps2Token: String?,
    val feePpm: Long,
    val feeMinSat: Long,
    val minPaymentSat: Long,
    val maxPaymentSat: Long,
    val channelMinSat: Long,
    val channelMaxSat: Long,
    val enabled: Boolean = true,
    val message: String? = null,
    val network: String = "bitcoin",
) {
    fun toLspInfo(): LnLspInfo = LnLspInfo(
        nodeId = nodeId,
        host = host,
        port = port.toUShort(),
        token = lsps2Token,
    )

    fun toLspLimits(): LnLspLimits = LnLspLimits(
        minPaymentMsat = minPaymentSat.toULong() * 1_000uL,
        maxPaymentMsat = maxPaymentSat.toULong() * 1_000uL,
        channelMinSat = channelMinSat.toULong(),
        channelMaxSat = channelMaxSat.toULong(),
    )
}

private val ALLOWED_NETWORKS = setOf("bitcoin", "mainnet", "testnet", "signet", "regtest")

fun parseLnInfo(json: String): LnInfo = parseLnInfo(JSONObject(json))

fun parseLnInfo(o: JSONObject): LnInfo {
    val rawNodeId = o.getString("node_id").trim().lowercase()
    val isHexPubkey = rawNodeId.length == 66 &&
        (rawNodeId.startsWith("02") || rawNodeId.startsWith("03")) &&
        rawNodeId.all { it in '0'..'9' || it in 'a'..'f' }
    require(isHexPubkey) { "node_id must be a 66-hex compressed public key" }

    val host = o.getString("host").trim()
    val hostOk = host.isNotEmpty() &&
        host.length <= 253 &&
        !host.contains("://") &&
        host.all { it.isLetterOrDigit() || it in ".-:[]" }
    require(hostOk) { "invalid host" }

    val port = o.getInt("port")
    require(port in 1..65535) { "port out of range: $port" }

    val lsps2Token = if (o.has("lsps2_token") && !o.isNull("lsps2_token")) {
        val t = o.getString("lsps2_token")
        require(t.isNotEmpty() && t.length <= 512 && t.none { it.isISOControl() }) { "invalid lsps2_token" }
        t
    } else {
        null
    }

    val fee = o.getJSONObject("fee")
    val feePpm = fee.getLong("ppm")
    val feeMinSat = fee.getLong("min_sat")
    require(feePpm in 0..100_000) { "fee.ppm out of range: $feePpm" }
    require(feeMinSat >= 0) { "fee.min_sat must be non-negative" }

    val minPaymentSat = o.getLong("min_payment_sat")
    val maxPaymentSat = o.getLong("max_payment_sat")
    require(minPaymentSat >= 0 && minPaymentSat <= maxPaymentSat) { "invalid payment bounds" }

    val channelMinSat = o.getLong("channel_min_sat")
    val channelMaxSat = o.getLong("channel_max_sat")
    require(channelMinSat > 0 && channelMinSat <= channelMaxSat) { "invalid channel bounds" }

    val enabled = if (o.has("enabled") && !o.isNull("enabled")) o.getBoolean("enabled") else true
    val message = if (o.has("message") && !o.isNull("message")) {
        val m = o.getString("message")
        require(m.length <= 1000) { "message too long" }
        m.ifBlank { null }
    } else {
        null
    }
    val network = if (o.has("network") && !o.isNull("network")) o.getString("network").lowercase() else "bitcoin"
    require(network in ALLOWED_NETWORKS) { "unsupported network: $network" }

    return LnInfo(
        nodeId = rawNodeId,
        host = host,
        port = port,
        lsps2Token = lsps2Token,
        feePpm = feePpm,
        feeMinSat = feeMinSat,
        minPaymentSat = minPaymentSat,
        maxPaymentSat = maxPaymentSat,
        channelMinSat = channelMinSat,
        channelMaxSat = channelMaxSat,
        enabled = enabled,
        message = message,
        network = network,
    )
}

fun interface LnInfoApi {
    suspend fun fetch(): LnInfo
}

class EdgeLnInfoApi(
    private val http: OkHttpClient,
    base: String = HOSTED_EDGE,
    private val token: suspend () -> String,
) : LnInfoApi {
    private val base = base.trimEnd('/')

    override suspend fun fetch(): LnInfo {
        val auth = token()
        val req = Request.Builder()
            .url("$base/ln/info")
            .header("Authorization", "Bearer $auth")
            .get()
            .build()
        return http.send(req) { r ->
            val body = r.body?.string().orEmpty()
            if (!r.isSuccessful) {
                throw LnFfiException.LspUnreachable()
            }
            try {
                parseLnInfo(body)
            } catch (e: Exception) {
                throw LnFfiException.LspUnreachable()
            }
        }
    }
}
