package com.fortis.wallet.swap

/** Bytes in the OP_RETURN a BTC swap funding carries. */
const val SWAP_REPLAY_GUARD_BYTES = 100

/**
 * The OP_RETURN data a swap funding on [chain] carries: on BTC, 100 fresh
 * random bytes — over the XBT fork's 82-byte consensus cap (its `reduced_data`
 * rule), so the funding can't be replayed onto XBT, where it would lock the
 * same coins' XBT copies in an HTLC the counterparty can redeem. On XBT,
 * `null`: its `SIGHASH_UNIFIED` signatures are already invalid on BTC.
 * (wallet-core adds its own fixed guard to every BTC escrow redeem and refund.)
 */
fun swapFundingGuard(chain: String): ByteArray? =
    if (chain == "btc") com.fortis.wallet.wallet.randomBytes(SWAP_REPLAY_GUARD_BYTES) else null
