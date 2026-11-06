//! FORTIS XBT: Lightning on the Bitcoin BLAKE2b chain (XBT).
//!
//! A channel whose `channel_type` carries `option_unified_sigs` (bit 514)
//! signs every bilateral signature (commitment, closing, HTLC) with the
//! `SIGHASH_UNIFIED` (0x20) bit set: `0x21` for SIGHASH_ALL, `0xa3` for the
//! anchor HTLC signature sent to the peer. Spends only this node signs keep
//! plain hash types (BOLT 3 allows it).
//!
//! Nothing here reads global state: every function takes an explicit
//! `unified` flag, which callers derive from the channel's type with
//! [`is_unified`].

use bitcoin::consensus::encode::serialize;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::ecdsa::Signature;
use bitcoin::secp256k1::Message;
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::{Amount, Script, Transaction, TxOut, Witness};

use crate::prelude::*;
use crate::types::features::ChannelTypeFeatures;

/// The hash type bit that opts a signature in.
pub const SIGHASH_UNIFIED: u8 = 0x20;

/// Whether signatures on a channel of type `ct` are unified.
pub fn is_unified(ct: &ChannelTypeFeatures) -> bool {
	ct.requires_unified_sigs()
}

/// The hash type byte for a bilateral signature of type `base`.
pub fn hash_type(base: EcdsaSighashType, unified: bool) -> u8 {
	let b = base as u8;
	if unified {
		b | SIGHASH_UNIFIED
	} else {
		b
	}
}

/// The digest a bilateral signature over input `idx` of `tx` signs: BIP 143
/// when `!unified`, the unified message otherwise. `script_code` is the P2WSH
/// witness script of the spent output, `value` its amount. Bilateral
/// Lightning transactions spend one input, so the spent output is known.
///
/// Panics if `unified`, `tx` has several inputs and `base` lacks
/// ANYONECANPAY: use [`sighash_with_prevouts`] for those.
pub fn p2wsh_sighash(
	tx: &Transaction, idx: usize, script_code: &Script, value: Amount, base: EcdsaSighashType,
	unified: bool,
) -> Message {
	if !unified {
		let h = SighashCache::new(tx).p2wsh_signature_hash(idx, script_code, value, base).unwrap();
		return Message::from_digest(h.to_byte_array());
	}
	let acp = (base as u8) & 0x80 != 0;
	assert!(
		acp || tx.input.len() == 1,
		"XBT: a unified SIGHASH_ALL over a multi-input transaction needs every prevout"
	);
	let prevout = TxOut { value, script_pubkey: script_code.to_p2wsh() };
	Message::from_digest(unified_message(
		tx,
		&[prevout],
		idx,
		script_code,
		1,
		hash_type(base, true),
	))
}

/// The multi-input form of [`p2wsh_sighash`], for splices and wallet inputs:
/// `prevouts` holds the output spent by every input of `tx`, in order.
pub fn sighash_with_prevouts(
	tx: &Transaction, prevouts: &[TxOut], idx: usize, script_code: &Script, base: EcdsaSighashType,
	unified: bool,
) -> Message {
	if !unified {
		let h = SighashCache::new(tx)
			.p2wsh_signature_hash(idx, script_code, prevouts[idx].value, base)
			.unwrap();
		return Message::from_digest(h.to_byte_array());
	}
	Message::from_digest(unified_message(
		tx,
		prevouts,
		idx,
		script_code,
		1,
		hash_type(base, true),
	))
}

/// Pushes `sig` with the bilateral hash type for `base`.
pub fn push_sig(w: &mut Witness, sig: &Signature, base: EcdsaSighashType, unified: bool) {
	let mut s = sig.serialize_der().to_vec();
	s.push(hash_type(base, unified));
	w.push(s);
}

fn sha(b: &[u8]) -> [u8; 32] {
	sha256::Hash::hash(b).to_byte_array()
}

fn put_compact_size(out: &mut Vec<u8>, n: u64) {
	if n < 0xFD {
		out.push(n as u8);
	} else if n <= 0xFFFF {
		out.push(0xFD);
		out.extend_from_slice(&(n as u16).to_le_bytes());
	} else if n <= 0xFFFF_FFFF {
		out.push(0xFE);
		out.extend_from_slice(&(n as u32).to_le_bytes());
	} else {
		out.push(0xFF);
		out.extend_from_slice(&n.to_le_bytes());
	}
}

