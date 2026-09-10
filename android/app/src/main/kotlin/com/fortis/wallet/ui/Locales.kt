package com.fortis.wallet.ui

import android.app.Activity
import android.app.LocaleManager
import android.content.Context
import android.content.ContextWrapper
import android.content.res.Configuration
import android.os.Build
import android.os.LocaleList
import java.util.Locale

/** BCP47 tags the app ships translations for — keep in sync with
 *  `res/xml/locales_config.xml`. "en" is the base (`values/`). */
val SUPPORTED_LOCALES: List<String> = listOf(
    "en", "af", "am", "ar", "az", "be", "bg", "bn", "ca", "cs", "da", "de", "el", "en-GB",
    "es", "es-US", "et", "eu", "fa", "fi", "fil", "fr", "fr-CA", "gl", "gu", "he", "hi",
    "hr", "hu", "hy", "id", "is", "it", "ja", "ka", "kk", "km", "kn", "ko", "ky", "lo",
    "lt", "lv", "mk", "ml", "mn", "mr", "ms", "my", "nb", "ne", "nl", "pa", "pl", "pt-BR",
    "pt-PT", "ro", "ru", "si", "sk", "sl", "sq", "sr", "sv", "sw", "ta", "te", "th", "tr",
    "uk", "ur", "vi", "zh-CN", "zh-HK", "zh-TW", "zu",
)

/** The language's own name for it, e.g. "Deutsch", "日本語" — with the region in
 *  parentheses when the tag carries one. Falls back to the raw tag. */
fun localeLabel(tag: String): String {
    val l = Locale.forLanguageTag(tag)
    val name = l.getDisplayLanguage(l).replaceFirstChar { it.titlecase(l) }
    val region = l.getDisplayCountry(l)
    return when {
        name.isBlank() -> tag
        region.isNotBlank() -> "$name ($region)"
        else -> name
    }
}

private const val PREFS = "fortis_locale"
private const val KEY = "tag"
private val API33 = Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU
private fun prefs(ctx: Context) = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

/** The app-language override currently in effect, or null when following the
 *  device. API 33+ keeps this in the framework; below that, our own prefs. */
fun currentLocaleTag(ctx: Context): String? =
    if (API33)
        ctx.getSystemService(LocaleManager::class.java)?.applicationLocales
            ?.takeUnless { it.isEmpty }?.get(0)?.toLanguageTag()
    else prefs(ctx).getString(KEY, null)

/** Set the override (null → follow the device) and reload the UI. */
fun applyLocale(ctx: Context, tag: String?) {
    if (API33) {
        ctx.getSystemService(LocaleManager::class.java)?.applicationLocales =
            if (tag == null) LocaleList.getEmptyLocaleList() else LocaleList.forLanguageTags(tag)
    } else {
        prefs(ctx).edit().apply { if (tag == null) remove(KEY) else putString(KEY, tag) }.apply()
        ctx.findActivity()?.recreate()
    }
}

/** Below API 33 there's no framework per-app locale, so wrap the base context of
 *  the Application and every Activity with a config that carries the stored tag.
 *  A no-op on 33+ (the framework already applied it). */
fun localeWrap(base: Context): Context {
    if (API33) return base
    val tag = prefs(base).getString(KEY, null) ?: return base
    val locale = Locale.forLanguageTag(tag)
    Locale.setDefault(locale)
    val config = Configuration(base.resources.configuration).apply { setLocale(locale) }
    return base.createConfigurationContext(config)
}

private fun Context.findActivity(): Activity? {
    var c: Context? = this
    while (c is ContextWrapper) {
        if (c is Activity) return c
        c = c.baseContext
    }
    return null
}
