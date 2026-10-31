package com.fortis.wallet.ui

import android.graphics.Bitmap
import android.graphics.Color
import android.net.Uri
import com.fortis.wallet.lightning.LnState
import com.google.zxing.BarcodeFormat
import com.google.zxing.EncodeHintType
import com.google.zxing.qrcode.QRCodeWriter
import com.google.zxing.qrcode.decoder.ErrorCorrectionLevel
import uniffi.wallet_ffi.LnFfiException
import uniffi.wallet_ffi.LnLspLimits
import uniffi.wallet_ffi.LnReceiveQuote
import uniffi.wallet_ffi.LnTarget
import uniffi.wallet_ffi.LnUnifiedLightning
import uniffi.wallet_ffi.LnurlParams
import uniffi.wallet_ffi.lnParse
import uniffi.wallet_ffi.lnParseAt
import uniffi.wallet_ffi.lnResolveLnurl
import java.net.URI

/** Render `text` as a QR code bitmap. Black-on-white so any scanner reads it. */
fun qrBitmap(text: String, sizePx: Int = 640): Bitmap {
    val hints = mapOf(
        EncodeHintType.ERROR_CORRECTION to ErrorCorrectionLevel.M,
        EncodeHintType.MARGIN to 1,
    )
    val matrix = QRCodeWriter().encode(text, BarcodeFormat.QR_CODE, sizePx, sizePx, hints)
    val w = matrix.width
    val h = matrix.height
    val pixels = IntArray(w * h)
    for (y in 0 until h) {
        val row = y * w
        for (x in 0 until w) pixels[row + x] = if (matrix[x, y]) Color.BLACK else Color.WHITE
    }
    return Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888).apply { setPixels(pixels, 0, w, 0, 0, w, h) }
}

/** Bare address from a scanned string — tolerates a `bitcoin:` URI with params. */
fun addressFromScan(raw: String): String =
    raw.trim()
        .removePrefix("bitcoin:")
        .removePrefix("BITCOIN:")
        .substringBefore("?")
        .trim()

enum class ReceiveHint {
    AddAmountForLightning,
    BelowLightningMin,
}

data class ReceiveQr(
    val uri: String,
    val lightning: Boolean,
    val feeSat: Long?,
    val youGetSat: Long?,
    val hint: ReceiveHint?,
)

/** Converts millisatoshis to satoshis for UI display, rounding sub-satoshi
 *  amounts up so a non-zero sub-sat fee or amount never displays as 0. */
fun displaySatFromMsat(msat: ULong): Long =
    if (msat == 0uL) 0L else (((msat - 1uL) / 1_000uL) + 1uL).toLong()

fun buildReceiveQr(
    address: String,
    amountSat: Long?,
    quote: LnReceiveQuote?,
    minLnSat: Long,
    lightningEnabled: Boolean = minLnSat > 0L,
): ReceiveQr {
    if (!lightningEnabled || minLnSat <= 0L) {
        val uri = if (amountSat != null && amountSat > 0L) {
            "bitcoin:$address?amount=${fmtCoin(amountSat)}"
        } else {
            address
        }
        return ReceiveQr(uri = uri, lightning = false, feeSat = null, youGetSat = null, hint = null)
    }

    if (amountSat == null || amountSat <= 0L) {
        return ReceiveQr(
            uri = address,
            lightning = false,
            feeSat = null,
            youGetSat = null,
            hint = ReceiveHint.AddAmountForLightning,
        )
    }

    if (amountSat < minLnSat) {
        return ReceiveQr(
            uri = "bitcoin:$address?amount=${fmtCoin(amountSat)}",
            lightning = false,
            feeSat = null,
            youGetSat = null,
            hint = ReceiveHint.BelowLightningMin,
        )
    }

    if (quote == null) {
        return ReceiveQr(
            uri = "bitcoin:$address?amount=${fmtCoin(amountSat)}",
            lightning = false,
            feeSat = null,
            youGetSat = null,
            hint = null,
        )
    }

    val feeSat = displaySatFromMsat(quote.feeMsat)
    val displayTotalSat = maxOf(amountSat, displaySatFromMsat(quote.amountMsat))
    val youGetSat = (displayTotalSat - feeSat).coerceAtLeast(0L)
    return ReceiveQr(
        uri = "bitcoin:$address?amount=${fmtCoin(amountSat)}&lightning=${quote.invoice}",
        lightning = true,
        feeSat = feeSat,
        youGetSat = youGetSat,
        hint = null,
    )
}

