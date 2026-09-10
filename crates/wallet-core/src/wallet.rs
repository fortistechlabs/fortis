//! Read-only wallet view for one chain: derives addresses from an account xpub and
//! plans unsigned transactions from UTXOs the shell provides. Holds no secret keys.

use bitcoin::bip32::{ChildNumber, Xpub};
use bitcoin::script::PushBytesBuf;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::{
    absolute, transaction, Address, Amount, CompressedPublicKey, OutPoint, Script, ScriptBuf,
    Sequence, Transaction, TxIn, TxOut, Witness,
};

use crate::chain::ChainParams;
use crate::error::{Result, WalletError};
use crate::htlc::HtlcContract;

/// A 0-value `OP_RETURN` output carrying `data`.
///
/// Opt-in BTC-side replay protection: the BLAKE2b fork caps datacarrier data at
/// **82 bytes at consensus** (RDTS), so a Bitcoin transaction that includes an
/// `OP_RETURN` larger than that is consensus-invalid on the fork and cannot be
/// replayed there. Pass ~100 fresh random bytes. (Such an output is non-standard
/// on default Bitcoin relay too — broadcast via a node/service that accepts it.)
pub fn op_return_output(data: &[u8]) -> Result<TxOut> {
    let push = PushBytesBuf::try_from(data.to_vec())
        .map_err(|_| WalletError::Bitcoin("OP_RETURN data too large".into()))?;
    Ok(TxOut { value: Amount::ZERO, script_pubkey: ScriptBuf::new_op_return(push) })
}

/// A service fee for a hosted backend, charged as a percentage of the amount
/// moved and added as an extra output on `plan_payment` / `plan_sweep`. Clamped to
/// `[floor_sat, cap_sat]` (`cap_sat == 0` means uncapped). Self-hosted backends
/// pass `None` and charge nothing.
#[derive(Debug, Clone)]
pub struct ServiceFee {
    pub bps: u32,
    pub floor_sat: u64,
    pub cap_sat: u64,
    pub fee_spk: ScriptBuf,
}

impl ServiceFee {
    /// Basis points above this are ignored — a hosted backend advertising a
    /// larger cut than 10% is misconfigured or hostile, and the client should not
    /// build a transaction around it. The fee is also shown in the confirm sheet
    /// before the user signs.
    const MAX_BPS: u32 = 1_000;

    fn amount_sat(&self, send_amount_sat: u64) -> u64 {
        let bps = u128::from(self.bps.min(Self::MAX_BPS));
        let pct = u64::try_from(u128::from(send_amount_sat).saturating_mul(bps) / 10_000)
            .unwrap_or(u64::MAX);
        let fee = pct.max(self.floor_sat);
        if self.cap_sat > 0 {
            fee.min(self.cap_sat)
        } else {
            fee
        }
    }

    fn output(&self, send_amount_sat: u64) -> Option<TxOut> {
        let sat = self.amount_sat(send_amount_sat);
        if sat == 0 {
            return None;
        }
        // A fee below the dust threshold for the fee address makes the whole
        // transaction non-standard — bump it up to a spendable output.
        let value = Amount::from_sat(sat).max(self.fee_spk.minimal_non_dust());
        Some(TxOut { value, script_pubkey: self.fee_spk.clone() })
    }
}

/// A coin the shell reports to the core (from the platform indexer or an Electrum
/// server). Every wallet UTXO is assumed P2WPKH (BIP-84).
#[derive(Debug, Clone)]
pub struct Utxo {
    pub outpoint: OutPoint,
    pub value: Amount,
    pub script_pubkey: ScriptBuf,
    pub confirmations: u32,
    pub derivation_index: u32,
    pub is_change: bool,
}

/// The result of coin selection: an unsigned transaction plus what was chosen.
#[derive(Debug, Clone)]
pub struct FundingPlan {
    pub tx: Transaction,
    pub selected: Vec<Utxo>,
    pub fee: Amount,
    pub change: Option<Amount>,
    /// The service fee (if any), also present as an output in `tx`.
    pub service_fee: Option<Amount>,
}

// Rough vsize model. P2WPKH spends, segwit tx. Estimates run 1–2 vB high per input
// (low-s signature length varies), which errs toward slightly overpaying fees.
const TX_OVERHEAD_VB: u64 = 11;
const P2WPKH_INPUT_VB: u64 = 68;
/// Below this a change output costs more to spend than it's worth; fold it into fee.
const CHANGE_DUST_SAT: u64 = 294;

/// A feerate above this is refused by `plan_payment` / `plan_sweep`. Even the
/// worst historic mainnet fee spikes stayed near ~1000 sat/vB; a value an order
/// of magnitude past that is a fat-fingered custom feerate or a hostile backend
/// estimate, and building the transaction anyway would burn the wallet on fees.
pub const MAX_FEERATE_SAT_VB: u64 = 10_000;

