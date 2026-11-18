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

/** What the LSP keeps back from the Fortis balance for the routing fee of an offer payment (the
 *  server's own cap, `fee_cap_msat`): 1 % of the amount plus 50 sat. */
fun offerReserveMsat(amountMsat: ULong): ULong = amountMsat / 100uL + 50_000uL

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

/** True when the account receipt [e] is the credit of a move to the Fortis balance that this phone's own offer
 *  payment made: its invoice was paid ([topUps], by payment hash), or one for exactly [inFlightMsat] is being
 *  paid right now. Every other receipt, even during a send, is a real one and is announced. */
fun isOwnTopUpReceipt(e: AccountEntry, topUps: Set<String>, inFlightMsat: ULong?): Boolean =
    e.kind == "received" && (
        (e.paymentHash != null && e.paymentHash in topUps) ||
            (inFlightMsat != null && e.amountMsat == inFlightMsat.toLong())
        )
