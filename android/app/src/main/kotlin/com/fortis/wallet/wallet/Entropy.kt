package com.fortis.wallet.wallet

import java.io.ByteArrayOutputStream
import kotlin.math.min

/**
 * Supplementary entropy for seed generation. `SecureRandom` already gives 256
 * bits and is sound on modern Android, but a wallet is worth hedging: whatever
 * is gathered here is *mixed* with the CSPRNG bytes inside wallet-ffi
 * (`wallet_core::entropy`) — it can only strengthen the seed, never weaken it,
 * so collection is best-effort and the user can skip.
 *
 * The novel bit for a phone: raw motion-sensor samples while the user shakes it.
 * The low bits of MEMS accelerometer / gyroscope readings are genuine thermal
 * and mechanical noise, and a human's chaotic shake piles more on top.
 */
class EntropyCollector {
    private val buf = ByteArrayOutputStream()
    private var estBits = 0.0

    /** Conservative lower-bound estimate of bits gathered. */
    val bits: Int get() = estBits.toInt()

    /** One sensor reading (accelerometer m/s² or gyroscope rad/s, 3 axes). */
    @Synchronized
    fun addMotion(values: FloatArray) {
        for (v in values) {
            val bits = java.lang.Float.floatToRawIntBits(v)
            buf.write(bits and 0xff)
            buf.write((bits ushr 8) and 0xff)
        }
        buf.write((System.nanoTime() and 0xff).toInt())
        estBits += 2.0 // ~2 bits/sample, conservative
    }

    /** One point of a touch drag: coordinates + event time. */
    @Synchronized
    fun addTouch(x: Float, y: Float, eventNanos: Long) {
        val xi = x.toInt(); val yi = y.toInt()
        buf.write(xi and 0xff); buf.write((xi ushr 8) and 0xff)
        buf.write(yi and 0xff); buf.write((yi ushr 8) and 0xff)
        val t = eventNanos
        buf.write((t and 0xff).toInt()); buf.write(((t ushr 8) and 0xff).toInt())
        estBits += 3.0
    }

    /** Timing jitter of a tight loop — CPU scaling / scheduling noise. Runs with
     *  no user interaction. */
    @Synchronized
    fun addJitter(iterations: Int = 4000) {
        var acc = System.nanoTime()
        var last = acc
        repeat(iterations) {
            var s = 0.0
            for (i in 0 until 200) s += Math.sqrt((i + 1).toDouble())
            val now = System.nanoTime()
            buf.write(((now - last) and 0xff).toInt())
            buf.write((s.toRawBits() and 0xff).toInt())
            last = now
            acc += now
        }
        estBits += iterations * 0.2
    }

    @Synchronized
    fun bytes(): ByteArray = buf.toByteArray()
}

/** Target extra bits before the meter reads "full" (the base seed is already
 *  256-bit; this is headroom). */
const val ENTROPY_TARGET_BITS = 128

fun entropyProgress(bits: Int): Float = min(1f, bits / ENTROPY_TARGET_BITS.toFloat())
