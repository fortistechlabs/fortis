package com.fortis.wallet.ui

import androidx.compose.animation.core.animateDpAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.Visibility
import androidx.compose.material.icons.filled.VisibilityOff
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.fortis.wallet.ui.theme.Fx
import com.fortis.wallet.ui.theme.FxType
import com.fortis.wallet.ui.theme.caps
import com.fortis.wallet.ui.theme.ghostLabel
import com.fortis.wallet.ui.theme.Mono

/** Full-screen backdrop: flat graphite. */
@Composable
fun AmbientBackground(modifier: Modifier = Modifier) {
    Box(modifier.fillMaxSize().background(Fx.bg0))
}

/** A flat panel with a 1 dp edge. `hero = true` (the balance card) swaps the
 *  top edge for the green signal line. */
@Composable
fun GlassCard(
    modifier: Modifier = Modifier,
    fill: Color = Fx.panel,
    corner: Dp = Fx.r,
    hero: Boolean = false,
    content: @Composable ColumnScope.() -> Unit,
) {
    Column(
        modifier
            .fillMaxWidth()
            .background(fill, RoundedCornerShape(corner))
            // Before .border so it paints over the border's top edge, inset by a
            // half stroke so the whole 1 dp sits inside the card.
            .then(
                if (hero) Modifier.drawWithContent {
                    drawContent()
                    val stroke = 1.dp.toPx()
                    val inset = corner.toPx()
                    drawLine(Fx.signal, Offset(inset, stroke / 2), Offset(size.width - inset, stroke / 2), stroke)
                } else Modifier,
            )
            .border(1.dp, Fx.line, RoundedCornerShape(corner))
            .padding(Fx.s4),
        verticalArrangement = Arrangement.spacedBy(Fx.s3),
        content = content,
    )
}

/** [working]: the tap is being acted on — the button is disabled (dimmed, taps
 *  ignored) and its label reads "Working…" with the dots cycling. */
@Composable
fun PrimaryButton(
    text: String, modifier: Modifier = Modifier, enabled: Boolean = true,
    dense: Boolean = false, working: Boolean = false, onClick: () -> Unit,
) = RaisedButton(
    if (working) androidx.compose.ui.res.stringResource(com.fortis.wallet.R.string.working) + workingDots() else text,
    Fx.primaryTop, Fx.primaryBottom, Fx.primaryPressed, Fx.primaryLip, Color.White.copy(alpha = 0.45f), Fx.onPrimary,
    modifier.alpha(if (enabled && !working) 1f else 0.4f), enabled && !working, dense, onClick,
)

/** "", ".", "..", "..." on a loop, padded to three characters so the centred
 *  label (a monospace face) doesn't shift as the dots come and go. */
@Composable
private fun workingDots(): String {
    var n by remember { mutableIntStateOf(1) }
    LaunchedEffect(Unit) {
        while (true) {
            kotlinx.coroutines.delay(400)
            n = (n + 1) % 4
        }
    }
    return ".".repeat(n) + " ".repeat(3 - n)
}

@Composable
fun GhostButton(
    text: String, modifier: Modifier = Modifier, tint: Color = Fx.text,
    dense: Boolean = false, enabled: Boolean = true, onClick: () -> Unit,
) = RaisedButton(
    text, Fx.ghostTop, Fx.ghostBottom, Fx.ghostPressed, Fx.ghostLip, Color.White.copy(alpha = 0.12f), ghostLabel(tint),
    modifier.alpha(if (enabled) 1f else 0.4f), enabled, dense, onClick,
)

/** A button that looks like one: a shaded face with a faint top highlight, standing
 *  [Fx.lip] proud of a darker lip. Pressed, it sinks onto the lip, goes flat and
 *  darker, and loses its highlight. Long labels wrap inside the face, which grows to fit. */
@Composable
private fun RaisedButton(
    text: String, top: Color, bottom: Color, sunk: Color, lip: Color, highlight: Color, label: Color,
    modifier: Modifier, enabled: Boolean, dense: Boolean, onClick: () -> Unit,
) = Raised(top, bottom, sunk, lip, highlight, null, modifier, enabled, if (dense) Fx.btnHDense else Fx.btnH, onClick) {
    Text(caps(text), color = label, style = FxType.button, textAlign = TextAlign.Center,
        modifier = Modifier.padding(horizontal = Fx.s3, vertical = 8.dp))
}

/** A whole row you can press — e.g. a wallet on Home: a slate key with a green
 *  chevron; [selected] adds a signal outline. */
@Composable
fun PressableTile(selected: Boolean, onClick: () -> Unit, content: @Composable RowScope.() -> Unit) = Raised(
    Fx.ghostTop, Fx.ghostBottom, Fx.ghostPressed, Fx.ghostLip, Color.White.copy(alpha = 0.12f),
    if (selected) Fx.signal else null, Modifier, true, 40.dp, onClick,
) {
    Row(
        Modifier.fillMaxWidth().padding(horizontal = Fx.s4, vertical = Fx.s3),
        horizontalArrangement = Arrangement.spacedBy(Fx.s3),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        content()
        Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, null, tint = Fx.signal)
    }
}