/** Alias kept for on-chain callers expecting a bare address from a scan. */
fun bareAddress(raw: String): String = addressFromScan(raw)

val DEFAULT_LN_LIMITS = LnLspLimits(
    minPaymentMsat = 1_000uL,
    maxPaymentMsat = 500_000_000uL,
    channelMinSat = 100_000uL,
    channelMaxSat = 500_000uL,
)

fun LnUnifiedLightning.toLnTarget(): LnTarget = when (this) {
    is LnUnifiedLightning.Bolt11 -> LnTarget.Bolt11(
        invoice = invoice,
        amountMsat = amountMsat,
        description = description,
        expirySecs = expirySecs,
    )
    is LnUnifiedLightning.Bolt12Offer -> LnTarget.Bolt12Offer(
        offer = offer,
        amountMsat = amountMsat,
        description = description,
    )
}

/** Parse a scanned or pasted payment string via `wallet-ffi`. */
fun parseScan(
    text: String,
    network: String = "bitcoin",
    nowSecs: ULong? = null,
): LnTarget =
    if (nowSecs != null) lnParseAt(text, network, nowSecs)
    else lnParse(text, network)

sealed interface SendPlan {
    data class OnChain(val address: String, val amountSat: Long?) : SendPlan
    data class Lightning(val target: LnTarget, val amountMsat: ULong?) : SendPlan
    data class NeedsAmount(val target: LnTarget, val minMsat: ULong, val maxMsat: ULong) : SendPlan
    data class NeedsMove(val shortfallSat: Long) : SendPlan
    data class Invalid(val reason: String) : SendPlan
}

