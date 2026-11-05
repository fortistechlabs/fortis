//! Moving a Fortis account into the user's own channel once it passes the
//! per-account cap (spec §7). The decision is pure (`plan`); `main.rs` acts
//! on it every `CHECK` and settles it on LDK events. The balance is held
//! while a conversion runs and debited only when it has arrived (payment
//! succeeded, or the pushed channel is ready), so a conversion that fails
//! leaves the account as it was.

use std::time::Duration;

/// How often accounts over the cap are looked at.
pub const CHECK: Duration = Duration::from_secs(60);

/// The fee for a conversion: 1 % of the amount moved, at least 1,000 sat.
pub fn conversion_fee_msat(moved_msat: u64) -> u64 {
    (moved_msat / 100).max(1_000_000)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    None,
    /// The user's channel has room: pay the balance (less the fee) over it.
    PayOverChannel {
        amount_msat: u64,
    },
    /// Open a new channel to the user with the balance (less the fee) on
    /// their side.
    OpenChannel {
        channel_sat: u64,
        push_msat: u64,
    },
}

/// What to do for one account. `inbound_room_msat`: how much the LSP can
/// still send to the user over existing channels.
pub fn plan(
    balance_msat: u64,
    cap_msat: u64,
    peer_connected: bool,
    inbound_room_msat: u64,
    in_progress: bool,
    channel_min_sat: u64,
) -> Plan {
    if balance_msat <= cap_msat || !peer_connected || in_progress {
        return Plan::None;
    }
    let fee = conversion_fee_msat(balance_msat);
    let Some(moved) = balance_msat.checked_sub(fee).filter(|m| *m > 0) else {
        return Plan::None;
    };
    if inbound_room_msat >= moved {
        return Plan::PayOverChannel { amount_msat: moved };
    }
    // Twice the pushed amount: room for the user to keep receiving.
    let channel_sat = channel_min_sat.max(2 * moved.div_ceil(1_000));
    Plan::OpenChannel {
        channel_sat,
        push_msat: moved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u64 = 50_000_000;

    #[test]
    fn fee_is_one_percent_with_a_1000_sat_floor() {
        assert_eq!(conversion_fee_msat(55_000_000), 1_000_000);
        assert_eq!(conversion_fee_msat(150_000_000), 1_500_000);
    }

    #[test]
    fn nothing_at_or_under_the_cap() {
        assert_eq!(plan(CAP, CAP, true, 0, false, 20_000), Plan::None);
        assert_eq!(plan(1_000, CAP, true, 0, false, 20_000), Plan::None);
    }

    #[test]
    fn nothing_while_offline_or_in_progress() {
        assert_eq!(plan(55_000_000, CAP, false, 0, false, 20_000), Plan::None);
        assert_eq!(plan(55_000_000, CAP, true, 0, true, 20_000), Plan::None);
    }

    #[test]
    fn pays_over_a_channel_with_room() {
        assert_eq!(
            plan(55_000_000, CAP, true, 60_000_000, false, 20_000),
            Plan::PayOverChannel {
                amount_msat: 54_000_000
            }
        );
    }

    #[test]
    fn opens_a_channel_otherwise() {
        assert_eq!(
            plan(55_000_000, CAP, true, 0, false, 20_000),
            Plan::OpenChannel {
                channel_sat: 108_000,
                push_msat: 54_000_000
            }
        );
        // The channel minimum wins when it is larger.
        assert_eq!(
            plan(55_000_000, CAP, true, 0, false, 200_000),
            Plan::OpenChannel {
                channel_sat: 200_000,
                push_msat: 54_000_000
            }
        );
    }
}
