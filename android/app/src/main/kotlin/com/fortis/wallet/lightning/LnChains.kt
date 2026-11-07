package com.fortis.wallet.lightning

import com.fortis.wallet.R
import com.fortis.wallet.data.LN_CHAIN
import com.fortis.wallet.data.LN_XBT_CHAIN
import com.fortis.wallet.data.WalletConfig
import uniffi.wallet_ffi.LnChain
import uniffi.wallet_ffi.LnFfiException
import uniffi.wallet_ffi.LnTarget
import uniffi.wallet_ffi.LnTargetChain
import uniffi.wallet_ffi.LnUnifiedLightning

/*
 * Which chain each piece of the two Lightning wallets belongs to: the BTC one ("ln", with
 * the "btc" on-chain wallets) and the XBT one ("ln-xbt", with the "xbt" on-chain wallets).
 * BTC and XBT addresses and invoices look alike, so every choice here is made by the
 * wallet's chain, never by the look of an address.
 */

/** The Lightning wallet chain an on-chain wallet on [chain] moves funds into ("btc" → "ln",
 *  "xbt" → "ln-xbt"); null for anything else. */
fun lightningChainForOnchain(chain: String): String? = when (chain) {
    "btc" -> LN_CHAIN
    "xbt" -> LN_XBT_CHAIN
    else -> null
}

/** The on-chain chain the Lightning wallet on [lnWalletChain] moves funds out to. */
fun onchainChainForLightning(lnWalletChain: String): String? = when (lnWalletChain) {
    LN_CHAIN -> "btc"
    LN_XBT_CHAIN -> "xbt"
    else -> null
}

/** The Lightning wallet chain whose node runs on [chain]. */
fun lightningWalletChain(chain: LnChain): String = when (chain) {
    LnChain.BTC -> LN_CHAIN
    LnChain.XBT -> LN_XBT_CHAIN
}

/** The Lightning wallet that pays a target of [target]'s chain, scanned or opened from a
 *  wallet on [fromChain]. An LNURL or Lightning address (chain unknown until its invoice is
 *  fetched) goes to [fromChain]'s own Lightning wallet; the invoice is checked when paid. */
fun lightningChainForTarget(target: LnTargetChain, fromChain: String): String = when (target) {
    LnTargetChain.BTC -> LN_CHAIN
    LnTargetChain.XBT -> LN_XBT_CHAIN
    LnTargetChain.UNKNOWN -> if (fromChain == "xbt" || fromChain == LN_XBT_CHAIN) LN_XBT_CHAIN else LN_CHAIN
}

/** A BOLT12 offer (alone or as a BIP21 link's Lightning part) rather than an invoice. */
fun isOfferTarget(target: LnTarget): Boolean =
    target is LnTarget.Bolt12Offer ||
        (target is LnTarget.Unified && target.lightning is LnUnifiedLightning.Bolt12Offer)

/**
 * Null when the Lightning wallet on [walletChain] may pay [target]; otherwise the target's
 * chain. Only an invoice or offer whose chain is the wallet's passes: [LnTargetChain.UNKNOWN]
 * (an LNURL or address not fetched yet, anything unparseable) is never a permission.
 */
fun targetChainRefusal(walletChain: String, target: LnTarget): LnTargetChain? {
    val own = when (walletChain) {
        LN_CHAIN -> LnTargetChain.BTC
        LN_XBT_CHAIN -> LnTargetChain.XBT
        else -> return LnTargetChain.UNKNOWN
    }
    val chain = runCatching { uniffi.wallet_ffi.lnTargetChain(target) }.getOrDefault(LnTargetChain.UNKNOWN)
    return if (chain == own) null else chain
}

/** Throws [LnFfiException.WrongChain] unless a node on [chain] may pay [target] (see [targetChainRefusal]). */
fun requireTargetChain(chain: LnChain, target: LnTarget) {
    if (targetChainRefusal(lightningWalletChain(chain), target) != null) throw LnFfiException.WrongChain()
}

