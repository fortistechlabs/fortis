//! Pairs crossing opposite offers by limit price with partial fills, builds
//! the swap terms, and enforces the accept / funding deadlines (`tick`, run
//! by the watchtower, which also runs `rematch`).
//!
//! Offers cross when the seller's (`XbtToBtc`) price is at or below the
//! buyer's (`BtcToXbt`). The best price goes first (lowest ask for a buyer,
//! highest bid for a seller), then the oldest. The trade runs at the price of
//! the offer that was resting first; a private take at the maker's price.

use std::collections::HashSet;

use anyhow::{anyhow, bail};
use wallet_core::swap_terms::{
    btc_for_xbt, network_params, SwapTerms, ACCEPT_SECS, BTC_CONFS, INITIATOR_FUND_SECS,
    MIN_BTC_LEG_SAT, SWAP_FEE_BPS, XBT_CONFS,
};
use wallet_core::swap_timelocks;

use super::store::{new_id, Direction, Match, MatchStatus, Offer, OfferStatus, Store};
use super::Role;

/// XBT sats bought by `btc_sat` at `price_sat_per_xbt`, rounded down.
pub(crate) fn xbt_for_btc(btc_sat: u64, price_sat_per_xbt: u64) -> u64 {
    (btc_sat as u128 * 100_000_000 / price_sat_per_xbt as u128).min(u64::MAX as u128) as u64
}

/// An offer amount (on its own send chain) expressed as BTC-leg sats.
pub(crate) fn btc_leg_value(direction: Direction, amount_sat: u64, price: u64) -> u64 {
    match direction {
        Direction::XbtToBtc => btc_for_xbt(amount_sat, price),
        Direction::BtcToXbt => amount_sat,
    }
}

fn matchable(o: &Offer, now: u64) -> bool {
    o.status == OfferStatus::Open && o.expires_at > now
}

/// Whether a seller asking `seller_price` and a buyer bidding `buyer_price`
/// (both BTC sat per XBT) can trade.
pub fn crosses(seller_price: u64, buyer_price: u64) -> bool {
    seller_price <= buyer_price
}

/// Whether two opposite offers cross.
fn offers_cross(a: &Offer, b: &Offer) -> bool {
    match a.direction {
        Direction::XbtToBtc => crosses(a.price_sat_per_xbt, b.price_sat_per_xbt),
        Direction::BtcToXbt => crosses(b.price_sat_per_xbt, a.price_sat_per_xbt),
    }
}

/// After taking `consumed` from an offer: the remainder (at the offer's own
/// price) stays `Open` if it is worth at least the minimum, otherwise the
/// offer is done — `Matched` if it was the limiting side, `Filled` if a
/// sub-minimum remainder is being abandoned.
fn settle(o: &mut Offer, consumed: u64, limiting: bool, match_id: &str) {
    o.amount_sat -= consumed;
    if btc_leg_value(o.direction, o.amount_sat, o.price_sat_per_xbt) >= MIN_BTC_LEG_SAT {
        o.status = OfferStatus::Open;
        o.match_id = None;
    } else {
        o.status = if limiting { OfferStatus::Matched } else { OfferStatus::Filled };
        o.match_id = Some(match_id.to_string());
    }
}

/// Builds and stores a match between two opposite offers, or `None` if the
/// fill would be below the minimum. Caller holds the matching lock.
#[allow(clippy::too_many_arguments)]
fn pair(
    store: &Store,
    a: &Offer,
    b: &Offer,
    price: u64,
    now: u64,
    max_btc_sat: u64,
    network: &str,
    fee_address: &str,
) -> anyhow::Result<Option<Match>> {
    if price == 0 {
        bail!("zero price");
    }
    let (mut init, mut part) = match a.direction {
        Direction::XbtToBtc => (a.clone(), b.clone()),
        Direction::BtcToXbt => (b.clone(), a.clone()),
    };
    if init.direction != Direction::XbtToBtc || part.direction != Direction::BtcToXbt {
        bail!("offers are not opposite");
    }

    let part_xbt = xbt_for_btc(part.amount_sat, price);
    let xbt_sat = init.amount_sat.min(part_xbt).min(xbt_for_btc(max_btc_sat, price));
    let btc_sat = btc_for_xbt(xbt_sat, price);
    if btc_sat < MIN_BTC_LEG_SAT {
        return Ok(None);
    }

    let (btc_params, xbt_params) = network_params(network).map_err(|e| anyhow!("{e}"))?;
    let tl = swap_timelocks(&btc_params, &xbt_params);
    let matched_at = u32::try_from(now)?;
    let accept_by = matched_at + ACCEPT_SECS;
    let id = new_id()?;
    let terms = SwapTerms {
        match_id: id.clone(),
        network: network.to_string(),
        xbt_sat,
        btc_sat,
        price_sat_per_xbt: price,
        fee_bps: SWAP_FEE_BPS,
        fee_address: fee_address.to_string(),
        matched_at,
        xbt_locktime: matched_at + tl.xbt_leg_secs,
        btc_locktime: matched_at + tl.btc_leg_secs,
        xbt_confs: XBT_CONFS,
        btc_confs: BTC_CONFS,
        accept_by,
        initiator_fund_by: accept_by + INITIATOR_FUND_SECS,
        contract_version: wallet_core::htlc::CURRENT_CONTRACT,
    };
    let m = Match {
        id: id.clone(),
        terms,
        initiator_offer: init.id.clone(),
        participant_offer: part.id.clone(),
        initiator_token: init.token.clone(),
        participant_token: part.token.clone(),
        initiator_secret_hash: init.secret_hash,
        participant_secret_hash: part.secret_hash,
        accepted: (false, false),
        status: MatchStatus::Proposed,
        xbt_funding: None,
        btc_funding: None,
        participant_fund_by: None,
        pre_match_amounts: (init.amount_sat, part.amount_sat),
        closed_at: None,
        keys: (None, None),
        hashlock: None,
        missed_participant_deadline: false,
        release_txs: None,
        release_sigs: None,
    };

    let init_limiting = xbt_sat == init.amount_sat;
    let part_limiting = xbt_sat == part_xbt;
    settle(&mut init, xbt_sat, init_limiting, &id);
    settle(&mut part, btc_sat, part_limiting, &id);
    store.put_match(&m)?;
    store.put_offer(&init)?;
    store.put_offer(&part)?;
    swap_event!(
        m.id, "matched: offer {} (initiator, XBT) with offer {} (participant, BTC): {} XBT sat for {} BTC sat at {} sat/XBT",
        m.initiator_offer, m.participant_offer, m.terms.xbt_sat, m.terms.btc_sat, m.terms.price_sat_per_xbt
    );
    Ok(Some(m))
}