/// Sum `Amount`s, returning an error instead of panicking if the total would
/// overflow `u64` (a hostile backend can report absurd UTXO / output values).
fn checked_sum(amounts: impl IntoIterator<Item = Amount>) -> Result<Amount> {
    amounts.into_iter().try_fold(Amount::ZERO, |acc, a| {
        acc.checked_add(a)
            .ok_or_else(|| WalletError::InvalidInput("amount total overflows".into()))
    })
}

/// Reject a feerate of 0 (a non-relayable zero-fee transaction) or one above
/// [`MAX_FEERATE_SAT_VB`].
fn check_feerate(feerate_sat_vb: u64) -> Result<()> {
    if feerate_sat_vb == 0 || feerate_sat_vb > MAX_FEERATE_SAT_VB {
        return Err(WalletError::InvalidInput(format!(
            "feerate {feerate_sat_vb} sat/vB is out of range (1..={MAX_FEERATE_SAT_VB})"
        )));
    }
    Ok(())
}

/// Reject a spend output below the dust threshold for its own scriptPubKey — such
/// a transaction is non-standard and will not relay. `OP_RETURN` outputs (value
/// 0 by design) are exempt.
fn check_not_dust(outputs: &[TxOut]) -> Result<()> {
    for o in outputs {
        if o.script_pubkey.is_op_return() {
            continue;
        }
        if o.value < o.script_pubkey.minimal_non_dust() {
            return Err(WalletError::InvalidInput(format!(
                "output of {} is below the dust limit for its address",
                o.value
            )));
        }
    }
    Ok(())
}

fn varint_len(n: usize) -> u64 {
    match n {
        0..=0xFC => 1,
        0xFD..=0xFFFF => 3,
        0x1_0000..=0xFFFF_FFFF => 5,
        _ => 9,
    }
}

fn output_vb(spk: &Script) -> u64 {
    8 + varint_len(spk.len()) + spk.len() as u64
}

pub struct WalletView {
    params: ChainParams,
    xpub: Xpub,
    next_receive: u32,
    next_change: u32,
}

impl WalletView {
    pub fn new(params: ChainParams, xpub: Xpub) -> Self {
        Self { params, xpub, next_receive: 0, next_change: 0 }
    }

    /// Resume address derivation from persisted counters after a shell restart, so
    /// the next receive / change address is not one already handed out.
    pub fn set_next_indices(&mut self, next_receive: u32, next_change: u32) {
        self.next_receive = next_receive;
        self.next_change = next_change;
    }

    /// `(next_receive, next_change)` — read back after planning to persist the
    /// change index that a payment consumed.
    pub fn next_indices(&self) -> (u32, u32) {
        (self.next_receive, self.next_change)
    }

    pub fn balance(&self, utxos: &[Utxo]) -> Amount {
        utxos
            .iter()
            .map(|u| u.value)
            .fold(Amount::ZERO, |a, b| a.checked_add(b).unwrap_or(Amount::MAX_MONEY))
    }

    /// Next unused external (receive) address, BIP-84 `.../0/<i>`.
    pub fn next_receive_address(&mut self) -> Result<Address> {
        let addr = self.address_at(0, self.next_receive)?;
        self.next_receive += 1;
        Ok(addr)
    }

    /// Next unused internal (change) address, BIP-84 `.../1/<i>`.
    pub fn next_change_address(&mut self) -> Result<Address> {
        let addr = self.address_at(1, self.next_change)?;
        self.next_change += 1;
        Ok(addr)
    }

    /// The P2WPKH (BIP-84) address at `.../<branch>/<index>` — `branch` 0 for
    /// receive, 1 for change. Lets a shell re-derive a specific address (e.g. to
    /// cross-check one the node reported) without reimplementing derivation.
    pub fn address_at(&self, branch: u32, index: u32) -> Result<Address> {
        let secp = Secp256k1::verification_only();
        let child = self
            .xpub
            .derive_pub(
                &secp,
                &[
                    ChildNumber::from_normal_idx(branch)?,
                    ChildNumber::from_normal_idx(index)?,
                ],
            )
            .map_err(|e| WalletError::Derivation(e.to_string()))?;
        let pk = CompressedPublicKey(child.public_key);
        Ok(Address::p2wpkh(&pk, self.params.network))
    }

