package com.fortis.wallet.ui

import android.Manifest
import android.content.pm.PackageManager
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Box
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.fortis.wallet.R
import com.fortis.wallet.ui.theme.Fx
import com.google.zxing.BarcodeFormat
import com.journeyapps.barcodescanner.BarcodeCallback
import com.journeyapps.barcodescanner.DecoratedBarcodeView
import com.journeyapps.barcodescanner.DefaultDecoderFactory

private val MULTIPART_UR = Regex("^ur:[a-z0-9-]+/[1-9][0-9]*-[1-9][0-9]*/[a-z]+$", RegexOption.IGNORE_CASE)

/** One frame of an animated BC-UR QR code (`ur:<type>/<n>-<total>/<payload>`),
 *  as opposed to a single-part UR or plain text. */
fun isMultipartUr(text: String): Boolean = MULTIPART_UR.matches(text.trim())

/**
 * An inline camera view that keeps decoding QR codes — unlike the one-shot
 * `ScanContract` the Send tab uses, an animated QR needs every frame. Each
 * distinct frame is passed to [onFrame] (immediate repeats are dropped; an
 * animated code loops). Asks for the camera permission itself.
 */
@Composable
fun ContinuousQrScanner(onFrame: (String) -> Unit, modifier: Modifier = Modifier) {
    val ctx = LocalContext.current
    var granted by remember {
        mutableStateOf(ContextCompat.checkSelfPermission(ctx, Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED)
    }
    var asked by remember { mutableStateOf(false) }
    val ask = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted = it; asked = true }
    LaunchedEffect(Unit) { if (!granted) ask.launch(Manifest.permission.CAMERA) }
    if (!granted) {
        if (asked) Text(stringResource(R.string.hw_camera_denied), color = Fx.bad, modifier = modifier)
        return
    }

    val latest by rememberUpdatedState(onFrame)
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    val view = remember {
        DecoratedBarcodeView(ctx).apply {
            barcodeView.decoderFactory = DefaultDecoderFactory(listOf(BarcodeFormat.QR_CODE))
            setStatusText("")
            var last: String? = null
            decodeContinuous(BarcodeCallback { r ->
                val text = r.text ?: return@BarcodeCallback
                if (text != last) {
                    last = text
                    latest(text)
                }
            })
        }
    }
    DisposableEffect(lifecycle) {
        val obs = LifecycleEventObserver { _, e ->
            when (e) {
                Lifecycle.Event.ON_RESUME -> view.resume()
                Lifecycle.Event.ON_PAUSE -> view.pause()
                else -> {}
            }
        }
        lifecycle.addObserver(obs)
        if (lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED)) view.resume()
        onDispose {
            lifecycle.removeObserver(obs)
            view.pause()
        }
    }
    Box(modifier) { AndroidView({ view }, Modifier) }
}
