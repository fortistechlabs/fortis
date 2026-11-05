package com.fortis.wallet.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CheckboxDefaults
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import com.fortis.wallet.R
import com.fortis.wallet.WalletViewModel
import com.fortis.wallet.ui.theme.Fx
import com.fortis.wallet.ui.theme.FxType
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch

/** Opens the support form with an error already filled in; null where there is no form (e.g. before unlock). */
val LocalSendToSupport = compositionLocalOf<((String) -> Unit)?> { null }

/** The "Send to support" link under an error message. */
@Composable
fun SendToSupportLink(error: String) {
    val open = LocalSendToSupport.current ?: return
    TextButton({ open(error) }) { Text(stringResource(R.string.support_send_error), color = Fx.signal, style = FxType.meta) }
}

/** Settings → Help & support: opens the support form. */
@Composable
fun SupportCard(onOpen: () -> Unit) {
    GlassCard {
        Text(stringResource(R.string.support_title), color = Fx.text, fontWeight = FontWeight.SemiBold)
        Text(stringResource(R.string.support_card_body), style = FxType.meta, color = Fx.textDim)
        GhostButton(stringResource(R.string.support_contact), Modifier.fillMaxWidth()) { onOpen() }
    }
}

/**
 * The support form: what happened, an optional email for a reply, and whether to attach
 * diagnostics (on by default; what they contain is spelled out). Shows the ticket id once sent.
 */
@Composable
fun SupportDialog(vm: WalletViewModel, prefill: String, onDismiss: () -> Unit) {
    val scope = rememberCoroutineScope()
    var message by remember { mutableStateOf(if (prefill.isBlank()) "" else "\n\n— ${prefill.trim()}") }
    var email by remember { mutableStateOf("") }
    var includeLogs by remember { mutableStateOf(true) }
    var sending by remember { mutableStateOf(false) }
    var ticket by remember { mutableStateOf<String?>(null) }
    var failure by remember { mutableStateOf<String?>(null) }
    AlertDialog(
        onDismissRequest = { if (!sending) onDismiss() },
        containerColor = Fx.panel,
        title = { Text(stringResource(R.string.support_title), color = Fx.text) },
        text = {
            Column(
                Modifier.verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(Fx.s3),
            ) {
                val sent = ticket
                if (sent != null) {
                    Text(stringResource(R.string.support_sent, sent), color = Fx.good)
                } else {
                    Text(stringResource(R.string.support_what_happened), style = FxType.meta, color = Fx.textDim)
                    OutlinedTextField(
                        message, { message = it.take(MAX_MESSAGE) },
                        modifier = Modifier.fillMaxWidth().heightIn(min = 120.dp),
                        placeholder = { Text(stringResource(R.string.support_message_hint), color = Fx.textFaint) },
                    )
                    Field(email, { email = it.take(200) }, stringResource(R.string.support_email))
                    Row(verticalAlignment = Alignment.Top) {
                        Checkbox(includeLogs, { includeLogs = it }, colors = CheckboxDefaults.colors(checkedColor = Fx.signal))
                        Text(stringResource(R.string.support_include_logs), style = FxType.meta, color = Fx.textDim)
                    }
                    ErrorText(failure)
                }
            }
        },
        confirmButton = {
            if (ticket != null) {
                TextButton(onDismiss) { Text(stringResource(R.string.action_done), color = Fx.signal) }
            } else {
                TextButton(
                    {
                        sending = true
                        failure = null
                        scope.launch {
                            try {
                                ticket = vm.sendSupportTicket(message, email, includeLogs)
                            } catch (e: CancellationException) {
                                throw e
                            } catch (e: Exception) {
                                failure = e.message ?: e.toString()
                            } finally {
                                sending = false
                            }
                        }
                    },
                    enabled = !sending && message.isNotBlank(),
                ) {
                    Text(stringResource(if (sending) R.string.working else R.string.support_send), color = Fx.signal)
                }
            }
        },
        dismissButton = {
            if (ticket == null) TextButton({ if (!sending) onDismiss() }) { Text(stringResource(R.string.action_cancel), color = Fx.textDim) }
        },
    )
}

private const val MAX_MESSAGE = 5_000
