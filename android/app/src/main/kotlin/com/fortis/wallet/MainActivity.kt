package com.fortis.wallet

import android.content.Context
import android.os.Bundle
import androidx.activity.compose.setContent
import androidx.activity.viewModels
import com.fortis.wallet.security.AUTO_LOCK_MS
import com.fortis.wallet.security.AutoLock
import androidx.activity.enableEdgeToEdge
import androidx.fragment.app.FragmentActivity
import com.fortis.wallet.ui.localeWrap

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
        setContent { FortisApp(vm) }
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
