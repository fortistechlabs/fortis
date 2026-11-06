//! On-chain assertions, against Knots: a transaction is confirmed and its
//! witness signatures carry the expected hash type bytes.

use serde_json::Value;

use crate::env::{btc_json, R};

/// A transaction as Knots reports it.
pub struct TxInfo {
	pub txid: String,
	pub confirmations: u64,
	pub fee_sat: u64,
	/// Per input: the last byte (hash type) of each DER signature in its witness.
	pub sig_bytes: Vec<Vec<u8>>,
	/// Per input: the outpoint it spends.
	pub prevouts: Vec<(String, u32)>,
	/// Per output: value in sat and address.
	pub outputs: Vec<(u64, String)>,
}

/// The hash type bytes of the DER signatures among a witness's elements.
pub fn der_sig_bytes(witness: &[Vec<u8>]) -> Vec<u8> {
	witness.iter().filter(|w| is_der_sig(w)).map(|w| w[w.len() - 1]).collect()
}

/// `30 len 02 rlen r 02 slen s hashtype`, lengths consistent (so a preimage
/// or script that happens to start with 0x30 is not taken for one).
fn is_der_sig(w: &[u8]) -> bool {
	if w.len() < 9 || w[0] != 0x30 || w[1] as usize + 3 != w.len() || w[2] != 0x02 {
		return false;
	}
	let rlen = w[3] as usize;
	let s_at = 4 + rlen;
	s_at + 2 < w.len() && w[s_at] == 0x02 && s_at + 2 + w[s_at + 1] as usize + 1 == w.len()
}

fn unhex(h: &str) -> R<Vec<u8>> {
	if h.len() % 2 != 0 {
		return Err(format!("odd hex {h}"));
	}
	(0..h.len())
		.step_by(2)
		.map(|i| u8::from_str_radix(&h[i..i + 2], 16).map_err(|e| format!("hex {h}: {e}")))
		.collect()
}

fn sats(v: &Value) -> u64 {
	// Knots reports BTC with 8 decimals; round to the sat.
	(v.as_f64().unwrap_or(0.0) * 1e8).round() as u64
}

/// Fetches `txid` from Knots (with prevouts, for the fee).
pub fn tx(txid: &str) -> R<TxInfo> {
	let v = btc_json(&["getrawtransaction", txid, "2"])?;
	let mut sig_bytes = vec![];
	let mut prevouts = vec![];
	for vin in v["vin"].as_array().ok_or_else(|| format!("{txid}: no vin"))? {
		let witness: Vec<Vec<u8>> = vin["txinwitness"]
			.as_array()
			.into_iter()
			.flatten()
			.map(|w| unhex(w.as_str().unwrap_or("")))
			.collect::<R<_>>()?;
		sig_bytes.push(der_sig_bytes(&witness));
		prevouts.push((
			vin["txid"].as_str().unwrap_or("").to_string(),
			vin["vout"].as_u64().unwrap_or(0) as u32,
		));
	}
	let outputs = v["vout"]
		.as_array()
		.into_iter()
		.flatten()
		.map(|o| (sats(&o["value"]), o["scriptPubKey"]["address"].as_str().unwrap_or("").to_string()))
		.collect();
	Ok(TxInfo {
		txid: txid.to_string(),
		confirmations: v["confirmations"].as_u64().unwrap_or(0),
		fee_sat: sats(&v["fee"]),
		sig_bytes,
		prevouts,
		outputs,
	})
}

pub fn fmt_sigs(sig_bytes: &[Vec<u8>]) -> String {
	let inputs: Vec<String> = sig_bytes
		.iter()
		.map(|s| format!("[{}]", s.iter().map(|b| format!("0x{b:02x}")).collect::<Vec<_>>().join(", ")))
		.collect();
	format!("[{}]", inputs.join(", "))
}

/// Asserts `txid` is confirmed by Knots and that input `i`'s DER signatures
/// end in exactly `expected[i]` (`&[]`: plain, not checked).
pub fn confirmed_with(txid: &str, expected: &[&[u8]]) -> R<TxInfo> {
	let info = tx(txid)?;
	println!("    {txid}: {} conf, fee {} sat, sigs {}", info.confirmations, info.fee_sat, fmt_sigs(&info.sig_bytes));
	if info.confirmations < 1 {
		return Err(format!("{txid} is not confirmed"));
	}
	if expected.len() != info.sig_bytes.len() {
		return Err(format!(
			"{txid}: {} inputs, expected {} ({})",
			info.sig_bytes.len(),
			expected.len(),
			fmt_sigs(&info.sig_bytes)
		));
	}
	for (i, (got, want)) in info.sig_bytes.iter().zip(expected).enumerate() {
		if !want.is_empty() && got.as_slice() != *want {
			return Err(format!(
				"{txid} input {i}: signature hash types {} , expected {}",
				fmt_sigs(&[got.clone()]),
				fmt_sigs(&[want.to_vec()])
			));
		}
	}
	Ok(info)
}

/// `confirmed_with` where every input gets `per_input` (any input count).
pub fn confirmed_all(txid: &str, per_input: &[u8]) -> R<TxInfo> {
	let n = tx(txid)?.prevouts.len();
	confirmed_with(txid, &vec![per_input; n])
}

/// The index of the output of `txid` worth `value` sat (exactly one).
pub fn output_with_value(info: &TxInfo, value: u64) -> R<u32> {
	let found: Vec<usize> =
		info.outputs.iter().enumerate().filter(|(_, (v, _))| *v == value).map(|(i, _)| i).collect();
	match found.as_slice() {
		[i] => Ok(*i as u32),
		_ => Err(format!("{}: {} outputs worth {value} sat: {:?}", info.txid, found.len(), info.outputs)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn finds_der_signature_bytes() {
		// A 71-byte DER signature ending in 0x21, an empty element, a pubkey, a script.
		let mut sig = vec![0x30, 0x44, 0x02, 0x20];
		sig.extend([1u8; 32]);
		sig.extend([0x02, 0x20]);
		sig.extend([2u8; 32]);
		sig.push(0x21);
		let mut pubkey = vec![0x02];
		pubkey.extend([7u8; 32]);
		let witness = vec![vec![], sig.clone(), pubkey, vec![0x52, 0x21, 0x30]];
		assert_eq!(der_sig_bytes(&witness), vec![0x21]);
		let mut plain = sig;
		*plain.last_mut().unwrap() = 0x01;
		assert_eq!(der_sig_bytes(&[plain]), vec![0x01]);
		// A 32-byte preimage that starts like a DER sequence is not a signature.
		let mut preimage = vec![0x30, 0x1d];
		preimage.extend([9u8; 30]);
		assert!(der_sig_bytes(&[preimage]).is_empty());
	}
}
