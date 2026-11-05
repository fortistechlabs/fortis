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

enum class PayRoute { Channel, Account, Insufficient }

/** Channel first, then the account; each must cover the amount plus the fee cap. */
fun payRoute(amountMsat: ULong, feeCapMsat: ULong, sendableMsat: ULong, accountFreeMsat: ULong): PayRoute = when {
    sendableMsat >= amountMsat + feeCapMsat -> PayRoute.Channel
    accountFreeMsat >= amountMsat + feeCapMsat -> PayRoute.Account
    else -> PayRoute.Insufficient
}
