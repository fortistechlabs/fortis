//! Outbound channels to routing nodes, listed in `lsp.toml` as
//!
//! ```toml
//! [[outbound]]
//! peer = "03864ef0…@3.33.236.230:9735"
//! sat = 150000
//! ```
//!
//! They connect the LSP to the wider network: Fortis users pay outside
//! invoices through them, and outside payers find a route to the LSP over
//! them (they are announced, so `alias` and `announce_addresses` are required).
//!
//! The policy is deliberately conservative, because every open spends real
//! money: a target with **any** channel to its peer (pending, ready, or one
//! being closed) is left alone; a target is opened at most once per process
//! run, so a channel the peer closes is not reopened behind the operator's
//! back (a restart re-checks); nothing is opened before the on-chain wallet
//! has synced or while the spendable balance does not cover the channel plus
//! ldk-node's anchor reserve for it (it waits for funds instead of failing);
//! and a failed attempt waits [`RETRY_AFTER`] before the next.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::{Duration, Instant};

use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;

use crate::config::Result;

/// Smallest outbound channel accepted in the config. Below this the channel
/// would be mostly anchor reserve and on-chain fees.
pub const MIN_OUTBOUND_SAT: u64 = 20_000;
/// Largest: a typo guard (1 BTC).
pub const MAX_OUTBOUND_SAT: u64 = 100_000_000;
/// Wait between attempts for a target whose open failed.
pub const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// One `[[outbound]]` entry, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outbound {
    pub node_id: PublicKey,
    pub address: SocketAddress,
    pub sat: u64,
}

/// The TOML as written.
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RawOutbound {
    pub peer: String,
    pub sat: u64,
}

impl Outbound {
    /// `peer` is `<66-hex node id>@<host>:<port>`.
    pub fn parse(raw: &RawOutbound) -> Result<Outbound> {
        let (id, addr) = raw.peer.split_once('@').ok_or_else(|| {
            format!(
                "outbound peer {:?} must be <node_id>@<host>:<port>",
                raw.peer
            )
        })?;
        let node_id = PublicKey::from_str(id.trim()).map_err(|_| {
            format!(
                "outbound peer {:?}: node id must be a 66-hex public key",
                raw.peer
            )
        })?;
        let address = SocketAddress::from_str(addr.trim())
            .map_err(|e| format!("outbound peer {:?}: address: {e:?}", raw.peer))?;
        if !(MIN_OUTBOUND_SAT..=MAX_OUTBOUND_SAT).contains(&raw.sat) {
            return Err(format!(
                "outbound sat ({}) for {id} must be {MIN_OUTBOUND_SAT}..={MAX_OUTBOUND_SAT}",
                raw.sat
            ));
        }
        Ok(Outbound {
            node_id,
            address,
            sat: raw.sat,
        })
    }
}

/// Validates the list: every entry parses, no peer twice.
pub fn parse_all(raw: &[RawOutbound]) -> Result<Vec<Outbound>> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(raw.len());
    for r in raw {
        let o = Outbound::parse(r)?;
        if !seen.insert(o.node_id) {
            return Err(format!("outbound peer {} is listed twice", o.node_id));
        }
        out.push(o);
    }
    Ok(out)
}

/// What the opener remembers within one process run.
#[derive(Debug, Default)]
pub struct State {
    /// Targets an open was started for in this run: never again until restart.
    opened: HashSet<PublicKey>,
    /// Last failed attempt per target.
    failed_at: HashMap<PublicKey, Instant>,
}

impl State {
    pub fn record_opened(&mut self, peer: PublicKey) {
        self.failed_at.remove(&peer);
        self.opened.insert(peer);
    }

    pub fn record_failed(&mut self, peer: PublicKey, now: Instant) {
        self.failed_at.insert(peer, now);
    }
}

/// What the node looks like right now, for [`due`].
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Every peer the node has any channel with (`list_channels`, which
    /// includes unfunded and closing ones).
    pub with_channel: HashSet<PublicKey>,
    pub wallet_synced: bool,
    /// On-chain sat spendable now (already net of existing channels' reserve).
    pub spendable_sat: u64,
    /// The anchor reserve ldk-node keeps back for each new channel.
    pub reserve_per_channel_sat: u64,
}

/// The targets to open now, in config order, as many as the balance covers.
pub fn due<'a>(
    targets: &'a [Outbound],
    snap: &Snapshot,
    state: &State,
    now: Instant,
) -> Vec<&'a Outbound> {
    if !snap.wallet_synced {
        return Vec::new();
    }
    let mut budget = snap.spendable_sat;
    let mut out = Vec::new();
    for t in targets {
        let wanted = !snap.with_channel.contains(&t.node_id)
            && !state.opened.contains(&t.node_id)
            && state
                .failed_at
                .get(&t.node_id)
                .is_none_or(|at| now.saturating_duration_since(*at) >= RETRY_AFTER);
        let needs = t.sat.saturating_add(snap.reserve_per_channel_sat);
        if wanted && needs <= budget {
            budget -= needs;
            out.push(t);
        }
    }
    out
}