/// Pairs `incoming` with the best-priced (then oldest) open, public,
/// unexpired opposite offer from a different token that crosses it, at the
/// resting offer's price. `None` leaves `incoming` untouched on the book.
pub fn match_offer(
    store: &Store,
    incoming: &Offer,
    now: u64,
    max_btc_sat: u64,
    network: &str,
    fee_address: &str,
) -> anyhow::Result<Option<Match>> {
    let _g = store.lock_matching();
    match_locked(store, incoming, &HashSet::new(), now, max_btc_sat, network, fee_address)
}

/// [`match_offer`]'s body; the caller holds the matching lock. Candidates
/// whose id is in `barred` are skipped. The trade runs at the price of
/// whichever of the two offers is older (the candidate on a tie).
#[allow(clippy::too_many_arguments)]
fn match_locked(
    store: &Store,
    incoming: &Offer,
    barred: &HashSet<String>,
    now: u64,
    max_btc_sat: u64,
    network: &str,
    fee_address: &str,
) -> anyhow::Result<Option<Match>> {
    let incoming = store.offer(&incoming.id).unwrap_or_else(|| incoming.clone());
    if !matchable(&incoming, now) {
        return Ok(None);
    }
    let mut cands: Vec<Offer> = store
        .open_offers(incoming.direction.opposite())
        .into_iter()
        .filter(|c| {
            !c.private
                && matchable(c, now)
                && c.token != incoming.token
                && !barred.contains(&c.id)
                && offers_cross(&incoming, c)
        })
        .collect();
    match incoming.direction {
        // A seller wants the highest bid, a buyer the lowest ask.
        Direction::XbtToBtc => {
            cands.sort_by_key(|c| (std::cmp::Reverse(c.price_sat_per_xbt), c.created_at))
        }
        Direction::BtcToXbt => cands.sort_by_key(|c| (c.price_sat_per_xbt, c.created_at)),
    }
    for cand in cands {
        let price = if incoming.created_at < cand.created_at {
            incoming.price_sat_per_xbt
        } else {
            cand.price_sat_per_xbt
        };
        if let Some(m) =
            pair(store, &incoming, &cand, price, now, max_btc_sat, network, fee_address)?
        {
            return Ok(Some(m));
        }
    }
    Ok(None)
}