/**
 * The refusal the Lightning wallet on [walletChain] shows for a target on [target]'s chain,
 * exactly as worded in the plan ("This is an XBT Lightning invoice", …), or null when the
 * target is the wallet's own chain. [get] reads a string resource.
 */
fun wrongChainMessage(walletChain: String, target: LnTargetChain, isOffer: Boolean, get: (Int) -> String): String? {
    val res = when (walletChain) {
        LN_CHAIN -> when (target) {
            LnTargetChain.BTC -> return null
            LnTargetChain.XBT -> if (isOffer) R.string.ln_wrong_chain_xbt_offer else R.string.ln_wrong_chain_xbt_invoice
            // Unparseable: no chain to name.
            LnTargetChain.UNKNOWN -> R.string.ln_err_wrong_chain
        }
        // The XBT wallet refuses anything without bit 512: that is BTC.
        LN_XBT_CHAIN -> when (target) {
            LnTargetChain.XBT -> return null
            LnTargetChain.BTC, LnTargetChain.UNKNOWN ->
                if (isOffer) R.string.ln_wrong_chain_btc_offer else R.string.ln_wrong_chain_btc_invoice
        }
        else -> R.string.ln_err_wrong_chain
    }
    return get(res)
}

/** "Pay from your Lightning (XBT) wallet" / "Pay from your Lightning wallet": the button that
 *  hands a refused target to the Lightning wallet on [walletChain]. */
fun payFromOtherWalletRes(walletChain: String): Int =
    if (walletChain == LN_XBT_CHAIN) R.string.ln_pay_from_xbt_wallet else R.string.ln_pay_from_btc_wallet

/** Where the Lightning wallet on [lnWalletChain] moves funds on-chain: the chosen wallet
 *  ([chosenId]) while it is still eligible, else the first eligible one. Only ever a wallet of
 *  its own chain ([lightningSweepWallets]); null when there is none. */
fun moveTargetWallet(wallets: List<WalletConfig>, lnWalletChain: String, chosenId: String?): WalletConfig? {
    val choices = lightningSweepWallets(wallets, lnWalletChain)
    return choices.firstOrNull { it.id == chosenId } ?: choices.firstOrNull()
}

/** The address an orphan transit sweep of the Lightning wallet on [lnWalletChain] pays: the
 *  [moveTargetWallet]'s, read by [address]. Null (nothing is swept) when that chain has no
 *  eligible on-chain wallet. */
suspend fun sweepAddress(
    wallets: List<WalletConfig>,
    lnWalletChain: String,
    chosenId: String?,
    address: suspend (WalletConfig) -> String?,
): String? {
    val w = moveTargetWallet(wallets, lnWalletChain, chosenId) ?: return null
    check(w.chain == onchainChainForLightning(lnWalletChain)) { "sweep target on the wrong chain" }
    return address(w)
}

/** The XBT wallet's Offer card shows only once its LSP returned an offer ([offer]): the XBT LSP
 *  refuses one (`custody_full`, [offerOrNull] → null) while it holds no custody
 *  (`custody_max_sat = 0`). The account's `max_msat` is the per-account cap, not custody, so it
 *  cannot tell. The BTC wallet's always shows, as before. */
fun offerShown(walletChain: String, offer: String?): Boolean =
    walletChain != LN_XBT_CHAIN || offer != null

/** The account's reusable offer; null when its LSP holds no custody (`custody_full`). Any
 *  other failure throws. */
suspend fun FortisAccount.offerOrNull(signer: AccountSigner): String? = try {
    offer(signer)
} catch (e: AccountError.CustodyFull) {
    null
}

/** The app's two Lightning nodes: one session per Lightning wallet chain, each made by [make]
 *  for its own [LnChain] (data dir, LSP, VSS store and settings all follow from that). */
class LightningSessions(make: (walletChain: String, chain: LnChain) -> LightningSession) {
    val btc: LightningSession = make(LN_CHAIN, LnChain.BTC)
    val xbt: LightningSession = make(LN_XBT_CHAIN, LnChain.XBT)

