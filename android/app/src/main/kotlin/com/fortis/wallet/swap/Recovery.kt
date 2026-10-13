package com.fortis.wallet.swap

import com.fortis.wallet.data.Recovered
import com.fortis.wallet.data.Step
import com.fortis.wallet.data.SwapRecord
import com.fortis.wallet.data.WalletConfig

/**
 * Find a seed's swaps on the edge after a restore: ask `POST /swap/recover`
 * for the recovery tags of indices 0, 1, 2, … ([tagAt]), [batch] at a time,
 * until [gap] indices in a row past the last known one are unknown — or
 * [limit] indices, so an edge that knows every tag can't keep us asking.
 * Returns what the edge knows with each one's index, and the highest known
 * index (-1 for none), which the seed's counter must then move past.
 */
suspend fun scanTags(
    tagAt: (Int) -> String,
    recover: suspend (List<String>) -> List<Recovered>,
    gap: Int = 20,
    batch: Int = 100,
    limit: Int = 10_000,
): Pair<List<Pair<Int, Recovered>>, Int> {
    val found = mutableListOf<Pair<Int, Recovered>>()
    var highest = -1
    var start = 0
    while (start < limit) {
        val end = minOf(start + batch, limit)
        val byTag = (start until end).associateBy(tagAt)
        for (r in recover(byTag.keys.toList())) {
            val i = byTag[r.tag] ?: continue // a tag we didn't ask for
            found += i to r
            highest = maxOf(highest, i)
        }
        if (end - 1 - highest >= gap) break
        start = end
    }
    return found.sortedBy { it.first } to highest
}

/**
 * The wallets a recovered swap sending [sendChain] runs between: from the
 * seed's own wallet on that chain (its keys came from that seed, and its
 * refund pays there), into the seed's wallet on the other chain — or, if the
 * seed has none (the user's XBT and BTC wallets have different phrases), any
 * of [allWallets] on that chain: the new coins are the user's either way.
 * `null` without a wallet on either chain.
 */
fun recoveryWallets(
    sendChain: String,
    wallets: List<WalletConfig>,
    allWallets: List<WalletConfig> = wallets,
): Pair<WalletConfig, WalletConfig>? {
    val from = wallets.firstOrNull { it.chain == sendChain } ?: return null
    val to = wallets.firstOrNull { it.chain != sendChain }
        ?: allWallets.firstOrNull { it.chain != sendChain }
        ?: return null
    return from to to
}

/**
 * The swap record a restored phone rebuilds from what the edge recovered for
 * the tag of swap [index]: only for a match whose own leg is funded (the edge
 * then issued [Recovered.ownerSecret]), so a rebuilt swap never funds. It is
 * past accepting (the old phone accepted and took the remainder down) and
 * resumes at our funding; the engine rebuilds its session from the relay and
 * checks that it builds the contract this funding pays before anything else.
 */
fun recoveredRecord(
    index: Int, r: Recovered, seed: String, from: WalletConfig, to: WalletConfig,
    wrappedXbt: String, wrappedBtc: String, redeemAddress: String, refundAddress: String, now: Long,
): SwapRecord? {
    val m = r.match ?: return null
    val offerId = r.offerId ?: return null
    val secret = r.ownerSecret ?: return null
    val initiator = when (m.role) {
        "initiator" -> true
        "participant" -> false
        else -> return null
    }
    if (r.direction != if (initiator) "xbt_to_btc" else "btc_to_xbt") return null
    val send = if (initiator) "xbt" else "btc"
    if (from.chain != send || to.chain == send) return null
    val funding = (if (initiator) m.xbtFunding else m.btcFunding) ?: return null
    val amount = m.terms.optLong(if (initiator) "xbt_sat" else "btc_sat", 0)
    val price = m.terms.optLong("price_sat_per_xbt", 0)
    if (amount <= 0 || price <= 0) return null
    return SwapRecord(
        id = offerId, matchId = m.id, role = m.role,
        fromWalletId = from.id, toWalletId = to.id, sendChain = send,
        amountSat = amount, limitPrice = price, ownerSecret = secret,
        swapIndex = index, account = 0, wrappedXbt = wrappedXbt, wrappedBtc = wrappedBtc,
        termsJson = m.terms.toString(),
        fundingTxid = funding.txid, fundingVout = funding.vout,
        step = if (initiator) Step.FUNDED else Step.THEY_FUNDED,
        marks = setOf(ACCEPTED, REMAINDER_CANCELLED, FUNDED_POSTED),
        redeemAddress = redeemAddress, refundAddress = refundAddress,
        createdAt = now, seed = seed, recoveryTag = r.tag,
    )
}
