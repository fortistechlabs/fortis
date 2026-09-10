package com.fortis.wallet.ui

import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
import android.view.WindowManager
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.ui.platform.LocalView

private fun Context.findActivity(): Activity? {
    var c: Context? = this
    while (c is ContextWrapper) {
        if (c is Activity) return c
        c = c.baseContext
    }
    return null
}

/** Applies or removes `FLAG_SECURE` on the hosting window so `secure` screens are
 *  excluded from screenshots, screen recording and the recents thumbnail.
 *  Drive this from one place (see [com.fortis.wallet.FortisApp]) — a per-screen
 *  effect races with the screen-transition crossfade. */
@Composable
fun SecureWindow(secure: Boolean) {
    val window = LocalView.current.context.findActivity()?.window ?: return
    DisposableEffect(secure) {
        if (secure) {
            window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        } else {
            window.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
        }
        onDispose { }
    }
}