/// Targets still missing that the balance does not cover yet: for a
/// "waiting for funds" log line, not an error.
pub fn waiting_for_funds<'a>(
    targets: &'a [Outbound],
    snap: &Snapshot,
    state: &State,
    due: &[&Outbound],
) -> Vec<&'a Outbound> {
    if !snap.wallet_synced {
        return Vec::new();
    }
    targets
        .iter()
        .filter(|t| !snap.with_channel.contains(&t.node_id) && !state.opened.contains(&t.node_id))
        .filter(|t| !state.failed_at.contains_key(&t.node_id))
        .filter(|t| !due.iter().any(|d| d.node_id == t.node_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACINQ: &str = "03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f";
    const OTHER: &str = "0349a3c0752709ccdc7c759c87993f80b8e03b4f492e3a400c3304e5b322b44375";

    fn raw(peer: &str, sat: u64) -> RawOutbound {
        RawOutbound {
            peer: peer.into(),
            sat,
        }
    }

    fn target(id: &str) -> Outbound {
        Outbound::parse(&raw(&format!("{id}@3.33.236.230:9735"), 150_000)).unwrap()
    }

    #[test]
    fn parses_ip_and_hostname_peers() {
        let o = Outbound::parse(&raw(&format!("{ACINQ}@3.33.236.230:9735"), 150_000)).unwrap();
        assert_eq!(o.node_id.to_string(), ACINQ);
        assert_eq!(o.address.to_string(), "3.33.236.230:9735");
        assert_eq!(o.sat, 150_000);
        let o = Outbound::parse(&raw(&format!("{ACINQ}@node.example.com:9735"), 20_000)).unwrap();
        assert_eq!(o.address.to_string(), "node.example.com:9735");
    }

    #[test]
    fn rejects_bad_peers_and_amounts() {
        for (peer, sat, needle) in [
            ("3.33.236.230:9735", 150_000, "<node_id>@"),
            ("abcd@3.33.236.230:9735", 150_000, "node id"),
            (&format!("{ACINQ}@3.33.236.230"), 150_000, "address"),
            (&format!("{ACINQ}@3.33.236.230:9735"), 19_999, "sat"),
            (&format!("{ACINQ}@3.33.236.230:9735"), 100_000_001, "sat"),
        ] {
            let e = Outbound::parse(&raw(peer, sat)).unwrap_err();
            assert!(e.contains(needle), "{peer} {sat}: {e}");
        }
    }

    #[test]
    fn rejects_a_peer_listed_twice() {
        let p = format!("{ACINQ}@3.33.236.230:9735");
        let e = parse_all(&[raw(&p, 100_000), raw(&p, 200_000)]).unwrap_err();
        assert!(e.contains("twice"), "{e}");
        assert_eq!(parse_all(&[]).unwrap(), vec![]);
    }

    fn snap(with_channel: &[PublicKey], synced: bool, spendable_sat: u64) -> Snapshot {
        Snapshot {
            with_channel: with_channel.iter().copied().collect(),
            wallet_synced: synced,
            spendable_sat,
            reserve_per_channel_sat: 25_000,
        }
    }

    const PLENTY: u64 = 10_000_000;

    #[test]
    fn opens_only_missing_channels_and_only_after_the_wallet_synced() {
        let targets = vec![target(ACINQ), target(OTHER)];
        let now = Instant::now();
        let state = State::default();
        assert!(
            due(&targets, &snap(&[], false, PLENTY), &state, now).is_empty(),
            "wallet not synced"
        );
        assert_eq!(
            due(&targets, &snap(&[], true, PLENTY), &state, now).len(),
            2
        );
        let d = due(
            &targets,
            &snap(&[targets[0].node_id], true, PLENTY),
            &state,
            now,
        );
        assert_eq!(d, vec![&targets[1]]);
    }

    #[test]
    fn waits_for_funds_covering_the_channel_and_its_reserve() {
        let targets = vec![target(ACINQ), target(OTHER)]; // 150 000 sat each
        let now = Instant::now();
        let state = State::default();
        // 175 000 = one channel + one reserve; 174 999 is not enough.
        let s = snap(&[], true, 174_999);
        assert!(due(&targets, &s, &state, now).is_empty());
        assert_eq!(waiting_for_funds(&targets, &s, &state, &[]).len(), 2);
        let s = snap(&[], true, 175_000);
        let d = due(&targets, &s, &state, now);
        assert_eq!(d, vec![&targets[0]], "the budget covers only the first");
        assert_eq!(
            waiting_for_funds(&targets, &s, &state, &d),
            vec![&targets[1]]
        );
        assert_eq!(
            due(&targets, &snap(&[], true, 350_000), &state, now).len(),
            2
        );
    }

    #[test]
    fn never_reopens_in_the_same_run() {
        let targets = vec![target(ACINQ)];
        let mut state = State::default();
        state.record_opened(targets[0].node_id);
        // The channel is gone (closed by the peer): still not reopened.
        let later = Instant::now() + Duration::from_secs(24 * 3600);
        assert!(due(&targets, &snap(&[], true, PLENTY), &state, later).is_empty());
    }

    #[test]
    fn a_failed_open_waits_before_retrying() {
        let targets = vec![target(ACINQ)];
        let mut state = State::default();
        let t0 = Instant::now();
        state.record_failed(targets[0].node_id, t0);
        let s = snap(&[], true, PLENTY);
        assert!(due(
            &targets,
            &s,
            &state,
            t0 + RETRY_AFTER - Duration::from_secs(1)
        )
        .is_empty());
        assert_eq!(due(&targets, &s, &state, t0 + RETRY_AFTER).len(), 1);
    }
}
