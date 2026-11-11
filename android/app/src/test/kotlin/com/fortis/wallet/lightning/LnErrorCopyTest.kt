package com.fortis.wallet.lightning

import com.fortis.wallet.R
import com.fortis.wallet.data.WalletConfig
import com.fortis.wallet.ui.screens.lnErrorMessage
import com.fortis.wallet.ui.screens.shouldShowTakeoverBanner
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.w3c.dom.Element
import uniffi.wallet_ffi.LnBalances
import uniffi.wallet_ffi.LnChannelState
import uniffi.wallet_ffi.LnFfiException
import java.io.File
import java.lang.reflect.Modifier
import javax.xml.parsers.DocumentBuilderFactory

class LnErrorCopyTest {
    private fun loadStringsXml(): Map<String, String> {
        val doc = DocumentBuilderFactory.newInstance()
            .newDocumentBuilder()
            .parse(File("src/main/res/values/strings.xml"))
            .documentElement
        val list = doc.getElementsByTagName("string")
        val map = LinkedHashMap<String, String>()
        for (i in 0 until list.length) {
            val el = list.item(i) as Element
            map[el.getAttribute("name")] = el.textContent
        }
        return map
    }

    private fun resourceNameForId(resId: Int): String? =
        R.string::class.java.declaredFields.firstOrNull { f ->
            Modifier.isStatic(f.modifiers) && f.type == Int::class.javaPrimitiveType && f.getInt(null) == resId
        }?.name

    @Test
    fun every_error_variant_has_a_message() {
        val strings = loadStringsXml()
        val variants: List<LnFfiError> = listOf(
            LnFfiException.BackupUnavailable(),
            LnFfiException.ActiveOnAnotherDevice(),
            LnFfiException.LspUnreachable(),
            LnFfiException.InsufficientLightning(),
            LnFfiException.NoRoute(),
            LnFfiException.InvoiceExpired(),
            LnFfiException.WrongNetwork(),
            LnFfiException.WrongChain(),
            LnFfiException.PaymentStateUnknown(),
            LnFfiException.AmountOutOfRange(minMsat = 1_000uL, maxMsat = 500_000_000uL),
            LnFfiException.Other(reason = "unexpected"),
        )

        val declaredVariantClasses = LnFfiException::class.java.declaredClasses
            .filter { LnFfiException::class.java.isAssignableFrom(it) }
            .toSet()
        assertEquals(
            "all declared LnFfiException subclasses must be enumerated",
            declaredVariantClasses,
            variants.map { it.javaClass }.toSet(),
        )

        for (v in variants) {
            val resId = lnErrorMessage(v)
            assertTrue("expected non-zero string resource id for $v", resId != 0)
            val key = resourceNameForId(resId)
            assertNotNull("resource id $resId for $v not found on R.string", key)
            val copy = strings[key]
            assertFalse("missing or blank string in strings.xml for $key ($v)", copy.isNullOrBlank())
        }
    }

    @Test
    fun backup_unavailable_text_says_try_again_shortly() {
        val strings = loadStringsXml()
        val resId = lnErrorMessage(LnFfiException.BackupUnavailable())
        assertEquals(R.string.ln_err_backup_unavailable, resId)
        val key = resourceNameForId(resId)!!
        val copy = strings.getValue(key)
        assertTrue(
            "expected 'try again shortly' in '$copy'",
            copy.contains("try again shortly", ignoreCase = true),
        )
    }

    @Test
    fun active_elsewhere_shows_takeover_action() = runBlocking {
        val strings = loadStringsXml()
        val activeErr = LnState.Error(LnFfiException.ActiveOnAnotherDevice())
        assertTrue(shouldShowTakeoverBanner(activeErr))
        assertFalse(shouldShowTakeoverBanner(LnState.Error(LnFfiException.BackupUnavailable())))
        assertFalse(shouldShowTakeoverBanner(LnState.Off))

        val takeoverActionKey = resourceNameForId(R.string.ln_takeover_action)
        assertNotNull(takeoverActionKey)
        assertEquals("Use Lightning here instead", strings[takeoverActionKey])

        // Verify takeover() claims the session and moves state from Error(ActiveOnAnotherDevice) to Ready.
        var startShouldFailConflict = true
        var takeoverCalled = false
        val fakeNode = object : LnNodeApi {
            override fun start() {
                if (startShouldFailConflict) throw LnFfiException.ActiveOnAnotherDevice()
            }
            override fun takeover() {
                takeoverCalled = true
                startShouldFailConflict = false
            }
            override fun stop() {}
            override fun balances(): LnBalances = LnBalances(
                lightningSat = 120_000uL,
                onchainTransitSat = 0uL,
                pendingCloseSat = 0uL,
                reservedSat = 1_000uL,
                pendingTransitSat = 0uL,
                inboundSat = 380_000uL,
            )
            override fun channelState(): LnChannelState = LnChannelState.READY
        }

        val w = WalletConfig(
            id = "w-btc",
            name = "Main",
            chain = "ln",
            network = "mainnet",
            sealed = "deadbeef",
            salt = "cafebabe",
        )
        val store = InMemoryLightningStore(wallets = mapOf(w.id to w))
        val session = LightningSession(
            scope = CoroutineScope(SupervisorJob() + Dispatchers.Unconfined),
            store = store,
            nodeFactory = { fakeNode },
            info = LnInfoApi {
                LnInfo(
                    nodeId = "02" + "11".repeat(32),
                    host = "lsp.fortis.test",
                    port = 9735,
                    lsps2Token = "tok",
                    feePpm = 10_000L,
                    feeMinSat = 1_000L,
                    minPaymentSat = 10_000L,
                    maxPaymentSat = 500_000L,
                    channelMinSat = 100_000L,
                    channelMaxSat = 500_000L,
                )
            },
            chain = uniffi.wallet_ffi.LnChain.BTC,
        )

        session.onUnlocked(w.id, ByteArray(64) { 1 }).join()
        assertTrue(shouldShowTakeoverBanner(session.state.value))

        session.takeover().join()
        assertTrue(takeoverCalled)
        assertTrue(session.state.value is LnState.Ready)
        assertFalse(shouldShowTakeoverBanner(session.state.value))
    }
}