fn put_script(out: &mut Vec<u8>, s: &Script) {
	put_compact_size(out, s.len() as u64);
	out.extend_from_slice(s.as_bytes());
}

/// `SignatureHashUnified` (Knots PR #357) for script types 0 and 1. With
/// ANYONECANPAY only the signed input's prevout is read, so `prevouts` may
/// hold just that one (at position 0) or one per input.
pub fn unified_message(
	tx: &Transaction, prevouts: &[TxOut], idx: usize, script_code: &Script, script_type: u8,
	hash_type: u8,
) -> [u8; 32] {
	let acp = hash_type & 0x80 != 0;
	let base = hash_type & 0x1f;
	let mut m: Vec<u8> = Vec::with_capacity(256);
	m.push(0); // epoch
	m.push(hash_type);
	m.extend_from_slice(&serialize(&tx.version));
	m.extend_from_slice(&serialize(&tx.lock_time));
	m.push(0);
	if !acp {
		assert_eq!(prevouts.len(), tx.input.len());
		let mut b = Vec::new();
		for i in &tx.input {
			b.extend_from_slice(&serialize(&i.previous_output));
		}
		m.extend_from_slice(&sha(&b));
		b.clear();
		for p in prevouts {
			b.extend_from_slice(&p.value.to_sat().to_le_bytes());
		}
		m.extend_from_slice(&sha(&b));
		b.clear();
		for p in prevouts {
			put_script(&mut b, &p.script_pubkey);
		}
		m.extend_from_slice(&sha(&b));
		b.clear();
		for i in &tx.input {
			b.extend_from_slice(&serialize(&i.sequence));
		}
		m.extend_from_slice(&sha(&b));
	}
	if base != 0x02 && base != 0x03 {
		let mut b = Vec::new();
		for o in &tx.output {
			b.extend_from_slice(&serialize(o));
		}
		m.extend_from_slice(&sha(&b));
	}
	m.push(script_type);
	if acp {
		let p = if prevouts.len() == tx.input.len() { &prevouts[idx] } else { &prevouts[0] };
		m.extend_from_slice(&serialize(&tx.input[idx].previous_output));
		m.extend_from_slice(&serialize(p));
		m.extend_from_slice(&serialize(&tx.input[idx].sequence));
	} else {
		m.extend_from_slice(&(idx as u32).to_le_bytes());
	}
	put_script(&mut m, script_code);
	if base == 0x03 {
		m.extend_from_slice(&sha(&serialize(&tx.output[idx])));
	}
	let tag = sha(b"UnifiedSighash");
	let mut t = Vec::with_capacity(64 + m.len());
	t.extend_from_slice(&tag);
	t.extend_from_slice(&tag);
	t.extend_from_slice(&m);
	sha(&t)
}

#[cfg(test)]
mod tests {
	use super::*;
	use bitcoin::consensus::encode::deserialize;

	const VECTORS: &str = include_str!("xbt_unified_sighash.json");

	#[test]
	fn unified_message_matches_knots_vectors() {
		// Rows: [scriptCode, rawTx, inIdx, hashType, scriptType, [[amount, spk]...], sighash]
		let rows: Vec<&str> = VECTORS.lines().filter(|l| l.trim_start().starts_with("[\"")).collect();
		let mut checked = 0;
		for row in rows.iter().skip(1) {
			let r = row.trim().trim_end_matches(',');
			let f = parse_row(r);
			if f.4 > 1 {
				continue;
			}
			let tx: Transaction = deserialize(&hex(&f.1)).unwrap();
			let prevouts: Vec<TxOut> = f
				.5
				.iter()
				.map(|(a, s)| TxOut { value: Amount::from_sat(*a), script_pubkey: hex(s).into() })
				.collect();
			let sc: bitcoin::ScriptBuf = hex(&f.0).into();
			let got = unified_message(&tx, &prevouts, f.2, &sc, f.4, f.3);
			assert_eq!(got.to_vec(), hex(&f.6), "hash type {}", f.3);
			checked += 1;
		}
		assert_eq!(checked, 142);
	}

