package com.fortis.wallet.data

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import okhttp3.MediaType.Companion.toMediaTypeOrNull
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray
import org.json.JSONObject
import uniffi.wallet_ffi.WalletUtxo

/** fortisd gateway (your own Bitcoin Knots node). Port of web/src/gateway.js. */
class GatewayBackend(
    private val http: OkHttpClient,
    baseUrl: String,
    private val token: String,
) : Backend {
    private val base = baseUrl.trimEnd('/')
    override val label = "your node"

    private suspend fun req(method: String, path: String, jsonBody: String? = null): String =
        withContext(Dispatchers.IO) {
            val b = Request.Builder().url(base + path).header("authorization", "Bearer $token")
            if (jsonBody != null) b.method(method, jsonBody.toRequestBody("application/json".toMediaTypeOrNull()))
            http.newCall(b.build()).execute().use { r ->
                val body = r.body?.string().orEmpty()
                if (!r.isSuccessful) {
                    val msg = runCatching { JSONObject(body).optString("error") }.getOrNull()
                    error(msg?.ifBlank { "gateway HTTP ${r.code}" } ?: "gateway HTTP ${r.code}")
                }
                body
            }
        }

    suspend fun connect(chain: String, network: String, xpub: String, fingerprint: String) {
        val body = JSONObject().apply {
            put("chain", chain); put("network", network)
            put("account_xpub", xpub); put("master_fingerprint", fingerprint)
        }
        req("POST", "/v1/connect", body.toString())
    }

    override suspend fun status(): ChainStatus {
        val s = JSONObject(req("GET", "/v1/status"))
        val node = s.optJSONObject("node")
        val scanning = s.optJSONObject("scanning")
        val pricing = s.optJSONObject("pricing")?.let {
            ServicePricing(
                address = it.getString("address"),
                bps = it.getInt("bps"),
                floorSat = it.getLong("floor_sat"),
                capSat = it.getLong("cap_sat"),
            )
        }
        return ChainStatus(
            blocks = (node?.optLong("blocks") ?: 0L).toULong(),
            synced = scanning == null && (node?.optDouble("progress") ?: 0.0) > 0.999,
            via = label,
            chain = node?.optString("chain") ?: "",
            subversion = node?.optString("subversion") ?: "",
            scanningPct = scanning?.let { (it.optDouble("progress") * 100).toInt() },
            pricing = pricing,
        )
    }

    override suspend fun balances(): Balances {
        val b = JSONObject(req("GET", "/v1/balances"))
        return Balances(b.optLong("confirmed_sat"), b.optLong("pending_sat"))
    }

    override suspend fun utxos(minConf: UInt): List<WalletUtxo> {
        val arr = JSONArray(req("GET", "/v1/utxos?min_conf=$minConf"))
        return (0 until arr.length()).map { i ->
            val u = arr.getJSONObject(i)
            WalletUtxo(
                txid = u.getString("txid"),
                vout = u.getInt("vout").toUInt(),
                valueSat = u.getLong("value_sat").toULong(),
                scriptPubkeyHex = u.getString("script_pubkey_hex"),
                confirmations = u.getInt("confirmations").toUInt(),
                derivationIndex = u.getInt("derivation_index").toUInt(),
                isChange = u.getBoolean("is_change"),
            )
        }
    }

    override suspend fun feerateSatVb(confTarget: Int): ULong =
        JSONObject(req("GET", "/v1/feerate?conf_target=$confTarget")).optLong("sat_vb", 1).toULong()

    override suspend fun history(count: Int): List<HistoryEntry> {
        val arr = JSONArray(req("GET", "/v1/history?count=$count"))
        return (0 until arr.length()).map { i ->
            val h = arr.getJSONObject(i)
            HistoryEntry(
                txid = h.getString("txid"),
                send = h.getString("direction") == "send",
                amountSat = h.getLong("amount_sat"),
                confirmations = h.getLong("confirmations"),
                time = h.getLong("time"),
                feeSat = kotlin.math.abs(h.optLong("fee_sat")), // fortisd reports it signed (negative for sends)
            )
        }
    }

    override suspend fun broadcast(rawHex: String): String {
        val body = JSONObject().put("hex", rawHex).toString()
        return JSONObject(req("POST", "/v1/broadcast", body)).getString("txid")
    }
}
