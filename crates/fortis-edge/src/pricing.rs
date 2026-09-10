//! Optional service fee. When configured, the edge advertises it at
//! `GET /pricing` and rejects a `POST /<chain>/tx` whose transaction doesn't pay
//! at least `floor_sat` to the fee address — the enforcement teeth, matching the
//! check `fortisd` does on the self-hosted path.

use anyhow::{anyhow, Context, Result};
use bitcoin::address::NetworkUnchecked;
use bitcoin::{Address, Network, ScriptBuf, Transaction};
use serde_json::json;

pub struct Pricing {
    address: String,
    spk: ScriptBuf,
    bps: u32,
    floor_sat: u64,
    cap_sat: u64,
}

impl Pricing {
    pub fn new(address: String, network: Network, bps: u32, floor_sat: u64, cap_sat: u64) -> Result<Self> {
        let spk = address
            .parse::<Address<NetworkUnchecked>>()
            .context("service fee address")?
            .require_network(network)
            .map_err(|_| anyhow!("service fee address is not valid on {network}"))?
            .script_pubkey();
        Ok(Self { address, spk, bps, floor_sat, cap_sat })
    }

    pub fn as_json(&self) -> serde_json::Value {
        json!({
            "address": self.address,
            "bps": self.bps,
            "floor_sat": self.floor_sat,
            "cap_sat": self.cap_sat,
        })
    }

    /// Total paid to the fee address, in sat. Saturating — a hand-crafted tx hex
    /// can carry absurd output values, and this must not panic.
    fn paid_by(&self, tx: &Transaction) -> u64 {
        tx.output
            .iter()
            .filter(|o| o.script_pubkey == self.spk)
            .map(|o| o.value.to_sat())
            .fold(0u64, u64::saturating_add)
    }

    /// `Err(msg)` when `raw_tx_hex` parses but pays less than `floor_sat` to the
    /// fee address. An unparseable body passes here — the upstream rejects it.
    pub fn check_tx_hex(&self, raw_tx_hex: &str) -> std::result::Result<(), String> {
        let Ok(bytes) = hex::decode(raw_tx_hex.trim()) else { return Ok(()) };
        let Ok(tx) = bitcoin::consensus::deserialize::<Transaction>(&bytes) else { return Ok(()) };
        if self.paid_by(&tx) < self.floor_sat {
            return Err(format!(
                "transaction must pay the service fee: at least {} sat to {}",
                self.floor_sat, self.address
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{transaction::Version, Amount, TxOut};

    // a real mainnet p2wpkh address
    const FEE_ADDR: &str = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";

    fn pricing() -> Pricing {
        Pricing::new(FEE_ADDR.into(), Network::Bitcoin, 25, 200, 5_000).unwrap()
    }

    fn tx_paying(fee_sat: u64) -> String {
        let fee_spk = FEE_ADDR
            .parse::<Address<NetworkUnchecked>>()
            .unwrap()
            .assume_checked()
            .script_pubkey();
        let tx = Transaction {
            version: Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![
                TxOut { value: Amount::from_sat(100_000), script_pubkey: ScriptBuf::new() },
                TxOut { value: Amount::from_sat(fee_sat), script_pubkey: fee_spk },
            ],
        };
        hex::encode(bitcoin::consensus::serialize(&tx))
    }

    #[test]
    fn rejects_a_tx_that_underpays_the_fee() {
        assert!(pricing().check_tx_hex(&tx_paying(150)).is_err());
        assert!(pricing().check_tx_hex(&tx_paying(0)).is_err());
    }

    #[test]
    fn accepts_a_tx_that_pays_at_least_the_floor() {
        assert!(pricing().check_tx_hex(&tx_paying(200)).is_ok());
        assert!(pricing().check_tx_hex(&tx_paying(4_000)).is_ok());
    }

    #[test]
    fn absurd_output_values_do_not_panic() {
        let fee_spk = FEE_ADDR
            .parse::<Address<NetworkUnchecked>>()
            .unwrap()
            .assume_checked()
            .script_pubkey();
        let tx = Transaction {
            version: Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![
                TxOut { value: Amount::from_sat(u64::MAX), script_pubkey: fee_spk.clone() },
                TxOut { value: Amount::from_sat(u64::MAX), script_pubkey: fee_spk },
            ],
        };
        let hex_tx = hex::encode(bitcoin::consensus::serialize(&tx));
        // saturates rather than overflowing; the huge "payment" clears the floor
        assert!(pricing().check_tx_hex(&hex_tx).is_ok());
    }

    #[test]
    fn unparseable_bodies_pass_through_to_the_upstream() {
        assert!(pricing().check_tx_hex("not hex").is_ok());
        assert!(pricing().check_tx_hex("deadbeef").is_ok());
    }

    #[test]
    fn bad_address_is_rejected_at_construction() {
        assert!(Pricing::new("not-an-address".into(), Network::Bitcoin, 25, 200, 5_000).is_err());
        // testnet address on a mainnet edge
        assert!(Pricing::new(
            "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx".into(),
            Network::Bitcoin,
            25,
            200,
            5_000,
        )
        .is_err());
    }
}