@Composable
private fun Raised(
    top: Color, bottom: Color, sunk: Color, lip: Color, highlight: Color, outline: Color?,
    modifier: Modifier, enabled: Boolean, minHeight: Dp, onClick: () -> Unit,
    content: @Composable BoxScope.() -> Unit,
) {
    val source = remember { MutableInteractionSource() }
    val down by source.collectIsPressedAsState()
    val pressed = down && enabled
    val drop by animateDpAsState(if (pressed) Fx.lip else 0.dp, tween(50), label = "press")
    val shape = RoundedCornerShape(Fx.rBtn)
    val face = if (pressed) Brush.verticalGradient(listOf(sunk, sunk)) else Brush.verticalGradient(listOf(top, bottom))
    Box(modifier.fillMaxWidth().clickable(source, indication = null, enabled = enabled) { onClick() }) {
        Box(Modifier.matchParentSize().padding(top = Fx.lip).background(lip, shape))
        Box(
            Modifier
                .fillMaxWidth()
                .padding(top = drop, bottom = Fx.lip - drop)
                .heightIn(min = minHeight)
                .background(face, shape)
                .then(
                    if (pressed) Modifier
                    else Modifier.border(1.dp, Brush.verticalGradient(0f to highlight, 0.35f to Color.Transparent), shape)
                )
                .then(if (outline != null) Modifier.border(1.5.dp, outline, shape) else Modifier),
            contentAlignment = Alignment.Center,
            content = content,
        )
    }
}

@Composable
fun Field(
    value: String,
    onValueChange: (String) -> Unit,
    label: String? = null,
    password: Boolean = false,
    mono: Boolean = false,
    keyboardType: KeyboardType? = null,
    maxLen: Int? = null,
    modifier: Modifier = Modifier,
) {
    var reveal by remember { mutableStateOf(false) }
    val hidden = password && !reveal
    Column(modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(Fx.s1)) {
        if (label != null) Text(caps(label), style = FxType.label)
        OutlinedTextField(
            value = value,
            onValueChange = { if (maxLen == null || it.length <= maxLen) onValueChange(it) },
            singleLine = !mono,
            visualTransformation = if (hidden) PasswordVisualTransformation() else VisualTransformation.None,
            // Password keyboard type even when revealed: keeps the IME suggestion /
            // clipboard strip suppressed so a secret can't leak through it.
            keyboardOptions = when {
                password -> KeyboardOptions(keyboardType = KeyboardType.Password, autoCorrectEnabled = false)
                keyboardType != null -> KeyboardOptions(keyboardType = keyboardType)
                else -> KeyboardOptions.Default
            },
            trailingIcon = if (password) {
                {
                    IconButton(onClick = { reveal = !reveal }) {
                        Icon(
                            if (reveal) Icons.Filled.VisibilityOff else Icons.Filled.Visibility,
                            contentDescription = androidx.compose.ui.res.stringResource(
                                if (reveal) com.fortis.wallet.R.string.action_hide else com.fortis.wallet.R.string.action_show,
                            ),
                            tint = Fx.textDim,
                        )
                    }
                }
            } else null,
            textStyle = LocalTextStyle.current.copy(
                color = Fx.text,
                fontFamily = Mono,
            ),
            colors = OutlinedTextFieldDefaults.colors(
                focusedTextColor = Fx.text,
                unfocusedTextColor = Fx.text,
                disabledTextColor = Fx.text,
                cursorColor = Fx.signal,
                focusedBorderColor = Fx.signal,
                unfocusedBorderColor = Fx.control,
                focusedContainerColor = Fx.panel,
                unfocusedContainerColor = Fx.panel,
                focusedPlaceholderColor = Fx.textFaint,
                unfocusedPlaceholderColor = Fx.textFaint,
            ),
            shape = RoundedCornerShape(Fx.rSm),
            modifier = Modifier.fillMaxWidth(),
        )
    }
}

/** Screen scaffold: ambient behind, centred column, bar insets.
 *  `scroll = true` for long forms; keep it false when the content uses weights. */
@Composable
fun Screen(scroll: Boolean = false, content: @Composable ColumnScope.() -> Unit) {
    Box(Modifier.fillMaxSize()) {
        AmbientBackground()
        val base = Modifier
            .fillMaxSize()
            .widthIn(max = 460.dp)
            .align(Alignment.TopCenter)
            .systemBarsPadding()
            .imePadding()
        Column(
            (if (scroll) base.verticalScroll(rememberScrollState()) else base).padding(Fx.s4),
            verticalArrangement = Arrangement.spacedBy(Fx.s4),
            content = content,
        )
    }
}

@Composable
fun ErrorText(msg: String?) {
    if (!msg.isNullOrBlank()) Text(msg, color = Fx.bad, textAlign = TextAlign.Start)
}

@Composable
fun BrandMark(tagline: String? = null) {
    Column(
        Modifier.fillMaxWidth(),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(Fx.s3),
    ) {
        Image(
            painter = painterResource(com.fortis.wallet.R.drawable.ic_mark),
            contentDescription = null,
            modifier = Modifier.size(96.dp),
        )
        Text("fortis", style = FxType.brand)
        if (tagline != null) Text(tagline, color = Fx.textDim, textAlign = TextAlign.Center)
    }
}
