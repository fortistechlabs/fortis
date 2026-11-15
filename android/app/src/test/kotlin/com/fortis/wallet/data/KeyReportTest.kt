package com.fortis.wallet.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class KeyReportTest {
    private val snap = KeySnapshot(
        appVersion = "0.6.4 (604)", manufacturer = "Xiaomi", model = "Redmi Note 11 Pro 5G",
        release = "13", sdk = 33, securityPatch = "2024-12-01",
        deviceSecure = true, biometricStatus = "BIOMETRIC_SUCCESS",
        lockMode = "biometric", wrappedLength = 88,
        keyPresence = KeyPresence.MISSING, keyNote = null, keyInfo = null, aliases = listOf("fortis.swap"),
    )

    @Test
    fun logAppendsAndKeepsOnlyTheNewestEntries() {
        var l = emptyList<TraceEvent>()
        repeat(KeyTraceLog.MAX + 10) { l = KeyTraceLog.append(l, TraceEvent(it.toLong(), "e$it")) }
        assertEquals(KeyTraceLog.MAX, l.size)
        assertEquals("e${KeyTraceLog.MAX + 9}", l.last().text)
        assertEquals("e10", l.first().text)
    }

    @Test
    fun identicalConsecutiveEventsAreNotRepeated() {
        var l = KeyTraceLog.append(emptyList(), TraceEvent(1, "failed: x"))
        l = KeyTraceLog.append(l, TraceEvent(2, "failed: x"))
        assertEquals(1, l.size)
        l = KeyTraceLog.append(l, TraceEvent(3, "failed: y"))
        l = KeyTraceLog.append(l, TraceEvent(4, "failed: x"))
        assertEquals(3, l.size)
    }

    @Test
    fun encodeDecodeRoundTripsAndFlattensNewlines() {
        val l = listOf(TraceEvent(5, "a\nb|c"), TraceEvent(6, "plain"))
        val back = KeyTraceLog.decode(KeyTraceLog.encode(l))
        assertEquals(listOf(5L, 6L), back.map { it.atMs })
        assertFalse(back.any { it.text.contains('\n') })
        assertEquals("plain", back[1].text)
    }

    @Test
    fun decodeToleratesGarbage() {
        assertEquals(emptyList<TraceEvent>(), KeyTraceLog.decode(null))
        assertEquals(emptyList<TraceEvent>(), KeyTraceLog.decode("not a line\n|\nx|y"))
        assertEquals(1, KeyTraceLog.decode("7|ok\njunk").size)
    }

    @Test
    fun scrubHidesLongTokensButKeepsOrdinaryText() {
        val addr = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq"
        assertEquals("Keystore failed <x> code -26", scrubForReport("Keystore failed $addr code -26"))
        assertEquals("the fortis.app key is missing", scrubForReport("the fortis.app key is missing"))
    }

    @Test
    fun reportStatesTheFactsForALostKey() {
        val r = formatKeyReport(
            snap, nowMs = 1_000_000L, lastOkMs = null,
            events = listOf(TraceEvent(0, "device secure: true -> false")),
            failure = FailureNote("key missing; NullPointerException", listOf("SeedKeystore.secretKey(SeedKeystore.kt:40)")),
        )
        assertTrue(r, r.contains("Xiaomi Redmi Note 11 Pro 5G"))
        assertTrue(r, r.contains("Android 13 (SDK 33)"))
        assertTrue(r, r.contains("key fortis.app: MISSING"))
        assertTrue(r, r.contains("screen lock set: yes"))
        assertTrue(r, r.contains("wrapped secret: present (88 chars)"))
        assertTrue(r, r.contains("last successful unlock: never recorded"))
        assertTrue(r, r.contains("device secure: true -> false"))
        assertTrue(r, r.contains("SeedKeystore.kt:40"))
        assertTrue(r, r.contains("fortis.swap"))
    }

    @Test
    fun reportNeverContainsSecretsOrWalletData() {
        val r = formatKeyReport(snap, 0L, 0L, emptyList(), null)
        assertFalse(r.contains("seed", ignoreCase = true))
        assertFalse(r.contains("phrase", ignoreCase = true))
        assertFalse(r.contains("address", ignoreCase = true))
    }

    @Test
    fun healthyKeyReportsPresentAndItsProperties() {
        val r = formatKeyReport(snap.copy(keyPresence = KeyPresence.PRESENT, keyInfo = "TEE, auth required"), 5L, 3L, emptyList(), null)
        assertTrue(r, r.contains("key fortis.app: present"))
        assertTrue(r, r.contains("TEE, auth required"))
        assertTrue(r, r.contains("last failure: none"))
    }

    @Test
    fun aKeystoreErrorIsReportedAsAnErrorNotAsMissing() {
        // hasKey() used to turn every exception into "no key"; the report must tell them apart.
        val r = formatKeyReport(
            snap.copy(keyPresence = KeyPresence.ERROR, keyNote = "KeyStoreException: Keystore not available"),
            1L, null, emptyList(), null,
        )
        assertTrue(r, r.contains("key fortis.app: ERROR (KeyStoreException: Keystore not available)"))
        assertFalse(r, r.contains("MISSING"))
    }
}