    /// Coin-select from `utxos` (largest-first), add a change output when the leftover
    /// is economically spendable, and return an unsigned transaction.
    ///
    /// TODO(phase-1): branch-and-bound selection, BIP-69 / random output ordering.
    pub fn plan_payment(
        &mut self,
        utxos: &[Utxo],
        mut outputs: Vec<TxOut>,
        feerate_sat_vb: u64,
        min_confirmations: u32,
        service_fee: Option<&ServiceFee>,
        fee_from_amount: bool,
    ) -> Result<FundingPlan> {
        check_feerate(feerate_sat_vb)?;
        check_not_dust(&outputs)?;

        if fee_from_amount {
            return self.plan_payment_fee_inclusive(
                utxos,
                outputs,
                feerate_sat_vb,
                min_confirmations,
                service_fee,
            );
        }

        let service_fee_amt = service_fee.and_then(|sf| {
            let send_amount_sat = checked_sum(outputs.iter().map(|o| o.value)).ok()?.to_sat();
            sf.output(send_amount_sat)
        });
        if let Some(out) = &service_fee_amt {
            outputs.push(out.clone());
        }
        let service_fee_sat = service_fee_amt.map(|o| o.value);

        let target = checked_sum(outputs.iter().map(|o| o.value))?;
        let outputs_vb: u64 = outputs
            .iter()
            .map(|o| output_vb(&o.script_pubkey))
            .fold(0u64, u64::saturating_add);

        let mut eligible: Vec<&Utxo> =
            utxos.iter().filter(|u| u.confirmations >= min_confirmations).collect();
        eligible.sort_by_key(|u| std::cmp::Reverse(u.value)); // largest-first

        let change_spk = self.address_at(1, self.next_change)?.script_pubkey();
        let change_vb = output_vb(&change_spk);

        let mut selected: Vec<Utxo> = Vec::new();
        let mut acc = Amount::ZERO;

        for u in eligible {
            selected.push(u.clone());
            acc = acc.checked_add(u.value).ok_or_else(|| {
                WalletError::InvalidInput("selected input value overflows".into())
            })?;
            let n = selected.len() as u64;
            let base_vb = TX_OVERHEAD_VB
                .saturating_add(n.saturating_mul(P2WPKH_INPUT_VB))
                .saturating_add(outputs_vb);
            let fee_no_change = Amount::from_sat(base_vb.saturating_mul(feerate_sat_vb));
            let fee_with_change =
                Amount::from_sat(base_vb.saturating_add(change_vb).saturating_mul(feerate_sat_vb));

            let need_with_change = checked_sum([target, fee_with_change])?;
            if acc >= need_with_change {
                let change = acc - need_with_change;
                if change.to_sat() >= CHANGE_DUST_SAT {
                    let mut outs = outputs.clone();
                    outs.push(TxOut { value: change, script_pubkey: change_spk });
                    self.next_change += 1;
                    return Ok(assemble(selected, outs, fee_with_change, Some(change), service_fee_sat));
                }
            }
            if acc >= checked_sum([target, fee_no_change])? {
                // No change output — the surplus (< a change output's cost) is fee.
                return Ok(assemble(selected, outputs, acc - target, None, service_fee_sat));
            }
        }

        // Fell short. Report the true minimum — the outputs (destination + any
        // service fee) *plus* the network fee to spend everything eligible — not
        // just the output total, so "need N sat" is the number to top up to.
        let n = selected.len().max(1) as u64;
        let base_vb = TX_OVERHEAD_VB
            .saturating_add(n.saturating_mul(P2WPKH_INPUT_VB))
            .saturating_add(outputs_vb);
        let est_fee = Amount::from_sat(base_vb.saturating_mul(feerate_sat_vb));
        let need = checked_sum([target, est_fee]).unwrap_or(Amount::MAX_MONEY);
        Err(WalletError::InsufficientFunds { need: need.to_sat(), have: acc.to_sat() })
    }

