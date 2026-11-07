package com.fortis.wallet

import android.content.Context
import android.content.Intent
import android.os.Bundle
import androidx.activity.compose.setContent
import androidx.activity.viewModels
import com.fortis.wallet.security.AUTO_LOCK_MS
import com.fortis.wallet.security.AutoLock
import androidx.activity.enableEdgeToEdge
import androidx.fragment.app.FragmentActivity
import com.fortis.wallet.ui.localeWrap
import com.fortis.wallet.ui.textFromIntent

// FragmentActivity (not ComponentActivity) so androidx.biometric's BiometricPrompt
// can attach — it needs a FragmentManager.
class MainActivity : FragmentActivity() {
    private val vm: WalletViewModel by viewModels()

    /** Out of sight for a minute or more: lock (seeds and the app secret leave memory). */
    private val autoLock = AutoLock(AUTO_LOCK_MS)

    override fun attachBaseContext(newBase: Context) = super.attachBaseContext(localeWrap(newBase))

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        // debug-only: `adb shell am start -n com.fortistechlabs.wallet/com.fortis.wallet.MainActivity --ez crash true`
        if (BuildConfig.DEBUG && intent?.getBooleanExtra("crash", false) == true) {
            throw RuntimeException("test crash via intent")
        }
        if (savedInstanceState == null) {
            textFromIntent(intent?.dataString)?.let { vm.openSendWithUri(it) }
            intent?.let { openPushedWallet(it) }
        }
        setContent { FortisApp(vm) }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        textFromIntent(intent.dataString)?.let { vm.openSendWithUri(it) }
        openPushedWallet(intent)
    }

    /** A tapped Lightning push (its data arrives as extras): open the Lightning wallet its
     *  `chain` names ("xbt" → Lightning (XBT); none → the BTC one, as older LSPs send). */
    private fun openPushedWallet(intent: Intent) {
        val extras = intent.extras ?: return
        val data = extras.keySet().mapNotNull { k -> extras.getString(k)?.let { k to it } }.toMap()
        if (com.fortis.wallet.lightning.isLightningPush(data)) {
            vm.openLightningFromPush(com.fortis.wallet.lightning.pushTargetChain(data))
        }
    }

    override fun onStop() {
        super.onStop()
        autoLock.wentToBackground()
    }

    override fun onStart() {
        super.onStart()
        if (autoLock.shouldLockOnReturn() && !vm.locked) vm.lock()
    }
}
