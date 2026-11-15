package com.fortis.wallet

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test

class CrashReporterTest {
    @Test
    fun redactsBech32AndBase58Addresses() {
        val bech32 = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq"
        val p2pkh = "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa"
        val p2sh = "3J98t1WpEZ73CNmQviecrnyiWrnqRhWNLy"
        val testnet = "mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn"
        val out = CrashReporter.redact("failed on $bech32 and $p2pkh and $p2sh and $testnet")
        assertEquals("failed on <addr> and <addr> and <addr> and <addr>", out)
        assertFalse(out.contains(bech32))
        assertFalse(out.contains(p2pkh))
        assertFalse(out.contains(p2sh))
        assertFalse(out.contains(testnet))
    }

    @Test
    fun redactsSlip132ExtendedKeysAndHex() {
        val zpub = "zpub6rFR7y4Q2AijBEqTUquhVz398htDFrtymD9xYYfG1m4wAcvPhXNfE3EfH1r1ADqtfSdVCToUG868RvUUkgDKf31mGDtKsAYz2oz2AGutZYs"
        val vprv = "vprv9DMUxX4ShgxMM6df5d5iL6C6vWqK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9qK9"
        val txid = "748496929a0daefc3df9e135a7c5b805bed8ddfbd9f6c4ead83999dfa6552d50"
        val out = CrashReporter.redact("key=$zpub priv=$vprv tx=$txid")
        assertEquals("key=<xkey> priv=<xkey> tx=<hex>", out)
    }
}
