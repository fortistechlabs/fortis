package com.fortis.wallet.data

import com.fortis.wallet.PUBLIC_BTC_ESPLORA
import com.fortis.wallet.PUBLIC_XBT_ESPLORA
import org.junit.Assert.assertEquals
import org.junit.Test

class ChooseBackendTest {
    @Test
    fun own_server_on_mainnet_has_no_fallback() = assertEquals(
        BackendPlan(BackendTarget.Esplora("https://n/api"), null),
        chooseBackend("btc", "mainnet", "https://n/api"),
    )

    @Test
    fun own_server_off_mainnet_is_ignored() = assertEquals(
        BackendPlan(BackendTarget.Edge, BackendTarget.Esplora(PUBLIC_BTC_ESPLORA)),
        chooseBackend("btc", "regtest", "https://n/api"),
    )

    @Test
    fun no_own_server_is_edge_then_public_btc() = assertEquals(
        BackendPlan(BackendTarget.Edge, BackendTarget.Esplora(PUBLIC_BTC_ESPLORA)),
        chooseBackend("btc", "mainnet", null),
    )

    @Test
    fun no_own_server_is_edge_then_public_xbt() = assertEquals(
        BackendPlan(BackendTarget.Edge, BackendTarget.Esplora(PUBLIC_XBT_ESPLORA)),
        chooseBackend("xbt", "mainnet", null),
    )
}
