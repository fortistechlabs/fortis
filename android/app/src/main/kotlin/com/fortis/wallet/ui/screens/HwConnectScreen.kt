package com.fortis.wallet.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import com.fortis.wallet.MAX_WALLET_NAME
import com.fortis.wallet.R
import com.fortis.wallet.WalletViewModel
import com.fortis.wallet.ui.*
import com.fortis.wallet.ui.theme.Fx
import com.fortis.wallet.ui.theme.FxType
import uniffi.wallet_ffi.ImportedAccount
import uniffi.wallet_ffi.UrAccountScanner

/** Connect a hardware wallet: scan (or paste) its BIP-84 account key, add it as
 *  a BTC watch-only wallet, then — from the Shell — offer the XBT twin. */
@Composable
fun HwConnectScreen(vm: WalletViewModel) {
    val defaultName = stringResource(R.string.onboard_hardware_default_name)
    var name by remember { mutableStateOf(defaultName) }
    var pasted by remember { mutableStateOf("") }
    var found by remember { mutableStateOf<ImportedAccount?>(null) }
    var progress by remember { mutableStateOf<Int?>(null) }
    var scanError by remember { mutableStateOf<String?>(null) }
    // One animated-QR accumulator per scan attempt; replaced on "scan again".
    var scanner by remember { mutableStateOf(UrAccountScanner()) }
    DisposableEffect(scanner) { onDispose { scanner.close() } }

    fun onFrame(text: String) {
        if (found != null) return
        try {
            if (isMultipartUr(text)) {
                scanner.receive(text)
                progress = scanner.progressPercent().toInt()
                if (scanner.isComplete()) found = vm.importAccountScan(scanner)
            } else {
                found = vm.importAccountText(text)
            }
            scanError = null
        } catch (e: Exception) {
            scanError = e.message
        }
    }

    Screen(scroll = true) {
        Text(stringResource(R.string.hw_title), style = MaterialTheme.typography.titleMedium, color = Fx.text)
        Text(stringResource(R.string.hw_body), color = Fx.textDim)
        Field(name, { name = it }, stringResource(R.string.field_wallet_name), maxLen = MAX_WALLET_NAME)

        val account = found
        if (account == null) {
            ContinuousQrScanner(
                ::onFrame,
                Modifier.fillMaxWidth().height(300.dp).clip(RoundedCornerShape(Fx.r)),
            )
            progress?.let {
                LinearProgressIndicator({ it / 100f }, Modifier.fillMaxWidth(), color = Fx.signal, trackColor = Fx.panelRaised)
                Text(stringResource(R.string.hw_scan_progress, it), style = FxType.meta)
            }
            ErrorText(scanError)
            Field(pasted, { pasted = it.trim() }, stringResource(R.string.hw_or_paste), mono = true)
        } else {
            GlassCard {
                val fp = account.masterFingerprint
                val path = account.path
                Text(
                    if (fp != null && path != null) stringResource(R.string.hw_found, fp, path)
                    else stringResource(R.string.hw_found_bare),
                    color = Fx.good,
                )
                Text(account.xpub, style = FxType.fiatSmall)
                GhostButton(stringResource(R.string.hw_scan_again), dense = true) {
                    found = null; progress = null; scanError = null; scanner = UrAccountScanner()
                }
            }
        }
        ErrorText(vm.error)
        Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
            GhostButton(stringResource(R.string.action_back), Modifier.weight(1f)) { vm.cancelOnboard() }
            PrimaryButton(
                stringResource(R.string.action_add), Modifier.weight(1f),
                enabled = found != null || pasted.isNotBlank(),
            ) {
                val a = found ?: try {
                    vm.importAccountText(pasted)
                } catch (e: Exception) {
                    scanError = e.message
                    null
                }
                if (a != null) vm.connectHardware(name, a)
            }
        }
    }
}

/** Shown once, right after a hardware wallet is added: open the same addresses
 *  on XBT as a view-only wallet. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun XbtOfferSheet(vm: WalletViewModel) {
    ModalBottomSheet(onDismissRequest = { vm.dismissXbtOffer() }, containerColor = Fx.panel) {
        Column(Modifier.padding(Fx.s4).padding(bottom = Fx.s5), verticalArrangement = Arrangement.spacedBy(Fx.s3)) {
            Text(stringResource(R.string.hw_offer_xbt_title), style = MaterialTheme.typography.titleMedium, color = Fx.text)
            Text(stringResource(R.string.hw_offer_xbt_body), color = Fx.textDim)
            PrimaryButton(stringResource(R.string.hw_offer_xbt_yes)) { vm.acceptXbtOffer() }
            GhostButton(stringResource(R.string.action_not_now)) { vm.dismissXbtOffer() }
        }
    }
}
