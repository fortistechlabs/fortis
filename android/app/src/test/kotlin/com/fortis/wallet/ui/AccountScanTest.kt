package com.fortis.wallet.ui

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class AccountScanTest {
    @Test
    fun framesOfAnAnimatedUrAreMultipart() {
        assertTrue(isMultipartUr("ur:crypto-account/1-3/lpadaxcfaxhdclaoweha"))
        // QR alphanumeric mode upper-cases the whole thing.
        assertTrue(isMultipartUr("UR:CRYPTO-ACCOUNT/12-30/LPADAXCFAXHDCLAOWEHA"))
        assertTrue(isMultipartUr("  ur:crypto-hdkey/7-2/lpadaxcf\n"))
    }

    @Test
    fun singlePartUrsAndPlainKeysAreNot() {
        assertFalse(isMultipartUr("ur:crypto-account/oeadcyemrewytyaolttaad"))
        assertFalse(isMultipartUr("zpub6rFR7y4Q2AijBEqTUquhVz398htDFrtymD9xYYfG1m4wAcvPhXNfE3EfH1r1ADqtfSdVCToUG868RvUUkgDKf31mGDtKsAYz2oz2AGutZYs"))
        assertFalse(isMultipartUr("[37b5eed4/84h/0h/0h]xpub6BkU445MSEBXbPjD3g2c2ch6mn8yy1SXXQUM7Ewjg"))
        assertFalse(isMultipartUr("bitcoin:bc1qxyz"))
        assertFalse(isMultipartUr("ur:crypto-account/x-3/lpadaxcf"))
    }
}
