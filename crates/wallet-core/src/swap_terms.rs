//! Terms of a matched P2P swap, and the check each wallet runs before accepting them.

use serde::{Deserialize, Serialize};

use crate::chain::ChainParams;
use crate::error::{Result, WalletError};
use crate::swap::{swap_timelocks, SwapRole};

pub const SWAP_FEE_BPS: u32 = 50;
/// Minimum commission per side, in sats of the chain the sender pays on.
pub const SWAP_FEE_FLOOR_SAT: u64 = 400;
/// Smallest BTC leg: well above dust plus a quiet-day HTLC spend fee (spends
/// are clamped to leave a non-dust output when fees run high).
pub const MIN_BTC_LEG_SAT: u64 = 2_000;
/// The highest price (BTC sat per 1 XBT) an offer or terms may carry: all
/// 21M BTC for one XBT. Keeps every amount conversion far from u64 limits.
pub const MAX_PRICE_SAT_PER_XBT: u64 = 2_100_000_000_000_000;
pub const XBT_CONFS: u32 = 6;
pub const BTC_CONFS: u32 = 2;
pub const ACCEPT_SECS: u32 = 120;
pub const INITIATOR_FUND_SECS: u32 = 1800;
pub const PARTICIPANT_FUND_SECS: u32 = 7200;
pub const MIN_BTC_LEG_LEFT_SECS: u32 = 12 * 3600;
pub const LOCKTIME_TOLERANCE_SECS: u32 = 600;

/// How far ahead of this wallet's clock the edge's `matched_at` may be. Every
/// locktime hangs off `matched_at`, so a match dated in the future would push
/// the refund out by as much: refused beyond this.
pub const MAX_CLOCK_SKEW_SECS: u32 = 600;

/// Everything both wallets must agree on. Locktimes and deadlines are absolute unix seconds.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SwapTerms {
    pub match_id: String,
    /// "bitcoin" | "regtest"
    pub network: String,
    pub xbt_sat: u64,
    pub btc_sat: u64,
    pub price_sat_per_xbt: u64,
    pub fee_bps: u32,
    pub fee_address: String,
    pub matched_at: u32,
    pub xbt_locktime: u32,
    pub btc_locktime: u32,
    pub xbt_confs: u32,
    pub btc_confs: u32,
    pub accept_by: u32,
    pub initiator_fund_by: u32,
    /// The HTLC script both legs use (`htlc::CONTRACT_V1` / `CONTRACT_V2`).
    /// Absent in terms from before versions existed: v1, the script those
    /// swaps were funded with. Left out when 1, so such terms stay as they were.
    #[serde(default = "contract_v1", skip_serializing_if = "is_contract_v1")]
    pub contract_version: u8,
}

fn contract_v1() -> u8 {
    crate::htlc::CONTRACT_V1
}

fn is_contract_v1(v: &u8) -> bool {
    *v == crate::htlc::CONTRACT_V1
}

/// BTC sats owed for `xbt_sat` at `price_sat_per_xbt`, rounded down;
/// saturates at `u64::MAX` rather than wrapping.
pub fn btc_for_xbt(xbt_sat: u64, price_sat_per_xbt: u64) -> u64 {
    (xbt_sat as u128 * price_sat_per_xbt as u128 / 100_000_000).min(u64::MAX as u128) as u64
}

/// Commission one side pays on `send_sat`: `SWAP_FEE_BPS` of it (rounded down),
/// but never less than `SWAP_FEE_FLOOR_SAT`.
pub fn swap_commission_sat(send_sat: u64) -> u64 {
    let pct = (send_sat as u128 * SWAP_FEE_BPS as u128 / 10_000) as u64;
    pct.max(SWAP_FEE_FLOOR_SAT)
}

/// Latest time the participant may still fund its BTC HTLC: it must keep
/// `min(MIN_BTC_LEG_LEFT_SECS, half the BTC leg)` in hand before `btc_locktime`.
/// On mainnet (24 h leg) that is 12 h; short regtest legs keep half.
pub fn participant_fund_latest(t: &SwapTerms) -> u32 {
    let leg = t.btc_locktime.saturating_sub(t.matched_at);
    t.btc_locktime.saturating_sub(MIN_BTC_LEG_LEFT_SECS.min(leg / 2))
}

fn reject<T>(msg: impl Into<String>) -> Result<T> {
    Err(WalletError::TermsRejected(msg.into()))
}

