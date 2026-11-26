package com.fortis.wallet.lightning

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** The name rules, identical to the server's table (crates/fortis-edge names). */
class LightningAddressNameTest {
    private val a33 = "a".repeat(33)

    @Test
    fun validNames() {
        for (n in listOf("abc", "donal", "a.b", "a_b-c", "x1y", "0day", "a".repeat(32))) {
            assertNull("'$n'", checkName(n))
        }
    }

    @Test
    fun badNamesNameTheirRule() {
        val table = listOf(
            "ab" to NameProblem.Length, "" to NameProblem.Length, a33 to NameProblem.Length,
            "Donal" to NameProblem.Characters, "dön" to NameProblem.Characters, "a b" to NameProblem.Characters,
            "a@b" to NameProblem.Characters, "a/b" to NameProblem.Characters,
            ".abc" to NameProblem.Edges, "abc." to NameProblem.Edges, "-abc" to NameProblem.Edges, "abc_" to NameProblem.Edges,
            "a..b" to NameProblem.DoubleDot,
            "admin" to NameProblem.Reserved, "lnurlp" to NameProblem.Reserved, "fortis" to NameProblem.Reserved,
        )
        for ((n, p) in table) assertEquals("'$n'", p, checkName(n))
    }

    @Test
    fun charactersAreCheckedBeforeLength() {
        assertEquals(NameProblem.Characters, checkName("A"))
    }

    @Test
    fun everyReservedNameIsRefused() {
        val reserved = "abuse admin administrator api billing bitcoin btc contact dns ftp fortis help hostmaster imap info " +
            "lightning lnurlp mail no-reply noreply postmaster pop root sales satoshi security smtp staff status " +
            "support team wallet webmaster www xbt"
        for (n in reserved.split(' ')) assertEquals(n, NameProblem.Reserved, checkName(n))
    }

    @Test
    fun theAppLowercasesAndTrimsFirst() {
        assertEquals("donal", normalizeName("  Donal "))
        assertNull(checkName(normalizeName("Donal")))
        assertEquals("a b", normalizeName("A B"))
    }

    @Test
    fun availabilityFromThePublicLookup() {
        assertEquals(NameAvailability.Taken, parseAvailability(200, """{"tag":"payRequest","callback":"x"}"""))
        assertEquals(NameAvailability.Available, parseAvailability(200, """{"status":"ERROR","reason":"not found"}"""))
        assertEquals(NameAvailability.Unknown, parseAvailability(503, """{"status":"ERROR"}"""))
        assertEquals(NameAvailability.Unknown, parseAvailability(200, "<html>"))
        assertEquals(NameAvailability.Unknown, parseAvailability(200, "{}"))
    }
}
