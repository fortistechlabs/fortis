package com.fortis.wallet.ui

import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.size
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.scale
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.fortis.wallet.ui.theme.Fx
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay

/** One Lightning balance change to celebrate: [sat] in (positive) or out (negative). [id] makes each one play. */
data class LightningBurstEvent(val id: Long, val sat: Long)

private val BOLT_YELLOW = Color(0xFFFFD43B)
private val BOLT_ORANGE = Color(0xFFFF9F1C)

/**
 * A lightning bolt that strikes in the middle of the screen, with a flash and an expanding
 * ring, and the amount below it ("+21,000 sat" / "−1,000 sat"); gone after about 1.8 s.
 * Draws nothing while [event] is null. Does not take touches.
 */
@Composable
fun LightningBurst(event: LightningBurstEvent?, onDone: () -> Unit, modifier: Modifier = Modifier) {
    if (event == null) return
    val scale = remember(event.id) { Animatable(0.2f) }
    val alpha = remember(event.id) { Animatable(0f) }
    val ring = remember(event.id) { Animatable(0f) }
    val flash = remember(event.id) { Animatable(0f) }
    LaunchedEffect(event.id) {
        coroutineScope {
            listOf(
                async { alpha.animateTo(1f, tween(120)) },
                async {
                    scale.animateTo(1.25f, tween(220, easing = FastOutSlowInEasing))
                    scale.animateTo(1f, tween(180))
                },
                async {
                    flash.animateTo(0.55f, tween(80))
                    flash.animateTo(0f, tween(420))
                },
                async { ring.animateTo(1f, tween(900, easing = LinearEasing)) },
            ).awaitAll()
        }
        delay(700)
        alpha.animateTo(0f, tween(300))
        onDone()
    }
    val incoming = event.sat >= 0
    Box(modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
        // A brief screen-wide flash.
        Canvas(Modifier.fillMaxSize()) { drawRect(BOLT_YELLOW.copy(alpha = flash.value * 0.25f)) }
        Column(
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(12.dp),
            modifier = Modifier.alpha(alpha.value),
        ) {
            Canvas(Modifier.size(160.dp).scale(scale.value)) {
                val w = size.width
                val h = size.height
                // Expanding ring behind the bolt.
                val r = w * (0.25f + 0.35f * ring.value)
                drawCircle(
                    color = BOLT_YELLOW.copy(alpha = (1f - ring.value) * 0.7f),
                    radius = r,
                    center = Offset(w / 2, h / 2),
                    style = Stroke(width = 6.dp.toPx() * (1f - ring.value) + 1f),
                )
                val bolt = Path().apply {
                    moveTo(w * 0.58f, h * 0.06f)
                    lineTo(w * 0.24f, h * 0.56f)
                    lineTo(w * 0.47f, h * 0.56f)
                    lineTo(w * 0.38f, h * 0.94f)
                    lineTo(w * 0.78f, h * 0.40f)
                    lineTo(w * 0.54f, h * 0.40f)
                    close()
                }
                // Glow, then the bolt itself.
                drawPath(bolt, BOLT_YELLOW.copy(alpha = 0.35f), style = Stroke(width = 14.dp.toPx()))
                drawPath(bolt, Brush.verticalGradient(listOf(BOLT_YELLOW, BOLT_ORANGE)))
            }
            Text(
                (if (incoming) "+" else "−") + "%,d sat".format(java.util.Locale.ROOT, kotlin.math.abs(event.sat)),
                color = if (incoming) Fx.good else Fx.text,
                fontSize = 30.sp,
                fontWeight = FontWeight.Bold,
            )
        }
    }
}

/** The bolt as a small inline icon (e.g. beside the Lightning wallet's name). */
@Composable
fun LightningBoltIcon(modifier: Modifier = Modifier.size(16.dp)) {
    Canvas(modifier) {
        val w = size.width
        val h = size.height
        val bolt = Path().apply {
            moveTo(w * 0.62f, 0f)
            lineTo(w * 0.12f, h * 0.56f)
            lineTo(w * 0.46f, h * 0.56f)
            lineTo(w * 0.34f, h)
            lineTo(w * 0.88f, h * 0.40f)
            lineTo(w * 0.54f, h * 0.40f)
            close()
        }
        drawPath(bolt, Brush.verticalGradient(listOf(BOLT_YELLOW, BOLT_ORANGE)))
    }
}
