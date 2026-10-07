//! Turns transactions into the rows the index stores. A P2WPKH spend has an
//! empty scriptSig and exactly two witness items, and consensus requires
//! HASH160(witness[1]) to equal the spent output's 20-byte program, so a spend
//! is attributed to its address without any database lookup.

use anyhow::Result;
use bitcoin::hashes::{hash160, Hash};
use bitcoin::{BlockHash, OutPoint, Transaction, TxIn, Txid};

use crate::chain::{parse_block, HeaderFormat};
use crate::keys::Program;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Funded {
    pub program: Program,
    pub vout: u32,
    pub value: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spent {
    pub program: Program,
    pub prevout: OutPoint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxRows {
    pub txid: Txid,
    pub funded: Vec<Funded>,
    pub spent: Vec<Spent>,
}

pub struct ParsedBlock {
    pub height: u32,
    pub hash: BlockHash,
    pub prev: BlockHash,
    pub time: u32,
    pub txs: Vec<TxRows>,
}

/// The program a P2WPKH input spends: an empty scriptSig and exactly two
/// witness items yield HASH160(witness[1]). The witness[1] length is not
/// checked, so this is a superset that can never miss a real P2WPKH spend;
/// a false positive yields a program nobody owns, which is harmless.
pub fn spend_program(input: &TxIn) -> Option<Program> {
    if !input.script_sig.is_empty() || input.witness.len() != 2 {
        return None;
    }
    let pk = input.witness.nth(1)?;
    Some(hash160::Hash::hash(pk).to_byte_array())
}

/// `None` if the tx funds and spends no program. The txid is computed only
/// when the result is `Some`. Coinbase inputs never count as spends.
pub fn tx_rows(tx: &Transaction) -> Option<TxRows> {
    let funded: Vec<Funded> = tx
        .output
        .iter()
        .enumerate()
        .filter(|(_, o)| o.script_pubkey.is_p2wpkh())
        .map(|(i, o)| {
            let mut program = [0u8; 20];
            program.copy_from_slice(&o.script_pubkey.as_bytes()[2..22]);
            Funded {
                program,
                vout: i as u32,
                value: o.value.to_sat(),
            }
        })
        .collect();
    let spent: Vec<Spent> = if tx.is_coinbase() {
        Vec::new()
    } else {
        tx.input
            .iter()
            .filter_map(|i| {
                spend_program(i).map(|program| Spent {
                    program,
                    prevout: i.previous_output,
                })
            })
            .collect()
    };
    if funded.is_empty() && spent.is_empty() {
        return None;
    }
    Some(TxRows {
        txid: tx.compute_txid(),
        funded,
        spent,
    })
}

pub fn parse_and_extract(
    height: u32,
    hash: BlockHash,
    bytes: &[u8],
    fmt: &HeaderFormat,
) -> Result<ParsedBlock> {
    let body = parse_block(bytes, fmt.header_len(height))?;
    Ok(ParsedBlock {
        height,
        hash,
        prev: body.prev,
        time: body.time,
        txs: body.txs.iter().filter_map(tx_rows).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::absolute::LockTime;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::{Secp256k1, SecretKey};
    use bitcoin::{
        transaction::Version, Amount, CompressedPublicKey, PrivateKey, ScriptBuf, Sequence, TxOut,
        Witness,
    };

    fn pubkey() -> (CompressedPublicKey, Program) {
        let secp = Secp256k1::new();
        let sk = SecretKey::from_slice(&[7u8; 32]).unwrap();
        let pk = CompressedPublicKey::from_private_key(
            &secp,
            &PrivateKey::new(sk, bitcoin::Network::Bitcoin),
        )
        .unwrap();
        let program = pk.wpubkey_hash().to_byte_array();
        (pk, program)
    }

    fn input(script_sig: ScriptBuf, witness: Witness) -> TxIn {
        TxIn {
            previous_output: OutPoint::new(Txid::from_byte_array([9; 32]), 3),
            script_sig,
            sequence: Sequence::MAX,
            witness,
        }
    }

    fn tx(inputs: Vec<TxIn>, outputs: Vec<TxOut>) -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: inputs,
            output: outputs,
        }
    }

    fn out(script: ScriptBuf, sats: u64) -> TxOut {
        TxOut {
            value: Amount::from_sat(sats),
            script_pubkey: script,
        }
    }

    #[test]
    fn a_p2wpkh_spend_is_attributed_from_its_witness() {
        let (pk, program) = pubkey();
        let addr = bitcoin::Address::p2wpkh(&pk, bitcoin::Network::Bitcoin);
        assert_eq!(&addr.script_pubkey().as_bytes()[2..22], &program[..]);
        let w = Witness::from_slice(&[vec![0u8; 72], pk.to_bytes().to_vec()]);
        assert_eq!(spend_program(&input(ScriptBuf::new(), w)), Some(program));
    }

    #[test]
    fn coinbase_taproot_and_wrapped_inputs_are_not_p2wpkh_spends() {
        let (pk, _) = pubkey();
        let two = Witness::from_slice(&[vec![0u8; 72], pk.to_bytes().to_vec()]);
        let coinbase = TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(vec![0x03, 1, 2, 3]),
            sequence: Sequence::MAX,
            witness: Witness::from_slice(&[vec![0u8; 32]]),
        };
        assert_eq!(spend_program(&coinbase), None);
        let taproot = input(ScriptBuf::new(), Witness::from_slice(&[vec![0u8; 64]]));
        assert_eq!(spend_program(&taproot), None);
        let wrapped = input(ScriptBuf::from_bytes(vec![0x16, 0, 0x14]), two);
        assert_eq!(spend_program(&wrapped), None);
        // a coinbase never counts as a spend in tx_rows either
        assert_eq!(tx_rows(&tx(vec![coinbase], vec![])), None);
    }

    #[test]
    fn outputs_keep_their_true_vout_and_only_p2wpkh_counts() {
        let (pk, program) = pubkey();
        let net = bitcoin::Network::Bitcoin;
        let p2wpkh = bitcoin::Address::p2wpkh(&pk, net).script_pubkey();
        let p2pkh = bitcoin::Address::p2pkh(pk, net).script_pubkey();
        let secp = Secp256k1::new();
        let (xonly, _) = SecretKey::from_slice(&[8u8; 32])
            .unwrap()
            .public_key(&secp)
            .x_only_public_key();
        let p2tr = ScriptBuf::new_p2tr_tweaked(
            bitcoin::key::TweakedPublicKey::dangerous_assume_tweaked(xonly),
        );
        let op_return = ScriptBuf::from_bytes(vec![0x6a, 0x01, 0x00]);
        let t = tx(
            vec![input(ScriptBuf::from_bytes(vec![1]), Witness::new())],
            vec![
                out(op_return, 0),
                out(p2wpkh, 2000),
                out(p2pkh, 5),
                out(p2tr, 6),
            ],
        );
        let rows = tx_rows(&t).unwrap();
        assert_eq!(
            rows.funded,
            vec![Funded {
                program,
                vout: 1,
                value: 2000
            }]
        );
        assert!(rows.spent.is_empty());
        assert_eq!(rows.txid, t.compute_txid());
    }

    #[test]
    fn a_tx_touching_no_program_yields_none() {
        let t = tx(
            vec![input(ScriptBuf::from_bytes(vec![1]), Witness::new())],
            vec![out(ScriptBuf::from_bytes(vec![0x6a]), 0)],
        );
        assert_eq!(tx_rows(&t), None);
    }

    #[test]
    fn the_real_xbt_block_extracts_without_error() {
        let bytes = hex::decode(include_str!("../tests/fixtures/xbt-976000.hex").trim()).unwrap();
        let hash: BlockHash = "000000000000000098441aee029573795681eb1602c75271e809b136e9217373"
            .parse()
            .unwrap();
        let fmt = HeaderFormat {
            v2_from: Some(961640),
        };
        let b = parse_and_extract(976000, hash, &bytes, &fmt).unwrap();
        assert_eq!(b.height, 976000);
        assert_eq!(b.hash, hash);
        for t in &b.txs {
            assert!(!t.funded.is_empty() || !t.spent.is_empty());
        }
    }
}
