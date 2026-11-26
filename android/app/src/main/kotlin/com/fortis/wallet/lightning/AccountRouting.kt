package com.fortis.wallet.lightning

/**
 * Where a Lightning wallet receive or send goes (spec §7): its own channel
 * when that can take it, the LSP's JIT channel for a first receive big
 * enough to open one, otherwise the Fortis account.
 */

enum class ReceiveRoute { OwnInvoice, JitChannel, Account }

/** [inboundRoomMsat]: the most one payment can bring in over a channel now. */
fun receiveRoute(amountMsat: ULong, inboundRoomMsat: ULong, jitMinMsat: ULong): ReceiveRoute = when {
    amountMsat <= inboundRoomMsat -> ReceiveRoute.OwnInvoice
    amountMsat >= jitMinMsat -> ReceiveRoute.JitChannel
    else -> ReceiveRoute.Account
}

enum class PayRoute { Channel, Account, Insufficient, ChannelViaAccount }

/** The most routing fee a payment of [amountMsat] may cost: 1 % of it, at least 1 sat. One rule for all three
 *  sides: the LSP holds this back for an account send (`fee_cap_msat`), the node hands it to LDK as the payment's
 *  limit (`routing_fee_cap_msat`), and the app checks the balance against it. */
fun routingFeeCapMsat(amountMsat: ULong): ULong = maxOf(amountMsat / 100uL, 1_000uL)

/** What the LSP keeps back from the Fortis balance for the routing fee of an offer payment (its own cap). */
fun offerReserveMsat(amountMsat: ULong): ULong = routingFeeCapMsat(amountMsat)

/** The most that one payment can carry out of [availableMsat]: the largest whole-sat amount whose own fee cap
 *  ([routingFeeCapMsat]) still fits beside it. Zero when not even 1 sat plus its 1 sat cap fits. This is what a
 *  "send all" button sends: the balance less the most the route may charge, so nothing is refused for fees. */
fun sendAllSat(availableMsat: ULong): ULong {
    // From 100 sat up the cap is 1 %, so amount * 1.01 must fit; below that the cap is its 1 sat minimum.
    val byPercent = availableMsat / 1_010uL
    if (byPercent >= 100uL) return byPercent
    return if (availableMsat >= 2_000uL) (availableMsat - 1_000uL) / 1_000uL else 0uL
}

/** Channel first, then the account; each must cover the amount plus the fee cap. A BOLT12 offer
 *  ([isOffer]) goes through the account first: its LSP asks the recipient for the invoice, which the
 *  phone often cannot (no route, or a recipient only reachable over Tor). When only the channel
 *  can pay it, an offer first moves the amount plus [offerReserveMsat] into the account
 *  ([PayRoute.ChannelViaAccount]), if that stays within the account's [accountMaxMsat]. */
fun payRoute(
    amountMsat: ULong, feeCapMsat: ULong, sendableMsat: ULong, accountFreeMsat: ULong, isOffer: Boolean = false,
    accountMaxMsat: ULong = 0uL, accountBalanceMsat: ULong = 0uL,
): PayRoute {
    val reserve = offerReserveMsat(amountMsat)
    return when {
        isOffer && accountFreeMsat >= amountMsat + reserve -> PayRoute.Account
        isOffer && accountMaxMsat > 0uL && sendableMsat >= amountMsat + reserve + feeCapMsat &&
            accountBalanceMsat + amountMsat + reserve <= accountMaxMsat -> PayRoute.ChannelViaAccount
        sendableMsat >= amountMsat + feeCapMsat -> PayRoute.Channel
        accountFreeMsat >= amountMsat + feeCapMsat -> PayRoute.Account
        else -> PayRoute.Insufficient
    }
}

/** The channel payment of a move to the Fortis balance whose outcome the app never got (the wait ended, or the
 *  payment was still pending): the newest outgoing payment of exactly [amountMsat] started at or after
 *  [sinceSecs], not failed (a failed one moved nothing) and not already in [known]. Its id is then remembered,
 *  so History explains both sides of the move instead of showing a send and a receipt nobody can account for. */
fun findTopUpPayment(
    payments: List<uniffi.wallet_ffi.LnPayment>,
    amountMsat: ULong,
    sinceSecs: Long,
    known: Set<String>,
): String? = payments
    .filter {
        it.direction == uniffi.wallet_ffi.LnDirection.OUT && it.amountMsat == amountMsat &&
            it.status != uniffi.wallet_ffi.LnPaymentStatus.FAILED &&
            it.timestamp.toLong() >= sinceSecs && it.id !in known
    }
    .maxByOrNull { it.timestamp }?.id

/** True when the account receipt [e] is the credit of a move to the Fortis balance that this phone's own offer
 *  payment made: its invoice was paid ([topUps], by payment hash), or one for exactly [inFlightMsat] is being
 *  paid right now. Every other receipt, even during a send, is a real one and is announced. */
fun isOwnTopUpReceipt(e: AccountEntry, topUps: Set<String>, inFlightMsat: ULong?): Boolean =
    e.kind == "received" && (
        (e.paymentHash != null && e.paymentHash in topUps) ||
            (inFlightMsat != null && e.amountMsat == inFlightMsat.toLong())
        )
