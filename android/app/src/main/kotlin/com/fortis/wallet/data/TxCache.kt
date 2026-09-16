package com.fortis.wallet.data

import android.content.ContentValues
import android.content.Context
import android.database.sqlite.SQLiteDatabase
import android.database.sqlite.SQLiteOpenHelper
import org.json.JSONArray

private const val DB_NAME = "fortis-tx-cache.sqlite"
private const val DB_VERSION = 1

/** Permanent, on-device store of confirmed transaction history, keyed by
 *  (chain, address) — confirmed transactions never change, so once seen here
 *  a wallet reopen or wallet switch can paint instantly from this instead of
 *  always waiting on a fresh network scan. Only ever holds confirmed entries
 *  (pending/unconfirmed ones can still change, so [EsploraBackend] filters
 *  them out before calling [put]) and is never consulted for UTXO/spend
 *  selection — that stays a mandatory live network call, see
 *  `EsploraBackend.utxos()`.
 *
 *  Hand-rolled `SQLiteOpenHelper` rather than Room: this is the only local
 *  table the app needs, so it follows the project's existing style on the
 *  Rust side (`rusqlite`, no ORM) rather than pulling in Room's ksp/kapt
 *  setup for one table. */
class TxCache(context: Context) : SQLiteOpenHelper(context.applicationContext, DB_NAME, null, DB_VERSION) {
    override fun onCreate(db: SQLiteDatabase) {
        db.execSQL(
            """CREATE TABLE IF NOT EXISTS confirmed_txs (
                chain   TEXT NOT NULL,
                address TEXT NOT NULL,
                txid    TEXT NOT NULL,
                fee     INTEGER NOT NULL,
                height  INTEGER NOT NULL,
                time    INTEGER NOT NULL,
                vin     TEXT NOT NULL,
                vout    TEXT NOT NULL,
                PRIMARY KEY (chain, address, txid)
            )""",
        )
        db.execSQL("CREATE INDEX IF NOT EXISTS confirmed_txs_addr ON confirmed_txs(chain, address)")
    }

    override fun onUpgrade(db: SQLiteDatabase, oldVersion: Int, newVersion: Int) {
        // No prior schema versions yet — nothing to migrate.
    }

    /** Every persisted confirmed tx for `addresses`, keyed by address. An
     *  address missing from the result was simply never scanned before —
     *  callers still need a live check for it, same as any cache miss.
     *
     *  Chunked well under any SQLite build's variable-count ceiling: a
     *  watch-only backend's seed guess is up to 1000 addresses (`2 ×
     *  PREWARM_DEPTH`), and binding all of them plus `chain` in one query
     *  (1001 params) sits right at — or over, depending on the device's
     *  SQLite build — the historical default limit of 999. A silent failure
     *  here would hit exactly the deep watch-only wallets this cache exists
     *  for. */
    fun load(chain: String, addresses: List<String>): Map<String, List<TxSummary>> {
        if (addresses.isEmpty()) return emptyMap()
        val out = HashMap<String, MutableList<TxSummary>>()
        for (batch in addresses.chunked(200)) {
            val placeholders = batch.joinToString(",") { "?" }
            val args = (listOf(chain) + batch).toTypedArray()
            readableDatabase.rawQuery(
                "SELECT address, txid, fee, height, time, vin, vout FROM confirmed_txs " +
                    "WHERE chain = ? AND address IN ($placeholders)",
                args,
            ).use { c ->
                while (c.moveToNext()) {
                    val address = c.getString(0)
                    val summary = TxSummary(
                        txid = c.getString(1),
                        fee = c.getLong(2),
                        confirmed = true,
                        blockHeight = c.getLong(3),
                        blockTime = c.getLong(4),
                        vin = decodePairs(c.getString(5)),
                        vout = decodePairs(c.getString(6)),
                    )
                    out.getOrPut(address) { mutableListOf() } += summary
                }
            }
        }
        return out
    }

    /** Replace `address`'s whole confirmed-tx set with `txs` (every entry
     *  must already be confirmed — callers filter before calling). Safe to
     *  call repeatedly: a gap-limit walk always hands over that address's
     *  complete current confirmed list, never a partial delta, so a
     *  replace-all is exactly right, not just convenient. */
    fun put(chain: String, address: String, txs: List<TxSummary>) {
        val db = writableDatabase
        db.beginTransaction()
        try {
            db.delete("confirmed_txs", "chain = ? AND address = ?", arrayOf(chain, address))
            for (t in txs) {
                val v = ContentValues().apply {
                    put("chain", chain)
                    put("address", address)
                    put("txid", t.txid)
                    put("fee", t.fee)
                    put("height", t.blockHeight)
                    put("time", t.blockTime)
                    put("vin", encodePairs(t.vin))
                    put("vout", encodePairs(t.vout))
                }
                db.insertWithOnConflict("confirmed_txs", null, v, SQLiteDatabase.CONFLICT_REPLACE)
            }
            db.setTransactionSuccessful()
        } finally {
            db.endTransaction()
        }
    }

    private fun encodePairs(pairs: List<Pair<String?, Long>>): String {
        val arr = JSONArray()
        for ((addr, value) in pairs) arr.put(JSONArray().put(addr).put(value))
        return arr.toString()
    }

    private fun decodePairs(json: String): List<Pair<String?, Long>> {
        val arr = JSONArray(json)
        return List(arr.length()) { i ->
            val pair = arr.getJSONArray(i)
            (if (pair.isNull(0)) null else pair.getString(0)) to pair.getLong(1)
        }
    }
}
