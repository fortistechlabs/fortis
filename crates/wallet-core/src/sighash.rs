//! Sighash routing, including the fork's opt-in `SIGHASH_UNIFIED`.
//!
//! The unified message is Bitcoin Knots PR #357 (`SIGHASH_UNIFIED = 0x20`, activated
//! by `DEPLOYMENT_BLAKE2B`). This port covers script types 0 (bare / P2SH) and 1
//! (segwit v0) — everything this wallet signs (P2WSH HTLC spends, P2WPKH funding
//! inputs). Taproot (types 2/3) is not implemented. Validated against all 142
//! non-taproot vectors in `src/test_data/unified_sighash.json`.

use bitcoin::consensus::encode::serialize;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::{Script, Transaction, TxOut};

use crate::error::{Result, WalletError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SighashVariant {
    /// BIP-143 segwit v0 (standard Bitcoin).
    SegwitV0,
    /// Knots BLAKE2b `SIGHASH_UNIFIED` — binds the spend to one chain so a spend of
    /// pre-fork coins cannot be replayed onto the other chain (Knots PR #357).
    Unified,
}

/// A sighash message plus the flag byte to append to the DER signature.
#[derive(Debug, Clone, Copy)]
pub struct SigHash {
    pub message: [u8; 32],
    /// `0x01` (`SIGHASH_ALL`) or `0x21` (`SIGHASH_ALL | SIGHASH_UNIFIED`).
    pub flag: u8,
}

const SCRIPT_TYPE_WITNESS_V0: u8 = 1;
const HASHTYPE_ALL: u8 = 0x01;
const HASHTYPE_ALL_UNIFIED: u8 = 0x21;

/// `SIGHASH_ALL` message for a segwit-v0 input. `script_code` is the BIP-143
/// scriptCode — the witnessScript for P2WSH, the implied P2PKH script for P2WPKH.
/// `prevouts` lists the spent output of every input in `tx`, in order (only the
/// signed input's entry is read for `SegwitV0`; all of them for `Unified`).
pub fn sighash_all(
    tx: &Transaction,
    prevouts: &[TxOut],
    input_index: usize,
    script_code: &Script,
    variant: SighashVariant,
) -> Result<SigHash> {
    match variant {
        SighashVariant::SegwitV0 => {
            let value = prevouts
                .get(input_index)
                .ok_or_else(|| WalletError::Bitcoin("prevout missing for input".into()))?
                .value;
            let mut cache = SighashCache::new(tx);
            let hash = cache
                .p2wsh_signature_hash(input_index, script_code, value, EcdsaSighashType::All)
                .map_err(|e| WalletError::Bitcoin(e.to_string()))?;
            Ok(SigHash { message: hash.to_byte_array(), flag: HASHTYPE_ALL })
        }
        SighashVariant::Unified => Ok(SigHash {
            message: unified_message(
                tx,
                prevouts,
                input_index,
                script_code,
                SCRIPT_TYPE_WITNESS_V0,
                HASHTYPE_ALL_UNIFIED,
            )?,
            flag: HASHTYPE_ALL_UNIFIED,
        }),
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    sha256::Hash::hash(bytes).to_byte_array()
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

/// The `SignatureHashUnified` message for script types 0 and 1 (Knots PR #357).
///
/// `hash_type` is the full byte (e.g. `0x21` for `ALL | UNIFIED`). Reproduces the
/// reference `ss <<` order exactly; validated against the project's vector file.
pub(crate) fn unified_message(
    tx: &Transaction,
    prevouts: &[TxOut],
    input_index: usize,
    script_code: &Script,
    script_type: u8,
    hash_type: u8,
) -> Result<[u8; 32]> {
    if prevouts.len() != tx.input.len() {
        return Err(WalletError::InvalidSwapParams(
            "prevouts length must equal input count".into(),
        ));
    }
    if input_index >= tx.input.len() {
        return Err(WalletError::InvalidSwapParams("input_index out of range".into()));
    }

    let anyonecanpay = hash_type & 0x80 != 0;
    let base = hash_type & 0x1f;

    let mut m: Vec<u8> = Vec::with_capacity(256);
    m.push(0); // epoch
    m.push(hash_type);
    m.extend_from_slice(&serialize(&tx.version)); // int32 LE
    m.extend_from_slice(&serialize(&tx.lock_time)); // uint32 LE
    m.push(0); // 5th, currently-zero locktime byte

    if !anyonecanpay {
        let mut b = Vec::new();
        for i in &tx.input {
            b.extend_from_slice(&serialize(&i.previous_output));
        }
        m.extend_from_slice(&sha256(&b)); // sha_prevouts

        b.clear();
        for p in prevouts {
            b.extend_from_slice(&p.value.to_sat().to_le_bytes());
        }
        m.extend_from_slice(&sha256(&b)); // sha_amounts

        b.clear();
        for p in prevouts {
            put_script(&mut b, &p.script_pubkey);
        }
        m.extend_from_slice(&sha256(&b)); // sha_scripts

        b.clear();
        for i in &tx.input {
            b.extend_from_slice(&serialize(&i.sequence));
        }
        m.extend_from_slice(&sha256(&b)); // sha_sequences
    }

    // Committed for every hash type that is not NONE or SINGLE (ALL, and the rest).
    if base != 0x02 && base != 0x03 {
        let mut b = Vec::new();
        for o in &tx.output {
            b.extend_from_slice(&serialize(o));
        }
        m.extend_from_slice(&sha256(&b)); // sha_outputs
    }

    m.push(script_type);

    if anyonecanpay {
        m.extend_from_slice(&serialize(&tx.input[input_index].previous_output));
        m.extend_from_slice(&serialize(&prevouts[input_index]));
        m.extend_from_slice(&serialize(&tx.input[input_index].sequence));
    } else {
        m.extend_from_slice(&(input_index as u32).to_le_bytes());
    }

    put_script(&mut m, script_code); // non-taproot: `ss << scriptCode`

    if base == 0x03 {
        let out = tx
            .output
            .get(input_index)
            .ok_or_else(|| WalletError::Bitcoin("SIGHASH_SINGLE with no output at index".into()))?;
        m.extend_from_slice(&sha256(&serialize(out)));
    }

    // TaggedHash("UnifiedSighash", m) — BIP-340 tag prefix, single SHA256.
    let tag = sha256(b"UnifiedSighash");
    let mut tagged = Vec::with_capacity(64 + m.len());
    tagged.extend_from_slice(&tag);
    tagged.extend_from_slice(&tag);
    tagged.extend_from_slice(&m);
    Ok(sha256(&tagged))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::consensus::encode::deserialize;
    use bitcoin::Amount;

    /// Vendored from Bitcoin Knots PR #357 (`privkeyio:hf-sighash-opt-in`,
    /// `src/test/data/unified_sighash.json`). Schema:
    /// `[scriptCode, rawTx, inIdx, hashType, scriptType, [[amount, spkHex], ...], sighash]`.
    const VECTORS: &str = include_str!("test_data/unified_sighash.json");

    #[test]
    fn unified_message_matches_reference_vectors() {
        let json: serde_json::Value = serde_json::from_str(VECTORS).unwrap();
        let rows = json.as_array().unwrap();

        let mut checked = 0;
        let mut skipped = 0;
        for row in &rows[1..] {
            let v = row.as_array().unwrap();
            let script_code = hex_script(v[0].as_str().unwrap());
            let raw_tx = hex::decode(v[1].as_str().unwrap()).unwrap();
            let in_idx = v[2].as_u64().unwrap() as usize;
            let hash_type = v[3].as_u64().unwrap() as u8;
            let script_type = v[4].as_u64().unwrap() as u8;
            let expected = v[6].as_str().unwrap();

            if script_type > 1 {
                skipped += 1; // taproot / tapscript — not implemented
                continue;
            }

            let tx: Transaction = deserialize(&raw_tx).unwrap();
            let prevouts: Vec<TxOut> = v[5]
                .as_array()
                .unwrap()
                .iter()
                .map(|pair| {
                    let p = pair.as_array().unwrap();
                    TxOut {
                        value: Amount::from_sat(p[0].as_u64().unwrap()),
                        script_pubkey: hex_script(p[1].as_str().unwrap()),
                    }
                })
                .collect();

            let got = unified_message(
                &tx,
                &prevouts,
                in_idx,
                &script_code,
                script_type,
                hash_type,
            )
            .unwrap();

            assert_eq!(hex::encode(got), expected, "vector hashType={hash_type}");
            checked += 1;
        }

        assert_eq!(checked, 142);
        assert_eq!(skipped, 24);
    }

    fn hex_script(s: &str) -> bitcoin::ScriptBuf {
        bitcoin::ScriptBuf::from(hex::decode(s).unwrap())
    }
}
