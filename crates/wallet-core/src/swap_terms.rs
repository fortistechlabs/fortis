//! Terms of a matched P2P swap, and the check each wallet runs before accepting them.

use serde::{Deserialize, Serialize};

use crate::chain::ChainParams;
use crate::error::{Result, WalletError};
use crate::swap::{swap_timelocks, SwapRole};

pub const SWAP_FEE_BPS: u32 = 50;
pub const MIN_BTC_LEG_SAT: u64 = 80_000;
pub const XBT_CONFS: u32 = 6;
pub const BTC_CONFS: u32 = 2;
pub const ACCEPT_SECS: u32 = 120;
pub const INITIATOR_FUND_SECS: u32 = 1800;
pub const PARTICIPANT_FUND_SECS: u32 = 7200;
pub const MIN_BTC_LEG_LEFT_SECS: u32 = 12 * 3600;
pub const LOCKTIME_TOLERANCE_SECS: u32 = 600;

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
}

/// BTC sats owed for `xbt_sat` at `price_sat_per_xbt`, rounded down.
pub fn btc_for_xbt(xbt_sat: u64, price_sat_per_xbt: u64) -> u64 {
    (xbt_sat as u128 * price_sat_per_xbt as u128 / 100_000_000) as u64
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

/// Check matched terms against our own price view and the protocol constants.
/// Role-independent today; `role` is kept so role-specific checks can be added.
pub fn validate_terms(
    t: &SwapTerms,
    our_price_sat_per_xbt: u64,
    max_slippage_bps: u32,
    _role: SwapRole,
    now: u32,
) -> Result<()> {
    let (btc, xbt) = network_params(&t.network)?;

    let diff = t.price_sat_per_xbt.abs_diff(our_price_sat_per_xbt) as u128;
    if our_price_sat_per_xbt == 0
        || diff * 10_000 > our_price_sat_per_xbt as u128 * max_slippage_bps as u128
    {
        return reject("price outside slippage");
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
        }
    }

    const ROLES: [SwapRole; 2] = [SwapRole::Initiator, SwapRole::Participant];

    fn check(t: &SwapTerms, now: u32) -> Result<()> {
        validate_terms(t, PRICE, 100, SwapRole::Initiator, now)
    }

    #[test]
    fn both_roles_derive_equal_locktimes() {
        let a = swap_timelocks(&ChainParams::bitcoin(), &ChainParams::blake2b());
        let b = swap_timelocks(&ChainParams::bitcoin(), &ChainParams::blake2b());
        assert_eq!(a, b);
        assert_eq!(a.xbt_leg_secs, 144 * 3600);
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
            assert!(validate_terms(&terms(), PRICE, 100, role, MATCHED_AT + 10).is_ok());
        }
    }

    #[test]
    fn rejects_out_of_slippage_price() {
        let ours = PRICE * 102 / 100;
        let r = validate_terms(&terms(), ours, 100, SwapRole::Participant, MATCHED_AT + 10);
        assert!(matches!(r, Err(WalletError::TermsRejected(_))));
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
    fn rejects_below_minimum() {
        let mut t = terms();
        t.xbt_sat = 7_142_768;
        t.btc_sat = btc_for_xbt(t.xbt_sat, PRICE);
        assert_eq!(t.btc_sat, 79_999);
        assert!(matches!(check(&t, MATCHED_AT + 10), Err(WalletError::TermsRejected(_))));
    }

    #[test]
    fn rejects_after_accept_deadline() {
        let t = terms();
        assert!(check(&t, t.accept_by).is_ok());
        assert!(matches!(check(&t, t.accept_by + 1), Err(WalletError::TermsRejected(_))));
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