	#[test]
	fn p2wsh_sighash_bip143_when_not_unified() {
		let sc: bitcoin::ScriptBuf = hex("5121020202020202020202020202020202020202020202020202020202020202020251ae").into();
		let tx = one_input_tx();
		let value = Amount::from_sat(100_000);
		let want = SighashCache::new(&tx)
			.p2wsh_signature_hash(0, &sc, value, EcdsaSighashType::All)
			.unwrap();
		let got = p2wsh_sighash(&tx, 0, &sc, value, EcdsaSighashType::All, false);
		assert_eq!(got.as_ref(), want.to_byte_array().as_slice());
		// and unified differs
		let u = p2wsh_sighash(&tx, 0, &sc, value, EcdsaSighashType::All, true);
		assert_ne!(u, got);
	}

	fn one_input_tx() -> Transaction {
		use bitcoin::{absolute::LockTime, transaction::Version, OutPoint, ScriptBuf, Sequence, TxIn};
		Transaction {
			version: Version::TWO,
			lock_time: LockTime::ZERO,
			input: vec![TxIn {
				previous_output: OutPoint::null(),
				script_sig: ScriptBuf::new(),
				sequence: Sequence::MAX,
				witness: Witness::new(),
			}],
			output: vec![TxOut { value: Amount::from_sat(90_000), script_pubkey: ScriptBuf::new() }],
		}
	}

	#[test]
	fn sighash_with_prevouts_matches_vector_multi_input() {
		// First row with several inputs, script type 1, hash type ALL without
		// ANYONECANPAY and with the unified bit.
		let mut found = false;
		for row in VECTORS.lines().filter(|l| l.trim_start().starts_with("[\"")).skip(1) {
			let f = parse_row(row.trim().trim_end_matches(','));
			let ht = f.3;
			if f.4 != 1 || ht & 0x1f != 1 || ht & 0x80 != 0 || ht & 0x20 == 0 {
				continue;
			}
			let tx: Transaction = deserialize(&hex(&f.1)).unwrap();
			if tx.input.len() < 2 {
				continue;
			}
			let prevouts: Vec<TxOut> = f
				.5
				.iter()
				.map(|(a, s)| TxOut { value: Amount::from_sat(*a), script_pubkey: hex(s).into() })
				.collect();
			let sc: bitcoin::ScriptBuf = hex(&f.0).into();
			let got = sighash_with_prevouts(&tx, &prevouts, f.2, &sc, EcdsaSighashType::All, true);
			assert_eq!(got.as_ref(), hex(&f.6).as_slice());
			found = true;
			break;
		}
		assert!(found, "no multi-input script-type-1 ALL vector");
	}

	#[test]
	fn hash_type_bytes() {
		assert_eq!(hash_type(EcdsaSighashType::All, true), 0x21);
		assert_eq!(hash_type(EcdsaSighashType::All, false), 0x01);
	}

	fn hex(s: &str) -> Vec<u8> {
		(0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
	}

	// A tiny parser for the vector rows (no serde_json in this crate's tests).
	fn parse_row(r: &str) -> (String, String, usize, u8, u8, Vec<(u64, String)>, String) {
		let inner = &r[1..r.len() - 1];
		let open = inner.find("[[").unwrap();
		let close = inner.rfind("]]").unwrap();
		let head: Vec<&str> = inner[..open].split(',').map(|s| s.trim()).collect();
		let tail = inner[close + 2..].trim().trim_start_matches(',').trim();
		let q = |s: &str| s.trim_matches('"').to_string();
		let pairs = inner[open + 2..close]
			.split("],")
			.map(|p| {
				let p = p.trim().trim_start_matches('[').trim_end_matches(']');
				let mut it = p.split(',');
				let a = it.next().unwrap().trim().parse().unwrap();
				(a, q(it.next().unwrap().trim()))
			})
			.collect();
		(q(head[0]), q(head[1]), head[2].parse().unwrap(), head[3].parse().unwrap(), head[4].parse().unwrap(), pairs, q(tail))
	}
}
