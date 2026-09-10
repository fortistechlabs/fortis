package com.fortis.wallet

import android.app.Application
import android.content.Context
import com.fortis.wallet.ui.localeWrap

class FortisApplication : Application() {
    override fun attachBaseContext(base: Context) = super.attachBaseContext(localeWrap(base))

    override fun onCreate() {
        super.onCreate()
        CrashReporter.install()
    }
}
