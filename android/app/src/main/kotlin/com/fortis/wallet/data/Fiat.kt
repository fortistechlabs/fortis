package com.fortis.wallet.data

import android.content.Context
import org.json.JSONObject
import java.text.NumberFormat
import java.util.Currency
import java.util.Locale

/** The currencies a balance can be shown in: USD (the price feeds' own) plus
 *  every currency in the ECB daily reference rates the edge converts with
 *  (fortis-edge `fx.rs`). One the edge doesn't send just shows no value. */
val FIAT_CODES: List<String> = listOf(
    "USD", "EUR", "GBP", "CHF", "JPY", "CAD", "AUD", "NZD", "SEK", "NOK", "DKK", "ISK",
    "PLN", "CZK", "HUF", "RON", "TRY", "BRL", "MXN", "CNY", "HKD", "SGD", "KRW", "INR",
    "IDR", "MYR", "PHP", "THB", "ILS", "ZAR",
)

/** The device region's currency when it's one we have, else USD. */
fun defaultFiat(locale: Locale = Locale.getDefault()): String =
    runCatching { Currency.getInstance(locale).currencyCode }.getOrNull()
        ?.takeIf { it in FIAT_CODES } ?: "USD"

/** `/v1/prices` → price per whole coin by currency code. Anything that isn't a
 *  3-letter upper-case code with a positive finite number is dropped; no USD
 *  means no usable feed (null). */
fun parsePrices(body: String): Map<String, Double>? {
    val o = runCatching { JSONObject(body) }.getOrNull() ?: return null
    val out = HashMap<String, Double>()
    for (k in o.keys()) {
        if (k.length != 3 || !k.all { it in 'A'..'Z' }) continue
        val v = (o.opt(k) as? Number)?.toDouble() ?: continue
        if (v.isFinite() && v > 0.0) out[k] = v
    }
    return out.takeIf { "USD" in it }
}

/** An approximate fiat value, grouped and punctuated per the device locale, in
 *  [code]'s own decimals: "$12.34", "1.234,56 €", "¥1,235", and "<$0.01" /
 *  "<¥1" for a non-zero amount below the smallest unit. */
fun fmtFiat(v: Double, code: String): String {
    val c = Currency.getInstance(code)
    val nf = NumberFormat.getCurrencyInstance().apply {
        currency = c
        val dp = c.defaultFractionDigits.coerceAtLeast(0)
        minimumFractionDigits = dp
        maximumFractionDigits = dp
    }
    val unit = Math.pow(10.0, -c.defaultFractionDigits.coerceAtLeast(0).toDouble())
    return spaced(if (v > 0.0 && v < unit) "<" + nf.format(unit) else nf.format(v))
}

/** A currency shown by its code gets a (no-break) space from the number, as
 *  the browser's Intl does: Java writes "SEK0.00" / "0,00SEK" outside the
 *  currency's home locale. */
private fun spaced(s: String): String =
    s.replace(Regex("([A-Z]{3})(?=[\\d-])"), "$1\u00A0").replace(Regex("(?<=\\d)([A-Z]{3})"), "\u00A0$1")

/** The currency's short symbol in the app's language ("€", "$", "kr", "zł"),
 *  or "" when it has none apart from its code (CHF). */
fun fiatSymbol(code: String): String {
    val s = runCatching {
        if (android.os.Build.VERSION.SDK_INT >= 28)
            android.icu.util.Currency.getInstance(code)
                .getName(android.icu.util.ULocale.getDefault(), android.icu.util.Currency.NARROW_SYMBOL_NAME, null)
        else Currency.getInstance(code).getSymbol(Locale.getDefault())
    }.getOrNull() ?: return ""
    return if (s == code || s.isBlank()) "" else s
}

/** "EUR · Euro", in the app's language. */
fun fiatLabel(code: String): String =
    runCatching { "$code · " + Currency.getInstance(code).getDisplayName(Locale.getDefault()) }.getOrDefault(code)

/** The chosen display currency; null = follow the device region. */
class FiatPrefs(ctx: Context) {
    private val prefs = ctx.getSharedPreferences("fortis_fiat", Context.MODE_PRIVATE)

    fun chosen(): String? = prefs.getString(KEY, null)?.takeIf { it in FIAT_CODES }

    fun set(code: String?) {
        prefs.edit().apply { if (code == null) remove(KEY) else putString(KEY, code) }.apply()
    }

    private companion object { const val KEY = "code" }
}
