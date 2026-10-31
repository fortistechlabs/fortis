package com.fortis.wallet.lightning

import org.json.JSONException
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.Random

/** Task 19: `/ln/info` comes from the network. Whatever it holds, `parseLnInfo`
 *  either returns a document inside the limits or throws one of the two
 *  exceptions the caller expects — never anything else, never a hang. */
class LnInfoFuzzTest {
    private val node = "02" + "11".repeat(32)

    private val valid = """
        {"node_id":"$node","host":"lsp.fortis.test","port":9735,"lsps2_token":"t",
         "fee":{"ppm":10000,"min_sat":1000},
         "min_payment_sat":10000,"max_payment_sat":500000,
         "channel_min_sat":100000,"channel_max_sat":500000,
         "enabled":true,"message":"hi","network":"bitcoin"}
    """.trimIndent()

    private val junk = listOf(
        "null", "true", "-1", "0", "1e999", "99999999999999999999", "\"\"", "\"\u0000\"",
        "\"${"x".repeat(2000)}\"", "[]", "{}", "[[[[]]]]", "9223372036854775807",
        "-9223372036854775808", "2100000000000000", "2100000000000001", "\"02\"", "NaN",
    )

    private fun mutate(r: Random): String {
        val sb = StringBuilder(valid)
        repeat(1 + r.nextInt(5)) {
            when (r.nextInt(4)) {
                0 -> if (sb.isNotEmpty()) sb.setCharAt(r.nextInt(sb.length), (r.nextInt(95) + 32).toChar())
                1 -> if (sb.isNotEmpty()) sb.deleteCharAt(r.nextInt(sb.length))
                2 -> sb.insert(r.nextInt(sb.length + 1), junk[r.nextInt(junk.size)])
                else -> if (sb.isNotEmpty()) sb.setLength(r.nextInt(sb.length + 1))
            }
        }
        return sb.toString()
    }

    private fun check(text: String) {
        val t0 = System.nanoTime()
        val info = try {
            parseLnInfo(text)
        } catch (e: JSONException) {
            return
        } catch (e: IllegalArgumentException) {
            return
        }
        assertTrue("took too long", System.nanoTime() - t0 < 2_000_000_000L)
        assertEquals(66, info.nodeId.length)
        assertTrue(info.nodeId.startsWith("02") || info.nodeId.startsWith("03"))
        assertTrue(info.port in 1..65535)
        assertTrue(info.feePpm in 0..100_000)
        assertTrue(info.minPaymentSat in 0..info.maxPaymentSat)
        assertTrue(info.maxPaymentSat <= MAX_INFO_SAT)
        assertTrue(info.channelMinSat in 1..info.channelMaxSat)
        assertTrue(info.channelMaxSat <= MAX_INFO_SAT)
        assertTrue(info.feeMinSat in 0..MAX_INFO_SAT)
        assertTrue(info.host.length <= 253 && !info.host.contains("://"))
        assertTrue((info.message?.length ?: 0) <= 1000)
        // The conversions the node config makes cannot wrap.
        val limits = info.toLspLimits()
        assertEquals(info.maxPaymentSat.toULong() * 1_000uL, limits.maxPaymentMsat)
    }

    @Test
    fun the_kill_switch_fields_are_read_and_default_to_on() {
        val on = parseLnInfo(valid)
        assertTrue(on.enabled)
        assertEquals("hi", on.message)

        val withoutSwitch = parseLnInfo(valid.replace("\"enabled\":true,\"message\":\"hi\",", ""))
        assertTrue("the fields really were removed", !valid.replace("\"enabled\":true,\"message\":\"hi\",", "").contains("enabled"))
        assertTrue("absent means enabled", withoutSwitch.enabled)
        assertEquals(null, withoutSwitch.message)

        val off = parseLnInfo(
            valid.replace("\"enabled\":true", "\"enabled\":false")
                .replace("\"message\":\"hi\"", "\"message\":\"Paused for upgrade \u2014 exits work.\""),
        )
        assertEquals(false, off.enabled)
        assertEquals("Paused for upgrade \u2014 exits work.", off.message)
        // A blank message is no message.
        assertEquals(null, parseLnInfo(valid.replace("\"message\":\"hi\"", "\"message\":\"  \"")).message)
        // The kill switch is not a way round the limits.
        assertTrue(
            runCatching {
                parseLnInfo(valid.replace("\"port\":9735", "\"port\":0").replace("\"enabled\":true", "\"enabled\":false"))
            }.isFailure,
        )
    }

    @Test
    fun mutated_documents_are_either_within_limits_or_rejected_cleanly() {
        val r = Random(0x19L) // fixed seed: failures reproduce
        repeat(20_000) { check(mutate(r)) }
    }

    @Test
    fun random_text_is_rejected_cleanly() {
        val r = Random(0x1919L)
        repeat(5_000) {
            val n = r.nextInt(600)
            check(String(CharArray(n) { (r.nextInt(0x2FF)).toChar() }))
        }
        check("")
        check("{" + "\"a\":".repeat(3000) + "1}")
        check("[".repeat(5000))
    }
}