    val all: List<LightningSession> get() = listOf(btc, xbt)

    /** The session of the Lightning wallet on [walletChain]; null for an on-chain chain. */
    fun of(walletChain: String): LightningSession? = when (walletChain) {
        LN_CHAIN -> btc
        LN_XBT_CHAIN -> xbt
        else -> null
    }

    /** The session an on-chain wallet on [chain] moves funds into. */
    fun forOnchain(chain: String): LightningSession? = lightningChainForOnchain(chain)?.let(::of)
}

/** "… send it from a BTC wallet." / "… from an XBT wallet.": an on-chain address pasted into the
 *  Lightning wallet on [lnWalletChain]. */
fun onchainTargetRes(lnWalletChain: String): Int =
    if (lnWalletChain == LN_XBT_CHAIN) R.string.ln_send_onchain_target_xbt else R.string.ln_send_onchain_target

/** The close dialog's "… back to your on-chain BTC / XBT wallet" ([named]: with the wallet's
 *  name as its argument). */
fun closeBodyRes(lnWalletChain: String, named: Boolean): Int = when {
    lnWalletChain == LN_XBT_CHAIN && named -> R.string.settings_lightning_close_body_to_xbt
    lnWalletChain == LN_XBT_CHAIN -> R.string.settings_lightning_close_body_xbt
    named -> R.string.settings_lightning_close_body_to
    else -> R.string.settings_lightning_close_body
}

/** The Lightning settings card's one-line description: Bitcoin / XBT payments. */
fun lightningBodyRes(lnWalletChain: String): Int =
    if (lnWalletChain == LN_XBT_CHAIN) R.string.settings_lightning_body_xbt else R.string.settings_lightning_body

/** The Move tab's help line: to the on-chain Bitcoin / XBT wallet. */
fun toOnchainHelpRes(lnWalletChain: String): Int =
    if (lnWalletChain == LN_XBT_CHAIN) R.string.transfer_to_onchain_help_xbt else R.string.transfer_to_onchain_help

/** "Add a BTC wallet …" / "Add an XBT wallet …": the Lightning wallet on [lnWalletChain] has no
 *  on-chain wallet of its chain to move funds to. */
fun noOnchainWalletRes(lnWalletChain: String): Int =
    if (lnWalletChain == LN_XBT_CHAIN) R.string.error_no_onchain_wallet_for_lightning_xbt
    else R.string.error_no_onchain_wallet_for_lightning

/**
 * What a send from the Lightning wallet on [walletChain] that failed with
 * [LnFfiException.WrongChain] says, for [target]:
 * - an LNURL / Lightning address: the node refused the invoice it returned, which it only does
 *   when that invoice is the other chain's: that chain's copy;
 * - an invoice or offer: its own chain's copy;
 * - anything that can't be classified: the generic wrong-chain copy, naming no chain.
 */
fun wrongChainPayMessage(walletChain: String, target: LnTarget, get: (Int) -> String): String {
    val chain = when {
        target is LnTarget.LnurlPay || target is LnTarget.LightningAddress ->
            if (walletChain == LN_XBT_CHAIN) LnTargetChain.BTC else LnTargetChain.XBT
        else -> targetChainRefusal(walletChain, target)
    }
    if (chain == null || chain == LnTargetChain.UNKNOWN) return get(R.string.ln_err_wrong_chain)
    return wrongChainMessage(walletChain, chain, isOfferTarget(target), get) ?: get(R.string.ln_err_wrong_chain)
}

/** The feerate (sat/vB) for moving funds out to [dest]: its own chain's backend estimate
 *  ([rateOf]), at least 1; 2 only when that estimate can't be had. */
suspend fun moveOutFeerate(dest: WalletConfig, rateOf: suspend (WalletConfig) -> ULong): Long =
    try {
        rateOf(dest).toLong().coerceAtLeast(1L)
    } catch (e: kotlinx.coroutines.CancellationException) {
        throw e
    } catch (e: Exception) {
        2L
    }
