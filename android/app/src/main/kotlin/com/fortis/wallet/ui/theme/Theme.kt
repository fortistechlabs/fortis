package com.fortis.wallet.ui.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.em
import com.fortis.wallet.R
import androidx.compose.ui.unit.sp

/** Graphite signal — see /DESIGN.md. Dark-only: flat graphite, high-contrast
 *  text, one green signal accent. Every text colour is ≥ 4.5:1 on [bg0] and
 *  [panel] (`ContrastTest` enforces it). */
object Fx {
    val bg0 = Color(0xFF0B0C0D)
    val panel = Color(0xFF111315)
    val panelRaised = Color(0xFF16191C)
    /** Decorative dividers and panel edges — deliberately quiet. */
    val line = Color(0xFF222528)
    /** Borders of interactive controls (fields, secondary buttons) — ≥ 3:1. */
    val control = Color(0xFF5C636A)

    val text = Color(0xFFE9ECEF)
    /** Secondary text, fiat values, meta. */
    val textDim = Color(0xFF9AA1A8)
    /** Instructional hints only — never a value. */
    val textFaint = Color(0xFF7D848B)

    /** The one accent: active tab, hero line, units, incoming amounts, focus. */
    val signal = Color(0xFF00FF41)
    val good = signal
    val warn = Color(0xFFE6C15A)
    val bad = Color(0xFFF2545B)

    val primary = Color(0xFFE9ECEF)
    val onPrimary = Color(0xFF03140A)
    /** Buttons: a shaded face (lighter at the top) on a darker lip. The main action is
     *  signal green; every other button is solid slate — filled, never just outlined. */
    val primaryTop = Color(0xFF4DFF7A)
    val primaryBottom = Color(0xFF00D936)
    val primaryPressed = Color(0xFF00C832)
    val primaryLip = Color(0xFF006B1B)
    val ghostTop = Color(0xFF3A424A)
    val ghostBottom = Color(0xFF2B3238)
    val ghostPressed = Color(0xFF272D33)
    val ghostLip = Color(0xFF15191C)
    /** Secondary text and the error colour, lifted to read on slate. */
    val dimOnSlate = Color(0xFFB4BBC2)
    val badOnSlate = Color(0xFFFF8A8F)

    val rSm = 4.dp
    val r = 6.dp
    val rLg = 8.dp
    /** Buttons: rounder than panels, and raised on a [lip] that the face presses down onto. */
    val rBtn = 10.dp
    val lip = 3.dp
    val btnH = 48.dp
    val btnHDense = 40.dp

    val s1 = 4.dp; val s2 = 8.dp; val s3 = 12.dp; val s4 = 16.dp; val s5 = 24.dp; val s6 = 32.dp

}

/** A secondary button's label colour: the dim and error tints are lifted so they still
 *  read on the slate face. */
fun ghostLabel(tint: Color): Color = when (tint) {
    Fx.bad -> Fx.badOnSlate
    Fx.textDim -> Fx.dimOnSlate
    else -> tint
}

private val DarkColors = darkColorScheme(
    primary = Fx.signal,
    onPrimary = Fx.onPrimary,
    secondary = Fx.signal,
    background = Fx.bg0,
    onBackground = Fx.text,
    surface = Fx.panel,
    onSurface = Fx.text,
    error = Fx.bad,
    outline = Fx.line,
)

/** JetBrains Mono — the one typeface: every word, number, address and the phrase
 *  grid. Scripts it lacks (CJK, Arabic, Indic…) fall back to the system font glyph
 *  by glyph. */
val Mono = FontFamily(
    Font(R.font.jetbrains_mono_regular, FontWeight.Normal),
    Font(R.font.jetbrains_mono_medium, FontWeight.Medium),
    Font(R.font.jetbrains_mono_semibold, FontWeight.SemiBold),
)

private fun mono(size: Int, weight: FontWeight = FontWeight.Normal, color: Color = Fx.text) =
    TextStyle(fontFamily = Mono, fontWeight = weight, fontSize = size.sp, color = color, fontFeatureSettings = "tnum")

/** The named type scale (see /DESIGN.md). `label`, `nav` and `button` are set in
 *  capitals — Compose has no text-transform, so callers wrap the text in `caps()`
 *  (locale-aware: Turkish i → İ). */
object FxType {
    val balance = mono(36, FontWeight.Medium).copy(letterSpacing = (-0.01).em)
    val fiat = mono(17, color = Fx.textDim)
    val amount = mono(15)
    /** Recovery-phrase grid — 13 sp so "13  favorite" fits a third of a 360 dp phone. */
    val phrase = mono(13)
    val fiatSmall = mono(13, color = Fx.textDim)
    val title = TextStyle(fontFamily = Mono, fontWeight = FontWeight.SemiBold, fontSize = 17.sp, color = Fx.text)
    val body = TextStyle(fontFamily = Mono, fontWeight = FontWeight.Normal, fontSize = 15.sp)
    val meta = TextStyle(fontFamily = Mono, fontWeight = FontWeight.Normal, fontSize = 13.sp, color = Fx.textDim)
    /** Uppercase at the call site. */
    val label = TextStyle(fontFamily = Mono, fontWeight = FontWeight.Medium, fontSize = 11.sp, letterSpacing = 0.08.em, color = Fx.textDim)
    /** Uppercase at the call site. */
    val nav = TextStyle(fontFamily = Mono, fontWeight = FontWeight.SemiBold, fontSize = 12.sp, letterSpacing = 0.06.em)
    /** Uppercase at the call site. */
    val button = nav
    val brand = TextStyle(fontFamily = Mono, fontWeight = FontWeight.Medium, fontSize = 32.sp, color = Fx.text)
}

/** Material's scale, all in [Mono] (Material components and an unstyled `Text`
 *  read from here), with the roles fortis uses pointed at [FxType]. Lazy: FxType's
 *  initialiser touches this file, so an eager value could capture half-built styles. */
val FortisType by lazy {
    Typography().run {
        fun TextStyle.mono() = copy(fontFamily = Mono)
        Typography(
            displayLarge = FxType.balance, displayMedium = displayMedium.mono(), displaySmall = displaySmall.mono(),
            headlineLarge = headlineLarge.mono(), headlineMedium = headlineMedium.mono(), headlineSmall = headlineSmall.mono(),
            titleLarge = titleLarge.mono(), titleMedium = FxType.title, titleSmall = titleSmall.mono(),
            bodyLarge = FxType.body, bodyMedium = FxType.body, bodySmall = FxType.meta,
            labelLarge = labelLarge.mono(), labelMedium = FxType.label, labelSmall = labelSmall.mono(),
        )
    }
}

/** Dark-first and dark-only: every screen is hand-styled against [Fx]'s dark
 *  palette, so the Material colour scheme is pinned to dark regardless of the
 *  system setting. This also keeps Material surfaces (menus, dialogs) dark. */
@Composable
fun FortisTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = DarkColors,
        typography = FortisType,
        content = content,
    )
}
