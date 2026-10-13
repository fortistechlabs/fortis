package com.fortis.wallet.ui.theme

import androidx.compose.runtime.Composable
import androidx.compose.runtime.ReadOnlyComposable
import androidx.compose.ui.platform.LocalConfiguration
import androidx.core.os.ConfigurationCompat
import java.util.Locale

/** [text] in capitals by [locale]'s rules — Turkish/Azerbaijani i → İ, not I.
 *  `String.uppercase()` alone uses `Locale.ROOT`. */
fun caps(text: String, locale: Locale): String = text.uppercase(locale)

/** [caps] in the UI's current language (follows the in-app language override).
 *  Used for every `label` / `nav` / `button` style. */
@Composable
@ReadOnlyComposable
fun caps(text: String): String =
    // An empty locale list (never seen) falls back to ROOT: Locale.getDefault()
    // here couldn't be observed, so a language change wouldn't recompose.
    caps(text, ConfigurationCompat.getLocales(LocalConfiguration.current)[0] ?: Locale.ROOT)