fun planSend(
    text: String,
    onchainSat: Long,
    ln: LnState,
    network: String,
    nowSecs: ULong? = null,
    limits: LnLspLimits = DEFAULT_LN_LIMITS,
    userAmountMsat: ULong? = null,
    lnurlResolver: (LnTarget) -> LnurlParams = ::lnResolveLnurl,
): SendPlan {
    val trimmed = text.trim()
    if (trimmed.isEmpty()) return SendPlan.Invalid("empty")

    val target = try {
        parseScan(trimmed, network, nowSecs)
    } catch (e: LnFfiException.WrongNetwork) {
        return SendPlan.Invalid("Wrong network for this wallet")
    } catch (e: LnFfiException.InvoiceExpired) {
        return SendPlan.Invalid("Invoice has expired")
    } catch (e: LnFfiException.Other) {
        return SendPlan.Invalid(e.reason)
    } catch (e: Exception) {
        return SendPlan.Invalid(e.message ?: "Invalid payment request")
    }

    val lightningSat = when (ln) {
        is LnState.Ready -> ln.balances.lightningSat.toLong()
        is LnState.Error -> ln.lastBalances.lightningSat.toLong()
        LnState.Off, LnState.Starting -> 0L
    }

    return when (target) {
        is LnTarget.OnChain -> SendPlan.OnChain(
            address = target.address,
            amountSat = target.amountSat?.toLong(),
        )
        is LnTarget.Unified -> {
            val lnPart = target.lightning.toLnTarget()
            val lnAmountMsat = when (val l = target.lightning) {
                is LnUnifiedLightning.Bolt11 -> l.amountMsat
                is LnUnifiedLightning.Bolt12Offer -> l.amountMsat
            } ?: target.amountSat?.let { it * 1_000uL } ?: userAmountMsat

            if (ln is LnState.Ready && lnAmountMsat != null) {
                val neededSat = displaySatFromMsat(lnAmountMsat)
                if (lightningSat >= neededSat) {
                    return SendPlan.Lightning(target = lnPart, amountMsat = lnAmountMsat)
                }
            }
            SendPlan.OnChain(
                address = target.address,
                amountSat = target.amountSat?.toLong(),
            )
        }
        is LnTarget.Bolt11 -> {
            if (ln is LnState.Off) {
                return SendPlan.Invalid("Lightning is disabled — enable it in Settings → Lightning (beta)")
            }
            planLightningWithAmount(
                target = target,
                fixedAmountMsat = target.amountMsat,
                userAmountMsat = userAmountMsat,
                minMsat = maxOf(1_000uL, limits.minPaymentMsat),
                maxMsat = limits.maxPaymentMsat,
                lightningSat = lightningSat,
            )
        }
        is LnTarget.Bolt12Offer -> {
            if (ln is LnState.Off) {
                return SendPlan.Invalid("Lightning is disabled — enable it in Settings → Lightning (beta)")
            }
            planLightningWithAmount(
                target = target,
                fixedAmountMsat = target.amountMsat,
                userAmountMsat = userAmountMsat,
                minMsat = maxOf(1_000uL, limits.minPaymentMsat),
                maxMsat = limits.maxPaymentMsat,
                lightningSat = lightningSat,
            )
        }
        is LnTarget.LnurlPay, is LnTarget.LightningAddress -> {
            if (ln is LnState.Off) {
                return SendPlan.Invalid("Lightning is disabled — enable it in Settings → Lightning (beta)")
            }
            val params = try {
                lnurlResolver(target)
            } catch (e: LnFfiException.Other) {
                return SendPlan.Invalid(e.reason)
            } catch (e: Exception) {
                return SendPlan.Invalid(e.message ?: "Failed to resolve LNURL")
            }
            planLightningWithAmount(
                target = target,
                fixedAmountMsat = null,
                userAmountMsat = userAmountMsat,
                minMsat = params.minMsat,
                maxMsat = params.maxMsat,
                lightningSat = lightningSat,
            )
        }
    }
}

private fun planLightningWithAmount(
    target: LnTarget,
    fixedAmountMsat: ULong?,
    userAmountMsat: ULong?,
    minMsat: ULong,
    maxMsat: ULong,
    lightningSat: Long,
): SendPlan {
    val effectiveMsat = if (fixedAmountMsat != null) {
        fixedAmountMsat
    } else {
        if (userAmountMsat == null || userAmountMsat == 0uL) {
            return SendPlan.NeedsAmount(target = target, minMsat = minMsat, maxMsat = maxMsat)
        }
        if (userAmountMsat < minMsat || userAmountMsat > maxMsat) {
            return SendPlan.Invalid("Amount out of range (${displaySatFromMsat(minMsat)}–${displaySatFromMsat(maxMsat)} sat)")
        }
        userAmountMsat
    }

    val neededSat = displaySatFromMsat(effectiveMsat)
    if (lightningSat < neededSat) {
        return SendPlan.NeedsMove(shortfallSat = neededSat - lightningSat)
    }
    return SendPlan.Lightning(target = target, amountMsat = effectiveMsat)
}

/** Extract a Send-ready string from an incoming `lightning:` or `bitcoin:` URI. */
fun textFromIntent(raw: String?): String? {
    val s = raw?.trim()?.takeIf { it.isNotEmpty() } ?: return null
    return when {
        s.startsWith("lightning:", ignoreCase = true) -> {
            val body = s.substring("lightning:".length).removePrefix("//").trim()
            body.takeIf { it.isNotEmpty() }
        }
        s.startsWith("bitcoin:", ignoreCase = true) -> {
            val body = s.substring("bitcoin:".length).removePrefix("//").trim()
            if (body.isEmpty()) null else "bitcoin:$body"
        }
        else -> null
    }
}

fun textFromIntent(uri: URI?): String? = textFromIntent(uri?.toString())

fun textFromIntent(uri: Uri?): String? = textFromIntent(uri?.toString())
