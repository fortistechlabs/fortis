package com.fortis.wallet.lightning

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.wallet_ffi.LnChain

class EffectiveLnEnabledTest {
    @Test fun xbtDebugDisabledRuns() = assertTrue(effectiveLnEnabled(LnChain.XBT, infoEnabled = false, debug = true))
    @Test fun xbtReleaseDisabledStaysOff() = assertFalse(effectiveLnEnabled(LnChain.XBT, infoEnabled = false, debug = false))
    @Test fun btcDebugDisabledStaysOff() = assertFalse(effectiveLnEnabled(LnChain.BTC, infoEnabled = false, debug = true))
    @Test fun enabledRuns() {
        assertTrue(effectiveLnEnabled(LnChain.XBT, true, false))
        assertTrue(effectiveLnEnabled(LnChain.BTC, true, false))
    }
}