/// `(btc, xbt)` chain params for a terms `network` ("bitcoin" | "regtest").
pub fn network_params(network: &str) -> Result<(ChainParams, ChainParams)> {
    match network {
        "bitcoin" => Ok((ChainParams::bitcoin(), ChainParams::blake2b())),
        "regtest" => Ok((ChainParams::bitcoin_regtest(), ChainParams::blake2b_regtest())),
        other => reject(format!("unknown network {other}")),
    }
}

/// Check matched terms against the user's own limit price and the protocol constants.
/// `role` decides the direction of the limit: the initiator (XBT sender) accepts any
/// price at or above it, the participant (BTC sender) any price at or below it.
/// [`validate_terms`] for terms that must be on `network` (the phone's own:
/// "bitcoin" | "regtest"). The edge chooses the terms' network; a phone never
/// takes terms for another chain.
pub fn validate_terms_on(
    t: &SwapTerms,
    network: &str,
    limit_price_sat_per_xbt: u64,
    role: SwapRole,
    now: u32,
) -> Result<()> {
    if t.network != network {
        return reject(format!("terms are for network {}, not {network}", t.network));
    }
    validate_terms(t, limit_price_sat_per_xbt, role, now)
}

pub fn validate_terms(
    t: &SwapTerms,
    limit_price_sat_per_xbt: u64,
    role: SwapRole,
    now: u32,
) -> Result<()> {
    let (btc, xbt) = network_params(&t.network)?;

    if !matches!(t.contract_version, crate::htlc::CONTRACT_V1 | crate::htlc::CONTRACT_V2) {
        return reject(format!("unknown contract version {}", t.contract_version));
    }
    if t.matched_at as u64 > now as u64 + MAX_CLOCK_SKEW_SECS as u64 {
        return reject("the match is dated in the future");
    }
    if limit_price_sat_per_xbt == 0 {
        return reject("zero limit price");
    }
    if t.price_sat_per_xbt == 0 || t.price_sat_per_xbt > MAX_PRICE_SAT_PER_XBT {
        return reject("price out of range");
    }
    let worse = match role {
        SwapRole::Initiator => t.price_sat_per_xbt < limit_price_sat_per_xbt,
        SwapRole::Participant => t.price_sat_per_xbt > limit_price_sat_per_xbt,
    };
    if worse {
        return reject("price is worse than your limit");
    }
    if t.btc_sat != btc_for_xbt(t.xbt_sat, t.price_sat_per_xbt) {
        return reject("btc amount does not match xbt amount at price");
    }
    if t.btc_sat < MIN_BTC_LEG_SAT {
        return reject("below minimum swap size");
    }
    if t.fee_bps != SWAP_FEE_BPS {
        return reject("unexpected fee");
    }
    if t.xbt_confs != XBT_CONFS || t.btc_confs != BTC_CONFS {
        return reject("unexpected confirmation requirement");
    }

    let tl = swap_timelocks(&btc, &xbt);
    let near = |actual: u32, expected: u64| {
        (actual as u64).abs_diff(expected) <= LOCKTIME_TOLERANCE_SECS as u64
    };
    if !near(t.xbt_locktime, t.matched_at as u64 + tl.xbt_leg_secs as u64)
        || !near(t.btc_locktime, t.matched_at as u64 + tl.btc_leg_secs as u64)
    {
        return reject("locktime does not match agreed timelocks");
    }

    if t.accept_by as u64 != t.matched_at as u64 + ACCEPT_SECS as u64
        || t.initiator_fund_by as u64 != t.accept_by as u64 + INITIATOR_FUND_SECS as u64
    {
        return reject("unexpected deadlines");
    }
    if now > t.accept_by {
        return reject("accept deadline passed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MATCHED_AT: u32 = 1_800_000_000;
    const PRICE: u64 = 1_120_000;

    fn terms() -> SwapTerms {
        let tl = swap_timelocks(&ChainParams::bitcoin(), &ChainParams::blake2b());
        SwapTerms {
            match_id: "m1".into(),
            network: "bitcoin".into(),
            xbt_sat: 100_000_000,
            btc_sat: btc_for_xbt(100_000_000, PRICE),
            price_sat_per_xbt: PRICE,
            fee_bps: SWAP_FEE_BPS,
            fee_address: "bc1qfee".into(),
            matched_at: MATCHED_AT,
            xbt_locktime: MATCHED_AT + tl.xbt_leg_secs,
            btc_locktime: MATCHED_AT + tl.btc_leg_secs,
            xbt_confs: XBT_CONFS,
            btc_confs: BTC_CONFS,
            accept_by: MATCHED_AT + ACCEPT_SECS,
            initiator_fund_by: MATCHED_AT + ACCEPT_SECS + INITIATOR_FUND_SECS,
            contract_version: crate::htlc::CURRENT_CONTRACT,
        }
    }

    const ROLES: [SwapRole; 2] = [SwapRole::Initiator, SwapRole::Participant];

    fn check(t: &SwapTerms, now: u32) -> Result<()> {
        validate_terms(t, PRICE, SwapRole::Initiator, now)
    }

    #[test]
    fn both_roles_derive_equal_locktimes() {
        let a = swap_timelocks(&ChainParams::bitcoin(), &ChainParams::blake2b());
        let b = swap_timelocks(&ChainParams::bitcoin(), &ChainParams::blake2b());
        assert_eq!(a, b);
        assert_eq!(a.xbt_leg_secs, 48 * 3600);
        assert_eq!(a.btc_leg_secs, 24 * 3600);
        assert!(a.xbt_leg_secs > a.btc_leg_secs);
    }

    #[test]
    fn btc_for_xbt_rounds_down() {
        assert_eq!(btc_for_xbt(100_000_000, 1_120_000), 1_120_000);
        assert_eq!(btc_for_xbt(1, 1_120_000), 0);
    }

    #[test]
    fn accepts_honest_terms() {
        for role in ROLES {
            assert!(validate_terms(&terms(), PRICE, role, MATCHED_AT + 10).is_ok());
        }
    }

    #[test]
    fn initiator_rejects_price_below_its_limit() {
        let t = terms(); // price PRICE
        assert!(validate_terms(&t, PRICE, SwapRole::Initiator, MATCHED_AT + 10).is_ok());
        assert!(validate_terms(&t, PRICE - 1, SwapRole::Initiator, MATCHED_AT + 10).is_ok());
        let e = validate_terms(&t, PRICE + 1, SwapRole::Initiator, MATCHED_AT + 10).unwrap_err();
        assert!(e.to_string().contains("worse than your limit"), "{e}");
    }

    #[test]
    fn participant_rejects_price_above_its_limit() {
        let t = terms();
        assert!(validate_terms(&t, PRICE, SwapRole::Participant, MATCHED_AT + 10).is_ok());
        assert!(validate_terms(&t, PRICE + 1, SwapRole::Participant, MATCHED_AT + 10).is_ok());
        assert!(validate_terms(&t, PRICE - 1, SwapRole::Participant, MATCHED_AT + 10).is_err());
    }

    #[test]
    fn btc_for_xbt_saturates() {
        assert_eq!(btc_for_xbt(u64::MAX, u64::MAX), u64::MAX);
        assert_eq!(btc_for_xbt(200_000_000, 9_223_372_036_854_775_808), u64::MAX);
    }

    #[test]
    fn rejects_terms_price_above_max() {
        let mut t = terms();
        t.price_sat_per_xbt = MAX_PRICE_SAT_PER_XBT + 1;
        t.btc_sat = btc_for_xbt(t.xbt_sat, t.price_sat_per_xbt);
        for role in ROLES {
            let e = validate_terms(&t, 1, role, MATCHED_AT + 10).unwrap_err();
            assert!(e.to_string().contains("price out of range"), "{e}");
        }
        // Exactly the cap is fine on price.
        t.price_sat_per_xbt = MAX_PRICE_SAT_PER_XBT;
        t.xbt_sat = 1;
        t.btc_sat = btc_for_xbt(t.xbt_sat, t.price_sat_per_xbt);
        assert!(validate_terms(&t, 1, SwapRole::Initiator, MATCHED_AT + 10).is_ok());
    }

    #[test]
    fn zero_limit_is_rejected() {
        assert!(validate_terms(&terms(), 0, SwapRole::Participant, MATCHED_AT + 10).is_err());
    }

    #[test]
    fn rejects_tampered_amounts() {
        let mut t = terms();
        t.btc_sat += 1;
        assert!(matches!(check(&t, MATCHED_AT + 10), Err(WalletError::TermsRejected(_))));
    }

    #[test]
    fn rejects_wrong_fee() {
        let mut t = terms();
        t.fee_bps = 51;
        assert!(matches!(check(&t, MATCHED_AT + 10), Err(WalletError::TermsRejected(_))));
    }

    #[test]
    fn rejects_tampered_locktimes() {
        let mut t = terms();
        t.btc_locktime += 601;
        assert!(check(&t, MATCHED_AT + 10).is_err());
        t.btc_locktime -= 1;
        assert!(check(&t, MATCHED_AT + 10).is_ok());
        let mut t = terms();
        t.xbt_locktime -= 601;
        assert!(check(&t, MATCHED_AT + 10).is_err());
    }

    #[test]
    fn rejects_wrong_confirmations() {
        let mut t = terms();
        t.xbt_confs = 5;
        assert!(matches!(check(&t, MATCHED_AT + 10), Err(WalletError::TermsRejected(_))));
    }

    #[test]
    fn minimum_leg_is_2_000() {
        let mut t = terms();
        t.xbt_sat = 178_572;
        t.btc_sat = btc_for_xbt(t.xbt_sat, PRICE);
        assert_eq!(t.btc_sat, 2_000);
        assert!(check(&t, MATCHED_AT + 10).is_ok());
        t.xbt_sat = 178_571;
        t.btc_sat = btc_for_xbt(t.xbt_sat, PRICE);
        assert_eq!(t.btc_sat, 1_999);
        let e = check(&t, MATCHED_AT + 10).unwrap_err();
        assert!(e.to_string().contains("below minimum swap size"), "{e}");
    }

    #[test]
    fn swap_commission_crossover() {
        assert_eq!(swap_commission_sat(20_000), 400);
        assert_eq!(swap_commission_sat(79_999), 400);
        assert_eq!(swap_commission_sat(80_000), 400);
        assert_eq!(swap_commission_sat(80_200), 401);
        assert_eq!(swap_commission_sat(200_000), 1_000);
        assert_eq!(swap_commission_sat(u64::MAX), (u64::MAX as u128 * 50 / 10_000) as u64);
    }

    #[test]
    fn rejects_after_accept_deadline() {
        let t = terms();
        assert!(check(&t, t.accept_by).is_ok());
        assert!(matches!(check(&t, t.accept_by + 1), Err(WalletError::TermsRejected(_))));
    }

    /// A hostile edge dating the match years ahead would push every locktime
    /// (and so the refund) out by as much: refused for both roles.
    #[test]
    fn rejects_a_match_dated_in_the_future() {
        let ten_years = 10 * 365 * 24 * 3600;
        for role in ROLES {
            let mut t = terms();
            let at = MATCHED_AT + ten_years;
            t.matched_at = at;
            t.xbt_locktime += ten_years;
            t.btc_locktime += ten_years;
            t.accept_by = at + ACCEPT_SECS;
            t.initiator_fund_by = at + ACCEPT_SECS + INITIATOR_FUND_SECS;
            assert!(matches!(
                validate_terms(&t, PRICE, role, MATCHED_AT),
                Err(WalletError::TermsRejected(m)) if m.contains("future")
            ));
        }
        // A clock a little ahead of ours is fine; past the skew it isn't.
        let t = terms();
        assert!(check(&t, MATCHED_AT - MAX_CLOCK_SKEW_SECS).is_ok());
        assert!(check(&t, MATCHED_AT - MAX_CLOCK_SKEW_SECS - 1).is_err());
    }

    /// The edge chooses the terms' network; the phone must not take terms for
    /// another chain than its own (regtest terms on a mainnet phone).
    #[test]
    fn terms_for_another_network_are_refused() {
        let t = terms();
        let now = t.matched_at + 5;
        assert!(validate_terms_on(&t, "bitcoin", PRICE, SwapRole::Initiator, now).is_ok());
        let e = validate_terms_on(&t, "regtest", PRICE, SwapRole::Initiator, now).unwrap_err();
        assert!(e.to_string().contains("network"), "{e}");
    }

    #[test]
    fn participant_fund_latest_keeps_min_of_12h_and_half_the_btc_leg() {
        let t = terms();
        assert_eq!(participant_fund_latest(&t), t.btc_locktime - 12 * 3600);
        let mut r = terms();
        r.network = "regtest".into();
        r.btc_locktime = r.matched_at + 600;
        assert_eq!(participant_fund_latest(&r), r.btc_locktime - 300);
    }

    #[test]
    fn terms_serde_round_trip() {
        let t = terms();
        let json = serde_json::to_string(&t).unwrap();
        let back: SwapTerms = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }
}
