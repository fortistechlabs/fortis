package com.fortis.wallet.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import com.fortis.wallet.R
import com.fortis.wallet.ui.theme.Fx
import com.fortis.wallet.ui.theme.FxType

/**
 * A small, portrait QR scanner in a popup over the current screen (instead of
 * the full-screen scanner activity). Calls [onResult] once, with the first code
 * read, and is then expected to be removed by the caller.
 */
@Composable
fun QrScanDialog(onResult: (String) -> Unit, onDismiss: () -> Unit) {
    var done by remember { mutableStateOf(false) }
    AlertDialog(
        onDismissRequest = onDismiss,
        containerColor = Fx.panel,
        title = { Text(stringResource(R.string.action_scan_qr), color = Fx.text) },
        text = {
            Column {
                ContinuousQrScanner(
                    onFrame = { text ->
                        if (!done) {
                            done = true
                            onResult(text)
                        }
                    },
                    modifier = Modifier.fillMaxWidth().aspectRatio(1f).clip(RoundedCornerShape(12.dp)),
                )
                Text(
                    stringResource(R.string.scan_prompt),
                    style = FxType.meta, color = Fx.textDim,
                    modifier = Modifier.padding(top = 8.dp),
                )
            }
        },
        confirmButton = {},
        dismissButton = {
            TextButton(onDismiss) { Text(stringResource(R.string.action_cancel), color = Fx.text) }
        },
    )
}