    /// `plan_payment` where the amount the caller asked for is the *total* leaving
    /// the wallet for this payment (excluding change): the network fee and the
    /// service fee are carved out of the destination output, so the recipient
    /// receives `amount − network_fee − service_fee`. Exactly one non-`OP_RETURN`
    /// output is expected.
    fn plan_payment_fee_inclusive(
        &mut self,
        utxos: &[Utxo],
        outputs: Vec<TxOut>,
        feerate_sat_vb: u64,
        min_confirmations: u32,
        service_fee: Option<&ServiceFee>,
    ) -> Result<FundingPlan> {
        let dest_idx = {
            let spendable: Vec<usize> = outputs
                .iter()
                .enumerate()
                .filter(|(_, o)| !o.script_pubkey.is_op_return())
                .map(|(i, _)| i)
                .collect();
            match spendable.as_slice() {
                [i] => *i,
                _ => {
                    return Err(WalletError::InvalidInput(
                        "fee-from-amount needs exactly one destination".into(),
                    ))
                }
            }
        };
        let dest_spk = outputs[dest_idx].script_pubkey.clone();
        let dest_dust = dest_spk.minimal_non_dust();
        // The user's number: everything this payment costs the wallet bar change.
        let budget = outputs[dest_idx].value;

        let sf_out = service_fee.and_then(|sf| sf.output(budget.to_sat()));
        let sf_value = sf_out.as_ref().map_or(Amount::ZERO, |o| o.value);
        let sf_vb = sf_out.as_ref().map_or(0, |o| output_vb(&o.script_pubkey));
        // vsize of every output except a possible change output.
        let non_change_vb: u64 = outputs
            .iter()
            .map(|o| output_vb(&o.script_pubkey))
            .fold(0u64, u64::saturating_add)
            .saturating_add(sf_vb);

        let mut eligible: Vec<&Utxo> =
            utxos.iter().filter(|u| u.confirmations >= min_confirmations).collect();
        eligible.sort_by_key(|u| std::cmp::Reverse(u.value));

        let change_spk = self.address_at(1, self.next_change)?.script_pubkey();
        let change_vb = output_vb(&change_spk);

        let too_small = |fees: Amount| {
            WalletError::InvalidInput(format!(
                "amount too small: {} sat of fees leaves the recipient below the dust limit",
                fees.to_sat()
            ))
        };

        let mut selected: Vec<Utxo> = Vec::new();
        let mut acc = Amount::ZERO;
        for u in eligible {
            selected.push(u.clone());
            acc = acc.checked_add(u.value).ok_or_else(|| {
                WalletError::InvalidInput("selected input value overflows".into())
            })?;
            if acc < budget {
                continue;
            }
            let n = selected.len() as u64;
            let base_vb = TX_OVERHEAD_VB
                .saturating_add(n.saturating_mul(P2WPKH_INPUT_VB))
                .saturating_add(non_change_vb);
            let surplus = acc - budget; // safe: acc >= budget

            let mut outs = outputs.clone();
            if surplus.to_sat() >= CHANGE_DUST_SAT {
                // Keep `surplus` as change; fees come out of `budget`.
                let net_fee = Amount::from_sat(
                    base_vb.saturating_add(change_vb).saturating_mul(feerate_sat_vb),
                );
                let recipient = budget
                    .checked_sub(sf_value)
                    .and_then(|v| v.checked_sub(net_fee))
                    .filter(|r| *r >= dest_dust)
                    .ok_or_else(|| too_small(sf_value + net_fee))?;
                outs[dest_idx].value = recipient;
                if let Some(o) = &sf_out {
                    outs.push(o.clone());
                }
                outs.push(TxOut { value: surplus, script_pubkey: change_spk });
                self.next_change += 1;
                return Ok(assemble(
                    selected,
                    outs,
                    net_fee,
                    Some(surplus),
                    sf_out.as_ref().map(|o| o.value),
                ));
            }
            // No change: the sub-dust surplus goes to the miner fee; the recipient
            // still gets exactly `budget − fees`.
            let net_fee = Amount::from_sat(base_vb.saturating_mul(feerate_sat_vb));
            let recipient = budget
                .checked_sub(sf_value)
                .and_then(|v| v.checked_sub(net_fee))
                .filter(|r| *r >= dest_dust)
                .ok_or_else(|| too_small(sf_value + net_fee))?;
            outs[dest_idx].value = recipient;
            if let Some(o) = &sf_out {
                outs.push(o.clone());
            }
            let real_fee = acc - recipient - sf_value; // net_fee + surplus
            return Ok(assemble(selected, outs, real_fee, None, sf_out.as_ref().map(|o| o.value)));
        }

        Err(WalletError::InsufficientFunds { need: budget.to_sat(), have: acc.to_sat() })
    }

    /// Send every input confirmed at least `min_confirmations` deep to a single
    /// destination, with the fee taken from the total (no change output). For
    /// "empty this wallet" / sweeps.
    pub fn plan_sweep(
        &self,
        utxos: &[Utxo],
        dest: ScriptBuf,
        feerate_sat_vb: u64,
        min_confirmations: u32,
        service_fee: Option<&ServiceFee>,
    ) -> Result<FundingPlan> {
        check_feerate(feerate_sat_vb)?;
        let selected: Vec<Utxo> = utxos
            .iter()
            .filter(|u| u.confirmations >= min_confirmations)
            .cloned()
            .collect();
        if selected.is_empty() {
            return Err(WalletError::InsufficientFunds { need: 1, have: 0 });
        }
        let total = checked_sum(selected.iter().map(|u| u.value))?;
        let fee_out = service_fee.and_then(|sf| sf.output(total.to_sat()));
        let fee_out_value = fee_out.as_ref().map_or(Amount::ZERO, |o| o.value);
        let fee_out_vb = fee_out.as_ref().map_or(0, |o| output_vb(&o.script_pubkey));

        let vb = TX_OVERHEAD_VB
            .saturating_add((selected.len() as u64).saturating_mul(P2WPKH_INPUT_VB))
            .saturating_add(output_vb(&dest))
            .saturating_add(fee_out_vb);
        let fee = Amount::from_sat(vb.saturating_mul(feerate_sat_vb));
        let value = total
            .checked_sub(fee)
            .and_then(|v| v.checked_sub(fee_out_value))
            .filter(|v| v.to_sat() >= CHANGE_DUST_SAT)
            .ok_or(WalletError::InsufficientFunds {
                need: (fee + fee_out_value).to_sat() + CHANGE_DUST_SAT,
                have: total.to_sat(),
            })?;
        let service_fee_sat = fee_out.as_ref().map(|o| o.value);
        let mut outs = vec![TxOut { value, script_pubkey: dest }];
        if let Some(o) = fee_out {
            outs.push(o);
        }
        Ok(assemble(selected, outs, fee, None, service_fee_sat))
    }

