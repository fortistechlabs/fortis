package com.fortis.wallet

import androidx.activity.compose.BackHandler
import androidx.activity.compose.LocalActivity
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import com.fortis.wallet.ui.theme.Fx
import androidx.lifecycle.viewmodel.compose.viewModel
import com.fortis.wallet.ui.AmbientBackground
import com.fortis.wallet.ui.SecureWindow
import com.fortis.wallet.ui.screens.*
import com.fortis.wallet.ui.theme.FortisTheme

// Screens that show the recovery phrase or a password.
private val SECURE_PHASES = setOf(
    Phase.Gen, Phase.Create, Phase.Restore, Phase.AppLock, Phase.RevealSeed,
)

@Composable
fun FortisApp(vm: WalletViewModel = viewModel()) {
    // One owner for FLAG_SECURE, keyed on the phase — a per-screen DisposableEffect
    // races with AnimatedContent's crossfade (the outgoing screen clears the flag
    // the incoming one just set).
    // Release builds keep every screen out of screenshots and the recents preview
    // (balances, addresses, history too). Debug builds only the secret screens,
    // so the dev builds can still be screenshotted while testing.
    SecureWindow(vm.phase in SECURE_PHASES || !BuildConfig.DEBUG)

    FortisTheme {
        AnimatedContent(
            targetState = vm.phase,
            transitionSpec = { fadeIn() togetherWith fadeOut() },
            label = "phase",
        ) { phase ->
            when (phase) {
                Phase.Loading -> Box(Modifier.fillMaxSize()) {
                    AmbientBackground()
                    CircularProgressIndicator(Modifier.align(Alignment.Center))
                }
                Phase.AppLock -> AppLockScreen(vm)
                Phase.Onboard -> OnboardScreen(vm)
                Phase.Gen -> GenScreen(vm)
                Phase.Create -> CreateScreen(vm)
                Phase.Restore -> RestoreScreen(vm)
                Phase.Watch -> WatchScreen(vm)
                Phase.HwConnect -> HwConnectScreen(vm)
                Phase.Shell -> Shell(vm)
                Phase.RevealSeed -> RevealSeedScreen(vm)
            }
        }

        // Back (button or edge swipe) goes to the previous screen; it never
        // leaves the app without asking. Dialogs and sheets handle their own.
        var askExit by remember { mutableStateOf(false) }
        BackHandler {
            when (vm.phase) {
                Phase.Shell -> if (!vm.back()) askExit = true
                Phase.RevealSeed -> vm.closeReveal()
                Phase.Gen, Phase.Create, Phase.Restore, Phase.Watch, Phase.HwConnect -> vm.backToOnboard()
                Phase.Onboard -> if (vm.addingWallet) vm.cancelOnboard() else askExit = true
                Phase.AppLock, Phase.Loading -> askExit = true
            }
        }
        if (askExit) {
            val activity = LocalActivity.current
            AlertDialog(
                onDismissRequest = { askExit = false },
                containerColor = Fx.panel,
                title = { Text(stringResource(R.string.exit_title), color = Fx.text) },
                text = { Text(stringResource(R.string.exit_body), color = Fx.textDim) },
                confirmButton = {
                    TextButton({ askExit = false; activity?.finish() }) { Text(stringResource(R.string.action_exit), color = Fx.signal) }
                },
                dismissButton = {
                    TextButton({ askExit = false }) { Text(stringResource(R.string.action_cancel), color = Fx.text) }
                },
            )
        }
    }
}
