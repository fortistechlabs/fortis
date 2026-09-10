package com.fortis.wallet

import android.content.Context
import android.os.Bundle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.fragment.app.FragmentActivity
import com.fortis.wallet.ui.localeWrap

// FragmentActivity (not ComponentActivity) so androidx.biometric's BiometricPrompt
// can attach — it needs a FragmentManager.
class MainActivity : FragmentActivity() {
    override fun attachBaseContext(newBase: Context) = super.attachBaseContext(localeWrap(newBase))

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        // debug-only: `adb shell am start -n com.fortistechlabs.wallet/com.fortis.wallet.MainActivity --ez crash true`
        if (BuildConfig.DEBUG && intent?.getBooleanExtra("crash", false) == true) {
            throw RuntimeException("test crash via intent")
        }
        setContent { FortisApp() }
    }
}