    /// Plan a transaction that funds `contract`'s HTLC output.
    pub fn plan_htlc_funding(
        &mut self,
        utxos: &[Utxo],
        contract: &HtlcContract,
        feerate_sat_vb: u64,
        min_confirmations: u32,
    ) -> Result<FundingPlan> {
        self.plan_payment(utxos, vec![contract.funding_output()], feerate_sat_vb, min_confirmations, None, false)
    }
}

fn assemble(
    selected: Vec<Utxo>,
    outputs: Vec<TxOut>,
    fee: Amount,
    change: Option<Amount>,
    service_fee: Option<Amount>,
) -> FundingPlan {
    let input = selected
        .iter()
        .map(|u| TxIn {
            previous_output: u.outpoint,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        })
        .collect();
    let tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input,
        output: outputs,
    };
    FundingPlan { tx, selected, fee, change, service_fee }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::MasterKey;
    use bitcoin::hashes::Hash;
    use bitcoin::Txid;

    fn view() -> WalletView {
        let (_m, key) = MasterKey::generate(&[3u8; 32]).unwrap();
        let params = ChainParams::bitcoin();
        let xpub = key.account_xpub(&params, 0).unwrap();
        WalletView::new(params, xpub)
    }

    fn utxo(sats: u64, confs: u32, tag: u8) -> Utxo {
        Utxo {
            outpoint: OutPoint::new(Txid::from_byte_array([tag; 32]), 0),
            value: Amount::from_sat(sats),
            script_pubkey: ScriptBuf::new(),
            confirmations: confs,
            derivation_index: tag as u32,
            is_change: false,
        }
    }

    fn htlc_out(sats: u64) -> TxOut {
        // 34-byte P2WSH-shaped placeholder scriptPubKey.
        TxOut {
            value: Amount::from_sat(sats),
            script_pubkey: ScriptBuf::from(vec![0u8; 34]),
        }
    }

    #[test]
    fn derives_distinct_bech32_addresses() {
        let mut v = view();
        let a0 = v.next_receive_address().unwrap();
        let a1 = v.next_receive_address().unwrap();
        let c0 = v.next_change_address().unwrap();
        assert_ne!(a0, a1);
        assert_ne!(a0, c0);
        assert!(a0.to_string().starts_with("bc1q"));
    }

    #[test]
    fn funds_with_change() {
        let mut v = view();
        let utxos = [utxo(1_000_000, 3, 1)];
        let plan = v.plan_payment(&utxos, vec![htlc_out(200_000)], 10, 1, None, false).unwrap();
        assert_eq!(plan.selected.len(), 1);
        assert_eq!(plan.tx.output.len(), 2); // htlc + change
        let change = plan.change.unwrap();
        assert_eq!(
            (Amount::from_sat(200_000) + plan.fee + change).to_sat(),
            1_000_000
        );
        // ~ (11 + 68 + 43 + 31) vB * 10 = 1530 sat
        assert!((1400..1700).contains(&plan.fee.to_sat()), "fee was {}", plan.fee);
    }

    #[test]
    fn no_change_when_surplus_is_small() {
        let mut v = view();
        // base vsize = 11 + 68 + 43 = 122; no-change fee @10 sat/vB = 1220.
        // A change output would cost ~310 more, so a ~100 sat leftover folds into fee.
        let utxos = [utxo(200_000 + 1_320, 3, 1)];
        let plan = v.plan_payment(&utxos, vec![htlc_out(200_000)], 10, 1, None, false).unwrap();
        assert_eq!(plan.tx.output.len(), 1);
        assert!(plan.change.is_none());
        assert_eq!(plan.fee.to_sat(), 1_320);
    }

    #[test]
    fn accumulates_multiple_inputs() {
        let mut v = view();
        let utxos = [utxo(100_000, 3, 1), utxo(90_000, 3, 2), utxo(80_000, 3, 3)];
        let plan = v.plan_payment(&utxos, vec![htlc_out(200_000)], 5, 1, None, false).unwrap();
        assert_eq!(plan.selected.len(), 3);
    }

    #[test]
    fn rejects_insufficient_funds() {
        let mut v = view();
        let utxos = [utxo(50_000, 3, 1)];
        let err = v.plan_payment(&utxos, vec![htlc_out(200_000)], 5, 1, None, false).unwrap_err();
        // `need` covers the outputs *and* the network fee, not just the output total.
        match err {
            WalletError::InsufficientFunds { need, have } => {
                assert!(need > 200_000, "need {need} should include the fee");
                assert_eq!(have, 50_000);
            }
            other => panic!("expected InsufficientFunds, got {other:?}"),
        }
    }

    #[test]
    fn op_return_plus_fee_from_amount() {
        let mut v = view();
        let utxos = [utxo(1_000_000, 3, 1)];
        let sf = ServiceFee { bps: 100, floor_sat: 400, cap_sat: 0, fee_spk: fee_addr_spk() };
        let mut outs = vec![htlc_out(200_000)];
        outs.push(super::op_return_output(&[7u8; 100]).unwrap()); // replay-protection blob
        let plan = v.plan_payment(&utxos, outs, 10, 1, Some(&sf), true).unwrap();
        // OP_RETURN survives, recipient shrank by all the fees (incl. its ~112 vB)
        assert_eq!(plan.tx.output.iter().filter(|o| o.script_pubkey.is_op_return()).count(), 1);
        let inputs = 1_000_000i64;
        let change = plan.change.map_or(0, |c| c.to_sat() as i64);
        let svc = plan.service_fee.map_or(0, |c| c.to_sat() as i64);
        let recipient = plan
            .tx
            .output
            .iter()
            .find(|o| o.script_pubkey == ScriptBuf::from(vec![0u8; 34]))
            .unwrap()
            .value
            .to_sat() as i64;
        assert_eq!(inputs - change, 200_000); // wallet is out exactly the amount asked
        assert_eq!(recipient + svc + plan.fee.to_sat() as i64, 200_000);
        assert!(plan.fee.to_sat() >= 100 * 10); // paid for the oversized OP_RETURN
    }

    #[test]
    fn fee_from_amount_carves_fees_out_of_the_destination() {
        let mut v = view();
        let utxos = [utxo(1_000_000, 3, 1)];
        let sf = ServiceFee { bps: 100, floor_sat: 400, cap_sat: 0, fee_spk: fee_addr_spk() };
        // "Send 200_000, fees included" — recipient gets 200_000 − netfee − 2_000 (1%).
        let plan = v
            .plan_payment(&utxos, vec![htlc_out(200_000)], 10, 1, Some(&sf), true)
            .unwrap();
        let inputs = 1_000_000i64;
        let change = plan.change.map_or(0, |c| c.to_sat() as i64);
        let svc = plan.service_fee.map_or(0, |c| c.to_sat() as i64);
        let recipient = plan.tx.output.iter().find(|o| o.script_pubkey != fee_addr_spk() && o.value.to_sat() != change as u64).unwrap().value.to_sat() as i64;
        // the wallet is out exactly 200_000 for this payment (the rest is change)
        assert_eq!(inputs - change, 200_000);
        // recipient + service fee + network fee == 200_000
        assert_eq!(recipient + svc + plan.fee.to_sat() as i64, 200_000);
        assert_eq!(svc, 2_000); // 1% of 200_000, above the 400 floor
        assert!(recipient < 200_000);
    }

    #[test]
    fn fee_from_amount_rejects_when_fees_eat_the_whole_amount() {
        let mut v = view();
        let utxos = [utxo(1_000_000, 3, 1)];
        let sf = ServiceFee { bps: 100, floor_sat: 400, cap_sat: 0, fee_spk: fee_addr_spk() };
        // Sending 900 fee-inclusive: 400 service + ~1500 network > 900 → nothing left.
        let err = v
            .plan_payment(&utxos, vec![htlc_out(900)], 10, 1, Some(&sf), true)
            .unwrap_err();
        assert!(matches!(err, WalletError::InvalidInput(_)), "{err:?}");
    }

    #[test]
    fn insufficient_funds_need_includes_the_service_fee() {
        let mut v = view();
        let utxos = [utxo(1_200, 3, 1)];
        let sf = ServiceFee { bps: 100, floor_sat: 546, cap_sat: 0, fee_spk: fee_addr_spk() };
        // 1000 to send + the 546-sat service-fee floor = 1546 in outputs alone,
        // so `need` must be at least that (plus the network fee).
        let err = v
            .plan_payment(&utxos, vec![htlc_out(1_000)], 2, 1, Some(&sf), false)
            .unwrap_err();
        match err {
            WalletError::InsufficientFunds { need, .. } => assert!(need >= 1_546, "need {need}"),
            other => panic!("expected InsufficientFunds, got {other:?}"),
        }
    }

    #[test]
    fn excludes_unconfirmed_below_threshold() {
        let mut v = view();
        let utxos = [utxo(1_000_000, 0, 1)];
        assert!(v.plan_payment(&utxos, vec![htlc_out(200_000)], 5, 1, None, false).is_err());
    }

    #[test]
    fn sweep_spends_everything_minus_fee() {
        let v = view();
        let utxos = [utxo(400_000, 3, 1), utxo(600_000, 3, 2), utxo(9_999, 0, 3)];
        let dest = ScriptBuf::from(vec![0u8; 22]); // P2WPKH-shaped
        let plan = v.plan_sweep(&utxos, dest, 10, 1, None).unwrap();
        assert_eq!(plan.selected.len(), 2); // the 0-conf utxo is excluded
        assert_eq!(plan.tx.output.len(), 1);
        assert!(plan.change.is_none());
        assert_eq!(plan.tx.output[0].value + plan.fee, Amount::from_sat(1_000_000));
        // vsize = 11 + 2*68 + 31 = 178; fee @10 = 1780
        assert_eq!(plan.fee.to_sat(), 1_780);
    }

    #[test]
    fn sweep_rejects_when_fee_exceeds_funds() {
        let v = view();
        let utxos = [utxo(500, 3, 1)];
        assert!(v.plan_sweep(&utxos, ScriptBuf::from(vec![0u8; 22]), 10, 1, None).is_err());
    }

    #[test]
    fn op_return_output_is_a_zero_value_data_push() {
        let out = super::op_return_output(&[7u8; 100]).unwrap();
        assert_eq!(out.value, Amount::ZERO);
        assert!(out.script_pubkey.is_op_return());
        // OP_RETURN + OP_PUSHDATA1 + len byte + 100 data
        assert_eq!(out.script_pubkey.len(), 103);
    }

    #[test]
    fn payment_with_op_return_pays_for_the_extra_bytes() {
        let mut v = view();
        let utxos = [utxo(1_000_000, 3, 1)];
        let mut outs = vec![htlc_out(200_000)];
        outs.push(super::op_return_output(&[9u8; 100]).unwrap());
        let plan = v.plan_payment(&utxos, outs, 10, 1, None, false).unwrap();
        assert_eq!(plan.tx.output.iter().filter(|o| o.script_pubkey.is_op_return()).count(), 1);
        // fee covers the ~112 vB OP_RETURN output on top of the base tx
        assert!(plan.fee.to_sat() >= (11 + 68 + 43 + 31 + 112) * 10 - 20);
    }

    #[test]
    fn resumes_change_index_from_persisted_counter() {
        let mut v = view();
        v.set_next_indices(7, 4);
        assert_eq!(v.next_indices(), (7, 4));
        let utxos = [utxo(1_000_000, 3, 1)];
        let plan = v.plan_payment(&utxos, vec![htlc_out(200_000)], 10, 1, None, false).unwrap();
        assert!(plan.change.is_some());
        assert_eq!(v.next_indices().1, 5); // change index advanced 4 -> 5
    }

    fn fee_addr_spk() -> ScriptBuf {
        ScriptBuf::from(vec![0u8; 22]) // P2WPKH-shaped
    }

    #[test]
    fn service_fee_is_a_percentage_with_floor_and_cap() {
        let sf = ServiceFee { bps: 25, floor_sat: 200, cap_sat: 5_000, fee_spk: fee_addr_spk() };
        assert_eq!(sf.amount_sat(10_000), 200); // 0.25% of 10k = 25, below floor
        assert_eq!(sf.amount_sat(1_000_000), 2_500); // 0.25% of 1,000,000 = 2,500
        assert_eq!(sf.amount_sat(10_000_000), 5_000); // 0.25% of 10M = 25,000, capped
    }

    #[test]
    fn service_fee_uncapped_when_cap_is_zero() {
        let sf = ServiceFee { bps: 25, floor_sat: 0, cap_sat: 0, fee_spk: fee_addr_spk() };
        assert_eq!(sf.amount_sat(10_000_000), 25_000);
    }

    #[test]
    fn service_fee_output_is_never_dust() {
        // 1% of a small send (and even a low floor) would be a dust output that
        // makes the whole tx non-standard — the output must be bumped up.
        let sf = ServiceFee { bps: 100, floor_sat: 100, cap_sat: 0, fee_spk: fee_addr_spk() };
        let out = sf.output(5_000).unwrap();
        assert!(out.value >= fee_addr_spk().minimal_non_dust());
        assert!(out.value.to_sat() > 100);
    }

    #[test]
    fn payment_adds_a_service_fee_output_paid_from_the_inputs() {
        let mut v = view();
        let utxos = [utxo(1_000_000, 3, 1)];
        let sf = ServiceFee { bps: 100, floor_sat: 200, cap_sat: 0, fee_spk: fee_addr_spk() };
        let plan = v.plan_payment(&utxos, vec![htlc_out(200_000)], 10, 1, Some(&sf), false).unwrap();
        // 1% of 200,000 = 2,000
        assert_eq!(plan.service_fee, Some(Amount::from_sat(2_000)));
        assert_eq!(
            plan.tx.output.iter().filter(|o| o.script_pubkey == fee_addr_spk()).count(),
            1
        );
        let change = plan.change.unwrap();
        assert_eq!(
            (Amount::from_sat(200_000) + Amount::from_sat(2_000) + plan.fee + change).to_sat(),
            1_000_000
        );
    }

    #[test]
    fn rejects_a_zero_or_absurd_feerate() {
        let mut v = view();
        let utxos = [utxo(100_000_000, 3, 1)];
        let zero = v.plan_payment(&utxos, vec![htlc_out(200_000)], 0, 1, None, false);
        assert!(matches!(zero, Err(WalletError::InvalidInput(_))));
        let huge = v.plan_payment(&utxos, vec![htlc_out(200_000)], MAX_FEERATE_SAT_VB + 1, 1, None, false);
        assert!(matches!(huge, Err(WalletError::InvalidInput(_))));
        // the ceiling itself is allowed (funds permitting)
        assert!(v.plan_payment(&utxos, vec![htlc_out(200_000)], MAX_FEERATE_SAT_VB, 1, None, false).is_ok());

        let sv = view();
        let dest = ScriptBuf::from(vec![0u8; 22]);
        assert!(matches!(
            sv.plan_sweep(&utxos, dest.clone(), 0, 1, None),
            Err(WalletError::InvalidInput(_))
        ));
        assert!(matches!(
            sv.plan_sweep(&utxos, dest, 999_999, 1, None),
            Err(WalletError::InvalidInput(_))
        ));
    }

    #[test]
    fn rejects_a_dust_destination_output() {
        let mut v = view();
        let utxos = [utxo(1_000_000, 3, 1)];
        // 100 sat to a P2WPKH-shaped script is below the dust limit.
        let dust = TxOut { value: Amount::from_sat(100), script_pubkey: ScriptBuf::from(vec![0u8; 34]) };
        assert!(v.plan_payment(&utxos, vec![dust], 10, 1, None, false).is_err());
    }

    #[test]
    fn overflowing_amounts_error_instead_of_panicking() {
        let mut v = view();
        let utxos = [utxo(u64::MAX, 3, 1)];
        // A payment whose target + fee overflows u64 must be rejected, not panic —
        // a hostile backend can report absurd UTXO / output values.
        let r = v.plan_payment(&utxos, vec![htlc_out(u64::MAX)], 10, 1, None, false);
        assert!(matches!(r, Err(WalletError::InvalidInput(_))));

        // And an overflowing input accumulation, likewise.
        let mut v2 = view();
        let many = [utxo(u64::MAX, 3, 1), utxo(u64::MAX, 3, 2)];
        let r2 = v2.plan_payment(&many, vec![htlc_out(u64::MAX)], 10, 1, None, false);
        assert!(matches!(r2, Err(WalletError::InvalidInput(_))));
    }

    #[test]
    fn service_fee_bps_is_clamped_to_a_sane_maximum() {
        // A backend advertising a 500% fee is capped at 10% of the amount.
        let sf = ServiceFee { bps: 50_000, floor_sat: 0, cap_sat: 0, fee_spk: fee_addr_spk() };
        assert_eq!(sf.amount_sat(1_000_000), 100_000);
    }

    #[test]
    fn sweep_carves_the_service_fee_out_of_the_swept_total() {
        let v = view();
        let utxos = [utxo(1_000_000, 3, 1)];
        let dest = ScriptBuf::from(vec![1u8; 22]);
        let sf = ServiceFee { bps: 100, floor_sat: 200, cap_sat: 0, fee_spk: fee_addr_spk() };
        let plan = v.plan_sweep(&utxos, dest, 10, 1, Some(&sf)).unwrap();
        // 1% of the swept 1,000,000 = 10,000
        assert_eq!(plan.service_fee, Some(Amount::from_sat(10_000)));
        assert_eq!(plan.tx.output.len(), 2); // destination + fee
        let dest_out = plan.tx.output.iter().find(|o| o.script_pubkey != fee_addr_spk()).unwrap();
        assert_eq!(
            (dest_out.value + Amount::from_sat(10_000) + plan.fee).to_sat(),
            1_000_000
        );
    }
}