#[cfg(test)]
thread_local! {
    /// Offers [`rematch`] tried to match (tests only).
    pub(super) static REMATCH_TRIED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Watchtower pass: pairs public offers left open together that cross (say,
/// both reopened by deadline cancels of separate matches). Walks them oldest
/// first, one match each per pass, never pairing two offers that already
/// share a cancelled match (a declined pair would loop match → decline).
pub fn rematch(store: &Store, now: u64, max_btc_sat: u64, network: &str, fee_address: &str) {
    let _g = store.lock_matching();
    let cancelled: Vec<(String, String)> = store
        .all_matches()
        .into_iter()
        .filter(|m| m.status == MatchStatus::Cancelled)
        .map(|m| (m.initiator_offer, m.participant_offer))
        .collect();
    let mut open: Vec<Offer> = store
        .all_offers()
        .into_iter()
        .filter(|o| o.status == OfferStatus::Open && !o.private)
        .collect();
    open.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
    // Only an offer that crosses the best opposite one (the highest bid, the
    // lowest ask) can match anything: try just those. A book where nothing
    // crosses — the usual state — then costs this one pass, not a full scan
    // per offer. Matching only fills offers (prices never move), so the best
    // prices read here can only be too good, never miss a match.
    let best = |d: Direction| {
        open.iter().filter(|c| c.direction == d && matchable(c, now)).fold(None::<&Offer>, |b, c| match (b, d) {
            (Some(b), Direction::BtcToXbt) if b.price_sat_per_xbt >= c.price_sat_per_xbt => Some(b),
            (Some(b), Direction::XbtToBtc) if b.price_sat_per_xbt <= c.price_sat_per_xbt => Some(b),
            _ => Some(c),
        })
        .cloned()
    };
    let (best_bid, best_ask) = (best(Direction::BtcToXbt), best(Direction::XbtToBtc));
    open.retain(|o| {
        let opposite = match o.direction {
            Direction::XbtToBtc => &best_bid,
            Direction::BtcToXbt => &best_ask,
        };
        opposite.as_ref().is_some_and(|c| offers_cross(o, c))
    });
    for o in open {
        let barred: HashSet<String> = cancelled
            .iter()
            .filter_map(|(i, p)| {
                if *i == o.id {
                    Some(p.clone())
                } else if *p == o.id {
                    Some(i.clone())
                } else {
                    None
                }
            })
            .collect();
        #[cfg(test)]
        REMATCH_TRIED.with(|t| t.set(t.get() + 1));
        if let Err(e) = match_locked(store, &o, &barred, now, max_btc_sat, network, fee_address) {
            eprintln!("fortis-edge: rematching offer {} failed: {e}", o.id);
        }
    }
}

/// Takes the private offer behind `code` with `taker`, at the maker's price.
pub fn take_private(
    store: &Store,
    code: &str,
    taker: &Offer,
    now: u64,
    max_btc_sat: u64,
    network: &str,
    fee_address: &str,
) -> anyhow::Result<Match> {
    let _g = store.lock_matching();
    let maker = store.offer_by_code(code).ok_or_else(|| anyhow!("unknown share code"))?;
    if !matchable(&maker, now) {
        bail!("offer is no longer available");
    }
    if maker.direction == taker.direction {
        bail!("offer is in the same direction");
    }
    if maker.token == taker.token {
        bail!("cannot take your own offer");
    }
    pair(store, taker, &maker, maker.price_sat_per_xbt, now, max_btc_sat, network, fee_address)?
        .ok_or_else(|| anyhow!("below minimum swap size"))
}

/// Take the public offer `offer_id` with `taker` (opposite direction, the
/// maker's price), as [`take_private`] does by share code: under the matching
/// lock, re-checking that the offer is still open, public and not the taker's.
pub fn take_public(
    store: &Store,
    offer_id: &str,
    taker: &Offer,
    now: u64,
    max_btc_sat: u64,
    network: &str,
    fee_address: &str,
) -> anyhow::Result<Match> {
    let _g = store.lock_matching();
    let maker = store.offer(offer_id).ok_or_else(|| anyhow!("no such offer"))?;
    if maker.private {
        bail!("that offer is private: take it with its share code");
    }
    if !matchable(&maker, now) {
        bail!("offer is no longer available");
    }
    if maker.direction == taker.direction {
        bail!("offer is in the same direction");
    }
    if maker.token == taker.token {
        bail!("cannot take your own offer");
    }
    if maker.price_sat_per_xbt != taker.price_sat_per_xbt {
        bail!("price changed");
    }
    pair(store, taker, &maker, maker.price_sat_per_xbt, now, max_btc_sat, network, fee_address)?
        .ok_or_else(|| anyhow!("below minimum swap size"))
}

#[derive(Clone, Copy)]
enum Release {
    /// Back on the book with what this match took restored.
    Reopen,
    /// Off the book for good.
    Cancel,
}

/// Undo a match's hold on one offer. A fully taken offer gets its pre-match
/// amount back; a partially filled one still on the book gets the consumed
/// amount added back. An offer whose remainder has since moved on is left alone.
fn release(
    store: &Store,
    m: &Match,
    offer_id: &str,
    pre: u64,
    consumed: u64,
    how: Release,
) -> anyhow::Result<()> {
    let Some(mut o) = store.offer(offer_id) else {
        return Ok(());
    };
    if o.match_id.as_deref() == Some(m.id.as_str()) {
        o.match_id = None;
        match how {
            Release::Reopen => {
                o.amount_sat = pre;
                o.status = OfferStatus::Open;
            }
            Release::Cancel => o.status = OfferStatus::Cancelled,
        }
    } else if o.status == OfferStatus::Open {
        match how {
            Release::Reopen => o.amount_sat += consumed,
            Release::Cancel => o.status = OfferStatus::Cancelled,
        }
    } else {
        return Ok(());
    }
    store.put_offer(&o)
}

fn cancel(
    store: &Store,
    mut m: Match,
    now: u64,
    initiator: Release,
    participant: Release,
) -> anyhow::Result<()> {
    m.status = MatchStatus::Cancelled;
    m.closed_at = Some(now);
    store.put_match(&m)?;
    release(store, &m, &m.initiator_offer, m.pre_match_amounts.0, m.terms.xbt_sat, initiator)?;
    release(store, &m, &m.participant_offer, m.pre_match_amounts.1, m.terms.btc_sat, participant)
}

fn tick_match(store: &Store, m: Match, now: u64) -> anyhow::Result<()> {
    match m.status {
        MatchStatus::Proposed if now > m.terms.accept_by as u64 => {
            // Not accepting in time is not griefing: nothing was locked.
            swap_event!(m.id, "cancelled: not accepted by {} (accepted: {:?}); both offers reopened", m.terms.accept_by, m.accepted);
            cancel(store, m, now, Release::Reopen, Release::Reopen)
        }
        MatchStatus::Accepted
            if m.xbt_funding.is_none() && now > m.terms.initiator_fund_by as u64 =>
        {
            store.add_strike(&m.initiator_token, now)?;
            swap_event!(m.id, "cancelled: the initiator didn't fund by {}; initiator struck", m.terms.initiator_fund_by);
            cancel(store, m, now, Release::Cancel, Release::Reopen)
        }
        MatchStatus::InitiatorFunded
            if m.btc_funding.is_none() && m.participant_fund_by.is_some_and(|t| now > t as u64) =>
        {
            let mut m = m;
            // Undone if a BTC funding mined by the deadline turns up late.
            m.missed_participant_deadline = true;
            store.add_strike(&m.participant_token, now)?;
            swap_event!(
                m.id, "cancelled: no BTC funding recorded by {}; participant struck (undone if a funding mined in time turns up)",
                m.participant_fund_by.unwrap_or_default()
            );
            // The initiator's XBT is locked until its refund; don't re-offer it.
            cancel(store, m, now, Release::Cancel, Release::Cancel)
        }
        _ => Ok(()),
    }
}

/// Watchtower housekeeping: deadlines first (which may reopen offers), then expiry.
pub fn tick(store: &Store, now: u64) {
    let _g = store.lock_matching();
    for m in store.all_matches() {
        let id = m.id.clone();
        if let Err(e) = tick_match(store, m, now) {
            eprintln!("fortis-edge: swap tick for match {id} failed: {e}");
        }
    }
    for mut o in store.all_offers() {
        if o.status == OfferStatus::Open && o.expires_at <= now {
            o.status = OfferStatus::Expired;
            if let Err(e) = store.put_offer(&o) {
                eprintln!("fortis-edge: expiring offer {} failed: {e}", o.id);
            }
        }
    }
}

/// A side declined the terms: cancel the match, take the decliner's offer
/// off the book for good (no phone follows it any more, and a reopened one
/// would be paired by every rematch pass), and put the other side's offer
/// back with what the match took. No strike. Only while `Proposed`: once
/// both accepted, the initiator may fund at any moment, and a decline then
/// (say, after seeing that funding in the mempool) would lock its XBT until
/// the refund with no strike; the deadlines handle a side that walks away.
pub(super) fn on_decline(store: &Store, match_id: &str, by: Role) -> anyhow::Result<()> {
    let _g = store.lock_matching();
    let m = store.get_match(match_id).ok_or_else(|| anyhow!("unknown match"))?;
    if m.status != MatchStatus::Proposed || m.xbt_funding.is_some() {
        bail!("match can no longer be declined");
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (initiator, participant) = match by {
        Role::Initiator => (Release::Cancel, Release::Reopen),
        Role::Participant => (Release::Reopen, Release::Cancel),
    };
    swap_event!(m.id, "cancelled: the {} declined", by.as_str());
    cancel(store, m, now, initiator, participant)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swap::store::{Direction, MatchStatus, Offer, OfferStatus, Store};
    use wallet_core::swap_terms::{ACCEPT_SECS, INITIATOR_FUND_SECS};
    use wallet_core::{validate_terms, SwapRole};

    const PRICE: u64 = 1_000_000; // 0.01 BTC per XBT: 1 XBT sat = 0.01 BTC sat
    const NOW: u64 = 1_800_000_000;
    const MAX: u64 = 1_000_000;
    const DAY: u64 = 86_400;

    /// A store in its own temp dir; dropping it closes the store, then
    /// removes the dir.
    struct TempStore {
        store: Store,
        _dir: crate::test_tmp::TempDir,
    }

    impl std::ops::Deref for TempStore {
        type Target = Store;
        fn deref(&self) -> &Store {
            &self.store
        }
    }

    fn temp_store(label: &str) -> TempStore {
        let dir = crate::test_tmp::TempDir::new(&format!("swap-matcher-test-{label}"));
        TempStore { store: Store::open(&dir).unwrap(), _dir: dir }
    }

    fn offer(id: &str, direction: Direction, amount_sat: u64, created_at: u64) -> Offer {
        Offer {
            id: id.into(),
            token: format!("tok-{id}"),
            secret_hash: [id.len() as u8; 32],
            direction,
            amount_sat,
            price_sat_per_xbt: PRICE,
            created_at,
            expires_at: created_at + DAY,
            private: false,
            share_code: None,
            status: OfferStatus::Open,
            match_id: None,
            client_id: None,
            recovery_tag: None,
        }
    }

    /// An offer at `price`, created `t` seconds into the test's book.
    fn priced(id: &str, direction: Direction, amount_sat: u64, price: u64, t: u64) -> Offer {
        Offer { price_sat_per_xbt: price, ..offer(id, direction, amount_sat, NOW - 100 + t) }
    }

    /// Sells `amount_sat` XBT for BTC at no less than `price`.
    fn sell(id: &str, amount_sat: u64, price: u64, t: u64) -> Offer {
        priced(id, Direction::XbtToBtc, amount_sat, price, t)
    }

    /// Buys XBT with `amount_sat` BTC at no more than `price`.
    fn buy(id: &str, amount_sat: u64, price: u64, t: u64) -> Offer {
        priced(id, Direction::BtcToXbt, amount_sat, price, t)
    }

    fn store_with(offers: &[Offer]) -> TempStore {
        let s = temp_store("book");
        for o in offers {
            s.put_offer(o).unwrap();
        }
        s
    }

    fn put(store: &Store, o: &Offer) {
        store.put_offer(o).unwrap();
    }

    fn quick(store: &Store, incoming: &Offer, now: u64) -> Option<crate::swap::store::Match> {
        match_offer(store, incoming, now, MAX, "bitcoin", "bc1qfee").unwrap()
    }

    /// XBT offer x (20M XBT sat = 200k BTC sat) against a 500k BTC offer b.
    fn partial(store: &Store) -> crate::swap::store::Match {
        put(store, &offer("b", Direction::BtcToXbt, 500_000, NOW - 10));
        let x = offer("x", Direction::XbtToBtc, 20_000_000, NOW);
        put(store, &x);
        quick(store, &x, NOW).unwrap()
    }

    #[test]
    fn fifo_pairs_oldest_opposite_offer() {
        let s = temp_store("fifo");
        put(&s, &offer("b1", Direction::BtcToXbt, 200_000, NOW - 50));
        put(&s, &offer("b2", Direction::BtcToXbt, 200_000, NOW - 100));
        put(&s, &offer("b3", Direction::BtcToXbt, 200_000, NOW - 10));
        put(&s, &offer("x_same_dir", Direction::XbtToBtc, 20_000_000, NOW - 200));
        let x = offer("x", Direction::XbtToBtc, 20_000_000, NOW);
        put(&s, &x);

        let m = quick(&s, &x, NOW).unwrap();
        assert_eq!(m.initiator_offer, "x");
        assert_eq!(m.participant_offer, "b2");
        assert_eq!(m.initiator_token, "tok-x");
        assert_eq!(m.participant_token, "tok-b2");
        assert_eq!(m.status, MatchStatus::Proposed);
        assert_eq!(m.terms.xbt_sat, 20_000_000);
        assert_eq!(m.terms.btc_sat, 200_000);
        assert_eq!(s.get_match(&m.id).unwrap(), m);
        for id in ["x", "b2"] {
            let o = s.offer(id).unwrap();
            assert_eq!(o.status, OfferStatus::Matched);
            assert_eq!(o.match_id.as_deref(), Some(m.id.as_str()));
        }
        assert_eq!(s.offer("b1").unwrap().status, OfferStatus::Open);
        assert_eq!(s.offer("b3").unwrap().status, OfferStatus::Open);
    }

    #[test]
    fn partial_fill_leaves_remainder_open() {
        let s = temp_store("partial");
        let m = partial(&s);
        assert_eq!(m.terms.xbt_sat, 20_000_000);
        assert_eq!(m.terms.btc_sat, 200_000);
        assert_eq!(m.pre_match_amounts, (20_000_000, 500_000));
        let b = s.offer("b").unwrap();
        assert_eq!(b.status, OfferStatus::Open);
        assert_eq!(b.amount_sat, 300_000);
        assert_eq!(b.match_id, None);
        assert_eq!(s.offer("x").unwrap().status, OfferStatus::Matched);

        // The XBT side can be the larger one too; its remainder stays in XBT units.
        let s = temp_store("partial-xbt");
        put(&s, &offer("x", Direction::XbtToBtc, 30_000_000, NOW - 10));
        let b = offer("b", Direction::BtcToXbt, 200_000, NOW);
        put(&s, &b);
        let m = quick(&s, &b, NOW).unwrap();
        assert_eq!(m.initiator_offer, "x");
        assert_eq!(m.terms.xbt_sat, 20_000_000);
        let x = s.offer("x").unwrap();
        assert_eq!(x.status, OfferStatus::Open);
        assert_eq!(x.amount_sat, 10_000_000);
    }

    #[test]
    fn remainder_below_minimum_is_filled_not_left_open() {
        let s = temp_store("dust");
        // 200 000 is taken; the 1 000 left is under the 2 000 minimum.
        put(&s, &offer("b", Direction::BtcToXbt, 201_000, NOW - 10));
        let x = offer("x", Direction::XbtToBtc, 20_000_000, NOW);
        put(&s, &x);
        let m = quick(&s, &x, NOW).unwrap();
        let b = s.offer("b").unwrap();
        assert_eq!(b.status, OfferStatus::Filled);
        assert_eq!(b.match_id.as_deref(), Some(m.id.as_str()));
        assert!(s.open_offers(Direction::BtcToXbt).is_empty());
    }

    /// A BTC offer of `btc_sat` against an XBT offer worth 200 000 BTC sat; returns
    /// the BTC offer after the match.
    fn btc_remainder_after_200k(label: &str, btc_sat: u64) -> Offer {
        let s = temp_store(label);
        put(&s, &offer("b", Direction::BtcToXbt, btc_sat, NOW - 10));
        let x = offer("x", Direction::XbtToBtc, 20_000_000, NOW);
        put(&s, &x);
        let m = quick(&s, &x, NOW).unwrap();
        assert_eq!(m.terms.btc_sat, 200_000);
        s.offer("b").unwrap()
    }

    #[test]
    fn remainder_between_20k_and_80k_stays_open() {
        let b = btc_remainder_after_200k("rem-50k", 250_000);
        assert_eq!(b.status, OfferStatus::Open);
        assert_eq!(b.amount_sat, 50_000);
        assert_eq!(b.match_id, None);
    }

    #[test]
    fn remainder_of_exactly_the_minimum_stays_open() {
        let b = btc_remainder_after_200k("rem-2k", 202_000);
        assert_eq!(b.status, OfferStatus::Open);
        assert_eq!(b.amount_sat, 2_000);
        assert_eq!(b.match_id, None);
    }

    #[test]
    fn below_minimum_or_above_cap_is_bounded() {
        let s = temp_store("bounds");
        put(&s, &offer("b", Direction::BtcToXbt, 1_500, NOW - 10));
        let x = offer("x", Direction::XbtToBtc, 20_000_000, NOW);
        put(&s, &x);
        assert!(quick(&s, &x, NOW).is_none());

        let s = temp_store("cap");
        put(&s, &offer("b", Direction::BtcToXbt, 1_500_000, NOW - 10));
        let x = offer("x", Direction::XbtToBtc, 150_000_000, NOW);
        put(&s, &x);
        let m = quick(&s, &x, NOW).unwrap();
        assert_eq!(m.terms.btc_sat, MAX);
        assert_eq!(s.offer("b").unwrap().amount_sat, 500_000);
        assert_eq!(s.offer("x").unwrap().amount_sat, 50_000_000);
    }

    #[test]
    fn private_offer_is_skipped_by_quick_match_and_taken_by_code() {
        let s = temp_store("private");
        let mut p = offer("b", Direction::BtcToXbt, 200_000, NOW - 10);
        p.private = true;
        p.share_code = Some("K7Q2".into());
        put(&s, &p);
        let x = offer("x", Direction::XbtToBtc, 20_000_000, NOW);
        put(&s, &x);
        assert!(quick(&s, &x, NOW).is_none());

        assert!(take_private(&s, "NOPE", &x, NOW, MAX, "bitcoin", "bc1qfee").is_err());
        let same_dir = offer("x2", Direction::BtcToXbt, 200_000, NOW);
        assert!(take_private(&s, "K7Q2", &same_dir, NOW, MAX, "bitcoin", "bc1qfee").is_err());

        let m = take_private(&s, "K7Q2", &x, NOW, MAX, "bitcoin", "bc1qfee").unwrap();
        assert_eq!(m.participant_offer, "b");
        assert_eq!(m.initiator_offer, "x");
        assert_eq!(s.offer("b").unwrap().status, OfferStatus::Matched);
        // Taken once; the code no longer works.
        assert!(take_private(&s, "K7Q2", &x, NOW, MAX, "bitcoin", "bc1qfee").is_err());
    }

    #[test]
    fn expired_offer_is_not_matched() {
        let s = temp_store("expired");
        let mut b = offer("b", Direction::BtcToXbt, 200_000, NOW - DAY - 10);
        b.expires_at = NOW - 10;
        put(&s, &b);
        let x = offer("x", Direction::XbtToBtc, 20_000_000, NOW);
        put(&s, &x);
        assert!(quick(&s, &x, NOW).is_none());

        tick(&s, NOW);
        assert_eq!(s.offer("b").unwrap().status, OfferStatus::Expired);
        assert_eq!(s.offer("x").unwrap().status, OfferStatus::Open);
    }

    fn assert_reopened(s: &Store) {
        let x = s.offer("x").unwrap();
        assert_eq!(
            (x.status, x.amount_sat, x.match_id.clone()),
            (OfferStatus::Open, 20_000_000, None)
        );
        let b = s.offer("b").unwrap();
        assert_eq!((b.status, b.amount_sat, b.match_id), (OfferStatus::Open, 500_000, None));
    }

    #[test]
    fn unaccepted_match_dissolves_after_120s_and_reopens_offers() {
        let s = temp_store("dissolve");
        let mut m = partial(&s);
        m.accepted = (true, false);
        s.put_match(&m).unwrap();

        tick(&s, NOW + 120);
        assert_eq!(s.get_match(&m.id).unwrap().status, MatchStatus::Proposed);
        tick(&s, NOW + 121);
        let m = s.get_match(&m.id).unwrap();
        assert_eq!(m.status, MatchStatus::Cancelled);
        assert_eq!(m.closed_at, Some(NOW + 121));
        assert_reopened(&s);
        assert_eq!(s.strikes("tok-x", NOW + 121), 0);
        assert_eq!(s.strikes("tok-b", NOW + 121), 0);
    }

    /// A decline is followed by no phone: the decliner's offer goes off the
    /// book for good (else rematch keeps pairing a ghost); the other side's
    /// offer reopens with what the match took. No strike either way.
    #[test]
    fn decline_cancels_the_decliners_offer_and_reopens_the_other() {
        // The initiator (x, fully taken) declines.
        let s = temp_store("decline-initiator");
        let m = partial(&s);
        on_decline(&s, &m.id, Role::Initiator).unwrap();
        assert_eq!(s.get_match(&m.id).unwrap().status, MatchStatus::Cancelled);
        assert_eq!(s.offer("x").unwrap().status, OfferStatus::Cancelled);
        let b = s.offer("b").unwrap();
        assert_eq!((b.status, b.amount_sat, b.match_id), (OfferStatus::Open, 500_000, None));
        assert_eq!(s.strikes("tok-x", NOW), 0);
        assert_eq!(s.strikes("tok-b", NOW), 0);

        // The participant (b, partly filled and still on the book) declines.
        let s = temp_store("decline-participant");
        let m = partial(&s);
        on_decline(&s, &m.id, Role::Participant).unwrap();
        assert_eq!(s.get_match(&m.id).unwrap().status, MatchStatus::Cancelled);
        assert_eq!(s.offer("b").unwrap().status, OfferStatus::Cancelled);
        let x = s.offer("x").unwrap();
        assert_eq!((x.status, x.amount_sat, x.match_id), (OfferStatus::Open, 20_000_000, None));
        assert_eq!(s.strikes("tok-x", NOW), 0);
        assert_eq!(s.strikes("tok-b", NOW), 0);

        // Nothing is left for a rematch pass to pair.
        put(&s, &offer("b2", Direction::BtcToXbt, 200_000, NOW));
        rematch(&s, NOW, MAX, "bitcoin", "bc1qfee");
        let live: Vec<_> =
            s.all_matches().into_iter().filter(|m| m.status == MatchStatus::Proposed).collect();
        assert_eq!(live.len(), 1);
        assert_eq!((live[0].initiator_offer.as_str(), live[0].participant_offer.as_str()), ("x", "b2"));
    }

    /// [Final I3] Decline only while `Proposed`.
    #[test]
    fn decline_after_accepted_is_refused() {
        let s = temp_store("decline-accepted");
        let mut m = partial(&s);
        m.accepted = (true, true);
        m.status = MatchStatus::Accepted;
        s.put_match(&m).unwrap();
        assert!(on_decline(&s, &m.id, Role::Initiator).is_err());
        assert!(on_decline(&s, &m.id, Role::Participant).is_err());
        assert_eq!(s.get_match(&m.id).unwrap().status, MatchStatus::Accepted);
        assert_eq!(s.offer("x").unwrap().status, OfferStatus::Matched);
    }

    #[test]
    fn missed_initiator_deadline_cancels_and_strikes() {
        let s = temp_store("initiator-deadline");
        let mut m = partial(&s);
        m.accepted = (true, true);
        m.status = MatchStatus::Accepted;
        s.put_match(&m).unwrap();
        let fund_by = m.terms.initiator_fund_by as u64;

        tick(&s, fund_by);
        assert_eq!(s.get_match(&m.id).unwrap().status, MatchStatus::Accepted);
        tick(&s, fund_by + 1);
        assert_eq!(s.get_match(&m.id).unwrap().status, MatchStatus::Cancelled);
        assert_eq!(s.strikes("tok-x", fund_by + 1), 1);
        assert_eq!(s.strikes("tok-b", fund_by + 1), 0);
        // The honest participant's offer goes back on the book with its full amount.
        let b = s.offer("b").unwrap();
        assert_eq!((b.status, b.amount_sat), (OfferStatus::Open, 500_000));
        assert_eq!(s.offer("x").unwrap().status, OfferStatus::Cancelled);

        // Participant deadline: only once Task 8 sets participant_fund_by.
        let s = temp_store("participant-deadline");
        let mut m = partial(&s);
        m.accepted = (true, true);
        m.status = MatchStatus::InitiatorFunded;
        m.xbt_funding = Some(("aa".repeat(32), 0));
        s.put_match(&m).unwrap();
        tick(&s, NOW + 30 * DAY);
        assert_eq!(s.get_match(&m.id).unwrap().status, MatchStatus::InitiatorFunded);
        m.participant_fund_by = Some((NOW + 5000) as u32);
        s.put_match(&m).unwrap();
        tick(&s, NOW + 5001);
        assert_eq!(s.get_match(&m.id).unwrap().status, MatchStatus::Cancelled);
        assert_eq!(s.strikes("tok-b", NOW + 5001), 1);
        assert_eq!(s.strikes("tok-x", NOW + 5001), 0);
    }

    #[test]
    fn three_strikes_in_24h_counted_fourth_day_resets() {
        let s = temp_store("strikes");
        s.add_strike("t", NOW).unwrap();
        s.add_strike("t", NOW + 3600).unwrap();
        s.add_strike("t", NOW + 7200).unwrap();
        assert_eq!(s.strikes("t", NOW + 7200), 3);
        assert_eq!(s.strikes("t", NOW + DAY + 1), 2);
        assert_eq!(s.strikes("other", NOW + 7200), 0);
        assert_eq!(s.strikes("t", NOW + 3 * DAY), 0);
        s.add_strike("t", NOW + 3 * DAY).unwrap();
        assert_eq!(s.strikes("t", NOW + 3 * DAY), 1);
    }

    #[test]
    fn terms_validate_for_both_roles() {
        let s = temp_store("terms");
        let m = partial(&s);
        let t = &m.terms;
        assert_eq!(t.match_id, m.id);
        assert_eq!(t.matched_at as u64, NOW);
        assert_eq!(t.accept_by, NOW as u32 + ACCEPT_SECS);
        assert_eq!(t.initiator_fund_by, t.accept_by + INITIATOR_FUND_SECS);
        assert_eq!(t.xbt_locktime, NOW as u32 + 48 * 3600);
        assert_eq!(t.btc_locktime, NOW as u32 + 24 * 3600);
        assert_eq!(t.fee_address, "bc1qfee");
        assert_eq!(m.initiator_secret_hash, [1; 32]);
        for role in [SwapRole::Initiator, SwapRole::Participant] {
            validate_terms(t, PRICE, role, NOW as u32 + 5).unwrap();
        }
    }

    #[test]
    fn crossing_is_seller_at_or_below_buyer() {
        assert!(crosses(990_000, 1_000_000));
        assert!(crosses(1_000_000, 1_000_000));
        assert!(!crosses(1_000_001, 1_000_000));
    }

    #[test]
    fn offers_that_do_not_cross_never_match() {
        // seller wants 1_010_000, buyer pays at most 1_000_000
        let s = store_with(&[sell("a", 100_000_000, 1_010_000, 1)]);
        assert!(match_offer(&s, &buy("b", 900_000, 1_000_000, 2), NOW, MAX, "bitcoin", "bc1qfee")
            .unwrap()
            .is_none());
        assert!(s.all_matches().is_empty());
    }

    #[test]
    fn equal_prices_cross() {
        let s = store_with(&[sell("a", 20_000_000, PRICE, 1)]);
        let m = quick(&s, &buy("b", 200_000, PRICE, 2), NOW).unwrap();
        assert_eq!(m.terms.price_sat_per_xbt, PRICE);
        assert_eq!((m.terms.xbt_sat, m.terms.btc_sat), (20_000_000, 200_000));
    }

    #[test]
    fn trade_runs_at_the_resting_price() {
        let s = store_with(&[sell("a", 20_000_000, 990_000, 1)]);
        let m = quick(&s, &buy("b", 200_000, 1_000_000, 2), NOW).unwrap();
        assert_eq!(m.terms.price_sat_per_xbt, 990_000);

        let s = store_with(&[buy("a", 200_000, 1_000_000, 1)]);
        let m = quick(&s, &sell("b", 20_000_000, 990_000, 2), NOW).unwrap();
        assert_eq!(m.terms.price_sat_per_xbt, 1_000_000);
    }

    #[test]
    fn best_price_beats_older_offer() {
        let s = store_with(&[
            sell("old", 20_000_000, 1_000_000, 1),
            sell("new", 20_000_000, 995_000, 2),
        ]);
        let m = quick(&s, &buy("b", 199_000, 1_000_000, 3), NOW).unwrap();
        assert_eq!(m.initiator_offer, "new");
        assert_eq!(m.terms.price_sat_per_xbt, 995_000);

        // A seller takes the highest bid first.
        let s = store_with(&[
            buy("old", 200_000, 990_000, 1),
            buy("new", 200_000, 995_000, 2),
        ]);
        let m = quick(&s, &sell("x", 20_000_000, 990_000, 3), NOW).unwrap();
        assert_eq!(m.participant_offer, "new");
        assert_eq!(m.terms.price_sat_per_xbt, 995_000);
    }

    #[test]
    fn oldest_wins_among_equal_prices() {
        let s = store_with(&[sell("a2", 20_000_000, PRICE, 2), sell("a1", 20_000_000, PRICE, 1)]);
        let m = quick(&s, &buy("b", 200_000, PRICE, 3), NOW).unwrap();
        assert_eq!(m.initiator_offer, "a1");
        assert_eq!(s.offer("a2").unwrap().status, OfferStatus::Open);
    }

    #[test]
    fn remainder_keeps_its_price() {
        // A BTC remainder: 500k at 1_000_000, 200k taken at that price.
        let s = store_with(&[buy("b", 500_000, 1_000_000, 1)]);
        let m = quick(&s, &sell("x", 20_000_000, 990_000, 2), NOW).unwrap();
        assert_eq!(m.terms.btc_sat, 200_000);
        let b = s.offer("b").unwrap();
        assert_eq!((b.status, b.amount_sat, b.price_sat_per_xbt), (OfferStatus::Open, 300_000, 1_000_000));

        // An XBT remainder: 30M at 990_000; 198k BTC buys 20M of it.
        let s = store_with(&[sell("x", 30_000_000, 990_000, 1)]);
        let m = quick(&s, &buy("b", 198_000, 1_000_000, 2), NOW).unwrap();
        assert_eq!(m.terms.xbt_sat, 20_000_000);
        let x = s.offer("x").unwrap();
        assert_eq!((x.status, x.amount_sat, x.price_sat_per_xbt), (OfferStatus::Open, 10_000_000, 990_000));
    }

    #[test]
    fn private_take_trades_at_the_makers_price() {
        let mut maker = sell("x", 20_000_000, 1_020_000, 1);
        maker.private = true;
        maker.share_code = Some("K7Q2".into());
        let s = store_with(&[maker]);
        let taker = buy("b", 204_000, 1_020_000, 2);
        let m = take_private(&s, "K7Q2", &taker, NOW, MAX, "bitcoin", "bc1qfee").unwrap();
        assert_eq!(m.terms.price_sat_per_xbt, 1_020_000);
        assert_eq!((m.terms.xbt_sat, m.terms.btc_sat), (20_000_000, 204_000));
    }

    #[test]
    fn terms_from_a_cross_validate_for_both_limits() {
        let s = store_with(&[sell("a", 12_345_678, 990_001, 1)]);
        let m = quick(&s, &buy("b", 333_333, 1_000_003, 2), NOW).unwrap();
        assert_eq!(m.terms.price_sat_per_xbt, 990_001);
        validate_terms(&m.terms, 990_001, SwapRole::Initiator, NOW as u32 + 5).unwrap();
        validate_terms(&m.terms, 1_000_003, SwapRole::Participant, NOW as u32 + 5).unwrap();
    }

    #[test]
    fn rematch_pairs_crossing_open_offers() {
        // Left open together, as after deadline cancels of two separate matches.
        let s = store_with(&[buy("b", 200_000, 1_000_000, 1), sell("a", 20_000_000, 990_000, 2)]);
        rematch(&s, NOW, MAX, "bitcoin", "bc1qfee");
        let ms = s.all_matches();
        assert_eq!(ms.len(), 1);
        // The older offer was resting first: its price.
        assert_eq!(ms[0].terms.price_sat_per_xbt, 1_000_000);
        assert_eq!((ms[0].initiator_offer.as_str(), ms[0].participant_offer.as_str()), ("a", "b"));
        rematch(&s, NOW, MAX, "bitcoin", "bc1qfee");
        assert_eq!(s.all_matches().len(), 1);
    }

    #[test]
    fn rematch_skips_pairs_that_already_cancelled_together() {
        let s = store_with(&[sell("a", 20_000_000, PRICE, 1)]);
        let b = buy("b", 200_000, PRICE, 2);
        put(&s, &b);
        quick(&s, &b, NOW).unwrap();
        // Unaccepted in time: both offers reopen (a decline cancels the
        // decliner's offer, so this is how a pair comes back together).
        let later = NOW + ACCEPT_SECS as u64 + 1;
        tick(&s, later);
        assert_eq!(s.offer("a").unwrap().status, OfferStatus::Open);
        assert_eq!(s.offer("b").unwrap().status, OfferStatus::Open);
        rematch(&s, later, MAX, "bitcoin", "bc1qfee");
        assert_eq!(s.all_matches().len(), 1);

        // Only that pair is barred: a third offer still matches `a`.
        put(&s, &buy("c", 200_000, PRICE, 3));
        rematch(&s, later, MAX, "bitcoin", "bc1qfee");
        let live: Vec<_> =
            s.all_matches().into_iter().filter(|m| m.status == MatchStatus::Proposed).collect();
        assert_eq!(live.len(), 1);
        assert_eq!((live[0].initiator_offer.as_str(), live[0].participant_offer.as_str()), ("a", "c"));
    }

    /** A book where nothing crosses — every ask above every bid — costs one
     *  pass, not one full scan per offer (which an offer flood made N²). */
    #[test]
    fn rematch_tries_only_offers_that_could_cross() {
        let mut offers = Vec::new();
        for i in 0..40u64 {
            offers.push(sell(&format!("s{i}"), 20_000_000, 2_000_000 + i, i));
            offers.push(buy(&format!("b{i}"), 200_000, 1_000_000 - i, i));
        }
        let s = store_with(&offers);
        REMATCH_TRIED.with(|t| t.set(0));
        rematch(&s, NOW, MAX, "bitcoin", "bc1qfee");
        assert_eq!(REMATCH_TRIED.with(|t| t.get()), 0);
        assert!(s.all_matches().is_empty());

        // One bid that crosses the lowest ask: only offers that could cross are tried.
        put(&s, &buy("hi", 200_000, 2_000_000, 50));
        REMATCH_TRIED.with(|t| t.set(0));
        rematch(&s, NOW, MAX, "bitcoin", "bc1qfee");
        assert_eq!(s.all_matches().len(), 1);
        assert!(REMATCH_TRIED.with(|t| t.get()) <= 2, "tried {}", REMATCH_TRIED.with(|t| t.get()));
    }

    #[test]
    fn rematch_leaves_private_offers_alone() {
        let mut p = buy("b", 200_000, PRICE, 1);
        p.private = true;
        let s = store_with(&[p, sell("a", 20_000_000, PRICE, 2)]);
        rematch(&s, NOW, MAX, "bitcoin", "bc1qfee");
        assert!(s.all_matches().is_empty());
    }

    #[test]
    fn purge_drops_week_old_closed_records() {
        let s = temp_store("purge");
        let old = partial(&s);
        s.push_msg(&old.id, "initiator", serde_json::json!({"hello": 1})).unwrap();
        let mut closed = old.clone();
        closed.status = MatchStatus::Closed;
        closed.closed_at = Some(NOW + 100);
        s.put_match(&closed).unwrap();
        let mut b = s.offer("b").unwrap();
        b.status = OfferStatus::Filled;
        s.put_offer(&b).unwrap();

        let mut recent = offer("r", Direction::XbtToBtc, 20_000_000, NOW + 6 * DAY);
        recent.status = OfferStatus::Cancelled;
        put(&s, &recent);

        let later = NOW + DAY + 7 * DAY + 101;
        s.purge(NOW + 7 * DAY);
        assert!(s.get_match(&old.id).is_some(), "a 7-day-old record is not yet purged");
        s.purge(later);
        assert!(s.get_match(&old.id).is_none());
        assert!(s.offer("x").is_none());
        assert!(s.offer("b").is_none());
        assert!(s.msgs_after(&old.id, 0).is_empty());
        assert!(s.offer("r").is_some());
    }
}
