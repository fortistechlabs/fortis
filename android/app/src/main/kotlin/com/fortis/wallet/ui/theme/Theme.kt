package com.fortis.wallet.ui.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/** The "glassy & layered" design language — see /DESIGN.md. Dark-first. */
object Fx {
    val bg0 = Color(0xFF060713)
    val bg1 = Color(0xFF0A0C1A)
    val bg2 = Color(0xFF0C1024)
    val blobA = Color(0xD95880FF)
    val blobB = Color(0x9EA860FF)
    val blobC = Color(0x5C3AE0CD)

    val text = Color(0xFFEEF1F8)
    val textDim = Color(0xFFB7BFD2)
    val textFaint = Color(0xFF8A93A8)

    val accent = Color(0xFF6EA8FE)
    val accent2 = Color(0xFFB98CFF)
    val good = Color(0xFF49E0A6)
    val bad = Color(0xFFFF6B7D)
    val warn = Color(0xFFF6C445)

    val glass1 = Color(0x0BFFFFFF)   // rgba(255,255,255,.045)
    val glass2 = Color(0x14FFFFFF)   // rgba(255,255,255,.08)
    val glassHero = Color(0x0FFFFFFF)
    val hair = Color(0x17FFFFFF)     // rgba(255,255,255,.09)

    val rSm = 12.dp
    val r = 18.dp
    val rLg = 26.dp
    val pill = 999.dp

    val s1 = 4.dp; val s2 = 8.dp; val s3 = 12.dp; val s4 = 16.dp; val s5 = 24.dp; val s6 = 32.dp
}

private val DarkColors = darkColorScheme(
    primary = Fx.accent,
    onPrimary = Color(0xFF0A0C16),
    secondary = Fx.accent2,
    background = Fx.bg0,
    onBackground = Fx.text,
    surface = Fx.bg1,
    onSurface = Fx.text,
    error = Fx.bad,
    outline = Fx.hair,
)

private val mono = FontFamily.Monospace

val FortisType = Typography(
    displayLarge = TextStyle(fontFamily = mono, fontWeight = FontWeight.SemiBold, fontSize = 34.sp, letterSpacing = (-0.02).sp),
    titleMedium = TextStyle(fontWeight = FontWeight.SemiBold, fontSize = 17.sp, letterSpacing = (-0.01).sp),
    bodyMedium = TextStyle(fontWeight = FontWeight.Normal, fontSize = 15.sp),
    labelMedium = TextStyle(fontWeight = FontWeight.Medium, fontSize = 12.sp, letterSpacing = 0.4.sp, color = Fx.textDim),
)

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
