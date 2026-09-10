package com.fortis.wallet.data

import android.content.Context
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.intPreferencesKey
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.first
import org.json.JSONArray
import org.json.JSONObject
import java.util.UUID

private val Context.dataStore by preferencesDataStore("fortis")

const val LOCK_BIOMETRIC = "biometric"
const val LOCK_PASSWORD = "password"

/** The XBT chain was persisted as "btcb2" before the ticker rename — upgrade any
 *  value read back from storage. */
private fun normalizeChain(chain: String) = if (chain == "btcb2") "xbt" else chain

/** One wallet on this device. `sealed` is the seed, already encrypted under the
 *  app secret — safe at rest. Chain-independent: the same seed can be listed on
 *  both chains (addresses are identical), so a "clone" just copies `sealed`. */
data class WalletConfig(
    val id: String,
    val name: String,
    val chain: String,
    val network: String,
    val sealed: String,
    val salt: String,
    val nextReceive: Int = 0,
    val nextChange: Int = 0,
    /** The per-install token for the hosted edge. */
    val backendToken: String? = null,
) {
    /** e.g. `XBT · Savings` — the label the picker shows. */
    val display: String get() = "${chain.uppercase()} · $name"
    val otherChain: String get() = if (chain == "btc") "xbt" else "btc"

    fun toJson(): JSONObject = JSONObject().apply {
        put("id", id); put("name", name); put("chain", chain); put("network", network)
        put("sealed", sealed); put("salt", salt)
        put("next_receive", nextReceive); put("next_change", nextChange)
        backendToken?.let { put("token", it) }
    }

    companion object {
        fun fromJson(o: JSONObject) = WalletConfig(
            id = o.getString("id"),
            name = o.optString("name", "Wallet"),
            chain = normalizeChain(o.optString("chain", "xbt")),
            network = o.optString("network", "mainnet"),
            sealed = o.getString("sealed"),
            salt = o.getString("salt"),
            nextReceive = o.optInt("next_receive", 0),
            nextChange = o.optInt("next_change", 0),
            backendToken = o.optString("token").ifBlank { null },
        )
    }
}

/**
 * The whole persisted picture.
 *
 * @param lockMode      [LOCK_BIOMETRIC] or [LOCK_PASSWORD] — how the app is unlocked.
 * @param appWrapped    biometric mode: the app secret wrapped by the Keystore key.
 */
data class WalletState(
    val wallets: List<WalletConfig>,
    val selectedId: String?,
    val lockMode: String?,
    val appWrapped: String?,
)

class Store(private val ctx: Context) {
    private object K {
        val wallets = stringPreferencesKey("wallets")
        val selected = stringPreferencesKey("selected")
        val lockMode = stringPreferencesKey("lock_mode")
        val appWrapped = stringPreferencesKey("app_wrapped")
        // legacy single-wallet keys (pre multi-wallet) — migrated on first load
        val chain = stringPreferencesKey("chain")
        val network = stringPreferencesKey("network")
        val sealed = stringPreferencesKey("sealed")
        val salt = stringPreferencesKey("salt")
        val nextReceive = intPreferencesKey("next_receive")
        val nextChange = intPreferencesKey("next_change")
        val backendToken = stringPreferencesKey("backend_token")
        val backendKind = stringPreferencesKey("backend_kind")
        val backendUrl = stringPreferencesKey("backend_url")
    }

    suspend fun load(): WalletState {
        val p = ctx.dataStore.data.first()

        p[K.wallets]?.let { raw ->
            val list = decode(raw)
            // Persist the btcb2 → xbt rename so the stored JSON stops carrying the old code.
            if (raw.contains("btcb2")) ctx.dataStore.edit { it[K.wallets] = encode(list) }
            val sel = p[K.selected]?.takeIf { id -> list.any { it.id == id } } ?: list.firstOrNull()?.id
            val lock = p[K.lockMode] ?: if (list.isNotEmpty()) LOCK_PASSWORD else null
            return WalletState(list, sel, lock, p[K.appWrapped])
        }

        // Migrate a legacy single wallet (password-locked) into the list.
        val legacySealed = p[K.sealed]
        val legacySalt = p[K.salt]
        if (legacySealed != null && legacySalt != null) {
            val w = WalletConfig(
                id = UUID.randomUUID().toString(),
                name = "Wallet",
                chain = normalizeChain(p[K.chain] ?: "xbt"),
                network = p[K.network] ?: "mainnet",
                sealed = legacySealed,
                salt = legacySalt,
                nextReceive = p[K.nextReceive] ?: 0,
                nextChange = p[K.nextChange] ?: 0,
                backendToken = p[K.backendToken],
            )
            ctx.dataStore.edit { e ->
                e[K.wallets] = encode(listOf(w))
                e[K.selected] = w.id
                e[K.lockMode] = LOCK_PASSWORD
                listOf(K.chain, K.network, K.sealed, K.salt, K.backendToken, K.backendKind, K.backendUrl)
                    .forEach { e.remove(it) }
                e.remove(K.nextReceive); e.remove(K.nextChange)
            }
            return WalletState(listOf(w), w.id, LOCK_PASSWORD, null)
        }

        return WalletState(emptyList(), null, null, null)
    }

    /** Record how the app is unlocked (set once, when the first wallet is made). */
    suspend fun setLock(mode: String, appWrapped: String?) = ctx.dataStore.edit { p ->
        p[K.lockMode] = mode
        if (appWrapped == null) p.remove(K.appWrapped) else p[K.appWrapped] = appWrapped
    }

    /** Insert or replace a wallet by id. */
    suspend fun save(w: WalletConfig) = ctx.dataStore.edit { p ->
        p[K.wallets] = encode(decode(p[K.wallets]).filter { it.id != w.id } + w)
    }

    suspend fun setSelected(id: String?) = ctx.dataStore.edit { p ->
        if (id == null) p.remove(K.selected) else p[K.selected] = id
    }

    /** Remove one wallet from the app (the seed is not destroyed — the phrase
     *  still restores it). Returns what's left. */
    suspend fun remove(id: String): WalletState {
        lateinit var out: WalletState
        ctx.dataStore.edit { p ->
            val list = decode(p[K.wallets]).filter { it.id != id }
            val sel = p[K.selected]?.takeIf { s -> list.any { it.id == s } } ?: list.firstOrNull()?.id
            val lock = if (list.isEmpty()) null else p[K.lockMode]
            val wrapped = if (list.isEmpty()) null else p[K.appWrapped]
            if (list.isEmpty()) {
                p.remove(K.wallets); p.remove(K.selected); p.remove(K.lockMode); p.remove(K.appWrapped)
            } else {
                p[K.wallets] = encode(list)
                if (sel == null) p.remove(K.selected) else p[K.selected] = sel
            }
            out = WalletState(list, sel, lock, wrapped)
        }
        return out
    }

    suspend fun wipeAll() = ctx.dataStore.edit { it.clear() }

    private fun decode(raw: String?): List<WalletConfig> {
        raw ?: return emptyList()
        val arr = JSONArray(raw)
        return (0 until arr.length()).map { WalletConfig.fromJson(arr.getJSONObject(it)) }
    }

    private fun encode(list: List<WalletConfig>) =
        JSONArray().apply { list.forEach { put(it.toJson()) } }.toString()
}
