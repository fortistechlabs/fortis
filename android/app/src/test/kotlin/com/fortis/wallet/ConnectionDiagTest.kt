package com.fortis.wallet

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class ConnectionDiagTest {
    @Test
    fun aSuccessLineSaysWhatRanHowLongAndAgainstWhat() {
        val line = connectionDebugLine("Refresh", ok = true, tookMs = 412, error = null, server = "api.fortistechlabs.com", detail = "height 982238", clock = "14:35:02")
        assertEquals("Refresh ok 14:35:02 · 412 ms · api.fortistechlabs.com · height 982238", line)
    }

    @Test
    fun aFailureLineCarriesTheReason() {
        val line = connectionDebugLine("Reconnect", ok = false, tookMs = 30_001, error = "timeout", server = "api.fortistechlabs.com", detail = null, clock = "14:35:02")
        assertTrue(line, line.startsWith("Reconnect FAILED 14:35:02 · 30001 ms"))
        assertTrue(line, line.endsWith("timeout"))
    }

    @Test
    fun theActivityLabelNamesTheRunningAction() {
        assertEquals("Refreshing…", connectionActivityLabel(ConnBusy.Refreshing))
        assertEquals("Reconnecting…", connectionActivityLabel(ConnBusy.Reconnecting))
    }
}
