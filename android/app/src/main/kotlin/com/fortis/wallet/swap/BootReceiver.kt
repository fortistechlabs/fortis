package com.fortis.wallet.swap

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/**
 * On boot, one engine pass via [SwapWorker]. Not the foreground service:
 * Android 15 forbids starting a `dataSync` foreground service from
 * BOOT_COMPLETED. (The periodic `swap-sync` work survives the reboot anyway.)
 */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(ctx: Context, intent: Intent) {
        if (intent.action == Intent.ACTION_BOOT_COMPLETED) SwapWorker.runOnce(ctx)
    }
}
