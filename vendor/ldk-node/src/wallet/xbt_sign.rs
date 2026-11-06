// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! FORTIS XBT: the on-chain wallet's signer on the Bitcoin BLAKE2b chain (XBT).
//!
//! Coins from before the fork exist on both chains, so a spend signed with a plain hash type
//! could be replayed onto Bitcoin. On XBT every P2WPKH input the wallet spends is signed with
//! `SIGHASH_ALL | SIGHASH_UNIFIED` (`0x21`) over the unified digest (Knots PR #357), which
//! commits to the chain and to every prevout of the transaction.

use bitcoin::bip32::DerivationPath;
use bitcoin::psbt::Psbt;
use bitcoin::secp256k1::{All, Secp256k1, SecretKey};
use bitcoin::sighash::EcdsaSighashType;
use bitcoin::{CompressedPublicKey, ScriptBuf, TxOut, Witness};
use lightning::xbt::{hash_type, sighash_with_prevouts};

/// The BIP 143 script code of a P2WPKH input: the implied P2PKH script
/// (`OP_DUP OP_HASH160 <20> OP_EQUALVERIFY OP_CHECKSIG`), as wallet-core's `sign_p2wpkh_tx`.
fn p2pkh_script_code(pk: &CompressedPublicKey) -> ScriptBuf {
	ScriptBuf::new_p2pkh(&pk.pubkey_hash())
}

/// Signs every input of `psbt` that spends a P2WPKH output of a key we hold with
/// `SIGHASH_ALL | SIGHASH_UNIFIED` (`0x21`) over every prevout, and finalizes it with the
/// witness `[sig || 0x21, pubkey]`.
///
/// An input is ours when `prevouts[i]` is P2WPKH and one of its `bip32_derivation` paths
/// derives, through `keys`, to the key that output pays. Other inputs are left untouched.
/// `prevouts[i]` is the output `psbt.unsigned_tx.input[i]` spends; it must hold one per input
/// (the digest commits to all of them), and agree with any `witness_utxo` the PSBT carries.
///
/// Returns the number of inputs signed. On `Err` nothing was signed.
pub(crate) fn sign_xbt_p2wpkh(
	psbt: &mut Psbt, prevouts: &[TxOut], keys: &impl Fn(&DerivationPath) -> Option<SecretKey>,
	secp: &Secp256k1<All>,
) -> Result<usize, ()> {
	let tx = &psbt.unsigned_tx;
	if prevouts.len() != tx.input.len() || psbt.inputs.len() != tx.input.len() {
		return Err(());
	}
	let mut witnesses = Vec::new();
	for (i, input) in psbt.inputs.iter().enumerate() {
		if input.witness_utxo.as_ref().map_or(false, |w| *w != prevouts[i]) {
			return Err(());
		}
		let spk = &prevouts[i].script_pubkey;
		if !spk.is_p2wpkh() {
			continue;
		}
		let ours = input.bip32_derivation.iter().find_map(|(pk, (_, path))| {
			let sk = keys(path)?;
			let cpk = CompressedPublicKey(*pk);
			(sk.public_key(secp) == *pk && ScriptBuf::new_p2wpkh(&cpk.wpubkey_hash()) == *spk)
				.then_some((sk, cpk))
		});
		let Some((sk, cpk)) = ours else { continue };
		let msg = sighash_with_prevouts(
			tx,
			prevouts,
			i,
			&p2pkh_script_code(&cpk),
			EcdsaSighashType::All,
			true,
		);
		let mut sig = secp.sign_ecdsa(&msg, &sk).serialize_der().to_vec();
		sig.push(hash_type(EcdsaSighashType::All, true));
		let mut witness = Witness::new();
		witness.push(sig);
		witness.push(cpk.to_bytes());
		witnesses.push((i, witness));
	}
	let signed = witnesses.len();
	for (i, witness) in witnesses {
		psbt.inputs[i].final_script_witness = Some(witness);
	}
	Ok(signed)
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;
	use std::sync::Arc;

	use bdk_wallet::template::Bip84;
	use bdk_wallet::{KeychainKind, Wallet as BdkWallet};
	use bitcoin::bip32::{DerivationPath, Fingerprint, Xpriv};
	use bitcoin::blockdata::constants::genesis_block;
	use bitcoin::hashes::Hash;
	use bitcoin::psbt::Psbt;
	use bitcoin::secp256k1::ecdsa::Signature;
	use bitcoin::secp256k1::{Message, PublicKey};
	use bitcoin::{
		absolute::LockTime, transaction::Version, Address, Amount, Block, CompressedPublicKey,
		Network, OutPoint, Sequence, Transaction, TxIn, Txid,
	};

	use super::*;
	use crate::config::{Config, XbtConfig};
	use crate::fee_estimator::OnchainFeeEstimator;
	use crate::io::test_utils::InMemoryStore;
	use crate::logger::Logger;
	use crate::tx_broadcaster::TransactionBroadcaster;
	use crate::types::{DynStore, PaymentStore};
	use crate::wallet::persist::KVStoreWalletPersister;
	use crate::wallet::{OnchainSendAmount, Wallet};

	const UNIFIED_ALL: u8 = 0x21;

	fn test_xprv() -> Xpriv {
		Xpriv::new_master(Network::Regtest, &[42u8; 64]).unwrap()
	}

	fn path(s: &str) -> DerivationPath {
		DerivationPath::from_str(s).unwrap()
	}

	fn p2wpkh(pk: &PublicKey) -> ScriptBuf {
		ScriptBuf::new_p2wpkh(&CompressedPublicKey(*pk).wpubkey_hash())
	}

	fn p2pkh_code(pk: &PublicKey) -> ScriptBuf {
		ScriptBuf::new_p2pkh(&bitcoin::PublicKey::new(*pk).pubkey_hash())
	}

	/// An unsigned transaction spending one P2WPKH output per entry of `spends`
	/// (`(owner key, PSBT derivation, amount)`), as a PSBT carrying the given derivations.
	fn psbt_spending(spends: &[(PublicKey, Option<DerivationPath>, u64)]) -> (Psbt, Vec<TxOut>) {
		let tx = Transaction {
			version: Version::TWO,
			lock_time: LockTime::ZERO,
			input: spends
				.iter()
				.enumerate()
				.map(|(i, _)| TxIn {
					previous_output: OutPoint {
						txid: Txid::from_byte_array([i as u8 + 1; 32]),
						vout: i as u32,
					},
					sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
					..Default::default()
				})
				.collect(),
			output: vec![TxOut {
				value: Amount::from_sat(70_000),
				script_pubkey: ScriptBuf::new_p2wsh(&bitcoin::WScriptHash::all_zeros()),
			}],
		};
		let prevouts: Vec<TxOut> = spends
			.iter()
			.map(|(pk, _, amt)| TxOut { value: Amount::from_sat(*amt), script_pubkey: p2wpkh(pk) })
			.collect();
		let mut psbt = Psbt::from_unsigned_tx(tx).unwrap();
		for (i, (pk, deriv, _)) in spends.iter().enumerate() {
			if let Some(p) = deriv {
				psbt.inputs[i].bip32_derivation.insert(*pk, (Fingerprint::default(), p.clone()));
			}
		}
		(psbt, prevouts)
	}

	/// Checks that `witness` is `[sig || 0x21, pubkey]` and that `sig` signs the unified
	/// digest of input `idx` over every prevout in `prevouts`.
	fn assert_unified(tx: &Transaction, prevouts: &[TxOut], idx: usize, witness: &Witness) {
		let secp = Secp256k1::verification_only();
		assert_eq!(witness.len(), 2, "input {idx}: witness is [sig, pubkey]");
		let sig = witness.nth(0).unwrap();
		assert_eq!(*sig.last().unwrap(), UNIFIED_ALL, "input {idx}: hash type byte");
		let pk = PublicKey::from_slice(witness.nth(1).unwrap()).unwrap();
		assert_eq!(prevouts[idx].script_pubkey, p2wpkh(&pk), "input {idx}: key owns the prevout");
		let sig = Signature::from_der(&sig[..sig.len() - 1]).unwrap();
		let digest = lightning::xbt::unified_message(
			tx,
			prevouts,
			idx,
			&p2pkh_code(&pk),
			1, // segwit v0
			UNIFIED_ALL,
		);
		secp.verify_ecdsa(&Message::from_digest(digest), &sig, &pk)
			.unwrap_or_else(|e| panic!("input {idx}: unified signature does not verify: {e}"));

		// The digest commits to every prevout: change another input's amount and it fails.
		if prevouts.len() > 1 {
			let mut other = prevouts.to_vec();
			let j = (idx + 1) % other.len();
			other[j].value += Amount::from_sat(1);
			let d = lightning::xbt::unified_message(tx, &other, idx, &p2pkh_code(&pk), 1, UNIFIED_ALL);
			assert!(secp.verify_ecdsa(&Message::from_digest(d), &sig, &pk).is_err());
		}
	}

	fn derive_keys<'a>(
		xprv: &'a Xpriv, secp: &'a Secp256k1<All>,
	) -> impl Fn(&DerivationPath) -> Option<SecretKey> + 'a {
		move |p: &DerivationPath| xprv.derive_priv(secp, p).ok().map(|k| k.private_key)
	}

	#[test]
	fn xbt_sign_p2wpkh_unified() {
		let secp = Secp256k1::new();
		let xprv = test_xprv();
		let p0 = path("m/84'/1'/0'/0/0");
		let p1 = path("m/84'/1'/0'/1/7");
		let pk0 = xprv.derive_priv(&secp, &p0).unwrap().private_key.public_key(&secp);
		let pk1 = xprv.derive_priv(&secp, &p1).unwrap().private_key.public_key(&secp);
		let (mut psbt, prevouts) =
			psbt_spending(&[(pk0, Some(p0), 50_000), (pk1, Some(p1), 30_000)]);

		let signed = sign_xbt_p2wpkh(&mut psbt, &prevouts, &derive_keys(&xprv, &secp), &secp);
		assert_eq!(signed, Ok(2));

		let tx = psbt.unsigned_tx.clone();
		for i in 0..2 {
			assert_unified(&tx, &prevouts, i, psbt.inputs[i].final_script_witness.as_ref().unwrap());
		}
	}

	#[test]
	fn xbt_sign_skips_foreign_inputs() {
		let secp = Secp256k1::new();
		let xprv = test_xprv();
		let p0 = path("m/84'/1'/0'/0/0");
		let pk0 = xprv.derive_priv(&secp, &p0).unwrap().private_key.public_key(&secp);
		// A key we do not hold, even though its PSBT entry names a path we can derive.
		let foreign = Xpriv::new_master(Network::Regtest, &[7u8; 64]).unwrap();
		let pf = path("m/84'/1'/0'/0/1");
		let pkf = foreign.derive_priv(&secp, &pf).unwrap().private_key.public_key(&secp);
		let (mut psbt, prevouts) =
			psbt_spending(&[(pk0, Some(p0), 50_000), (pkf, Some(pf), 30_000)]);

		let signed = sign_xbt_p2wpkh(&mut psbt, &prevouts, &derive_keys(&xprv, &secp), &secp);
		assert_eq!(signed, Ok(1));
		assert_unified(
			&psbt.unsigned_tx,
			&prevouts,
			0,
			psbt.inputs[0].final_script_witness.as_ref().unwrap(),
		);
		assert!(psbt.inputs[1].final_script_witness.is_none());
		assert!(psbt.unsigned_tx.input[1].witness.is_empty());
	}

	#[test]
	fn xbt_sign_refuses_missing_prevouts() {
		let secp = Secp256k1::new();
		let xprv = test_xprv();
		let p0 = path("m/84'/1'/0'/0/0");
		let pk0 = xprv.derive_priv(&secp, &p0).unwrap().private_key.public_key(&secp);
		let (mut psbt, prevouts) =
			psbt_spending(&[(pk0, Some(p0.clone()), 50_000), (pk0, Some(p0), 30_000)]);
		let res = sign_xbt_p2wpkh(&mut psbt, &prevouts[..1], &derive_keys(&xprv, &secp), &secp);
		assert_eq!(res, Err(()));
		assert!(psbt.inputs.iter().all(|i| i.final_script_witness.is_none()));
	}

	/// The same transaction signed by wallet-core (the app's XBT signer, `sign_p2wpkh_tx` with
	/// the BLAKE2b chain) gives byte-identical witnesses.
	#[test]
	fn xbt_sign_matches_wallet_core() {
		const PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
		let secp = Secp256k1::new();
		let params = wallet_core::ChainParams::blake2b();
		let core_key = wallet_core::MasterKey::from_phrase(PHRASE, "").unwrap();

		let seed = bip39::Mnemonic::parse(PHRASE).unwrap().to_seed("");
		let xprv = Xpriv::new_master(Network::Bitcoin, &seed).unwrap();
		// wallet-core's paths are m/84'/0'/account'/branch/index, as BDK's Bip84 on mainnet.
		let p0 = path("m/84'/0'/0'/0/0");
		let p1 = path("m/84'/0'/0'/1/3");
		let pk0 = xprv.derive_priv(&secp, &p0).unwrap().private_key.public_key(&secp);
		let pk1 = xprv.derive_priv(&secp, &p1).unwrap().private_key.public_key(&secp);
		let (mut psbt, prevouts) =
			psbt_spending(&[(pk0, Some(p0), 50_000), (pk1, Some(p1), 30_000)]);

		let mut core_tx = psbt.unsigned_tx.clone();
		core_key
			.sign_p2wpkh_tx(&params, 0, &mut core_tx, &prevouts, &[(false, 0), (true, 3)])
			.unwrap();

		assert_eq!(sign_xbt_p2wpkh(&mut psbt, &prevouts, &derive_keys(&xprv, &secp), &secp), Ok(2));
		for i in 0..2 {
			assert_eq!(
				psbt.inputs[i].final_script_witness.as_ref().unwrap(),
				&core_tx.input[i].witness,
				"input {i}"
			);
		}
	}

	// --- The wallet's signing paths ---

	struct TestWallet {
		wallet: Wallet,
		broadcaster: Arc<crate::types::Broadcaster>,
		/// The two wallet outputs funded at height 1.
		utxos: Vec<(OutPoint, TxOut)>,
	}

	fn test_wallet(xbt: bool) -> TestWallet {
		let xprv = test_xprv();
		let store: Arc<DynStore> = Arc::new(InMemoryStore::new());
		let logger = Arc::new(Logger::new_log_facade());
		let mut persister = KVStoreWalletPersister::new(Arc::clone(&store), Arc::clone(&logger));
		let mut bdk = BdkWallet::create(
			Bip84(xprv, KeychainKind::External),
			Bip84(xprv, KeychainKind::Internal),
		)
		.network(Network::Regtest)
		.create_wallet(&mut persister)
		.unwrap();

		let a0 = bdk.reveal_next_address(KeychainKind::External).address;
		let a1 = bdk.reveal_next_address(KeychainKind::Internal).address;
		let funding = Transaction {
			version: Version::TWO,
			lock_time: LockTime::ZERO,
			input: vec![TxIn {
				previous_output: OutPoint { txid: Txid::from_byte_array([9; 32]), vout: 0 },
				..Default::default()
			}],
			output: vec![
				TxOut { value: Amount::from_sat(100_000), script_pubkey: a0.script_pubkey() },
				TxOut { value: Amount::from_sat(60_000), script_pubkey: a1.script_pubkey() },
			],
		};
		let genesis = genesis_block(Network::Regtest);
		let mut header = genesis.header;
		header.prev_blockhash = genesis.block_hash();
		let block = Block { header, txdata: vec![funding.clone()] };
		bdk.apply_block(&block, 1).unwrap();
		let txid = funding.compute_txid();
		let utxos =
			(0..2).map(|v| (OutPoint { txid, vout: v }, funding.output[v as usize].clone())).collect();

		let mut config = Config::default();
		config.network = Network::Regtest;
		config.anchor_channels_config = None;
		if xbt {
			config.xbt = Some(XbtConfig::default());
		}
		let payment_store = Arc::new(PaymentStore::new(
			Vec::new(),
			"payments".to_string(),
			String::new(),
			Arc::clone(&store),
			Arc::clone(&logger),
		));
		let broadcaster = Arc::new(TransactionBroadcaster::new(Arc::clone(&logger)));
		let wallet = Wallet::new(
			bdk,
			persister,
			Arc::clone(&broadcaster),
			Arc::new(OnchainFeeEstimator::new()),
			payment_store,
			Arc::new(config),
			logger,
			xprv,
		);
		TestWallet { wallet, broadcaster, utxos }
	}

	fn foreign_utxo() -> (OutPoint, TxOut) {
		let secp = Secp256k1::new();
		let k = Xpriv::new_master(Network::Regtest, &[7u8; 64]).unwrap().private_key;
		(
			OutPoint { txid: Txid::from_byte_array([5; 32]), vout: 3 },
			TxOut { value: Amount::from_sat(40_000), script_pubkey: p2wpkh(&k.public_key(&secp)) },
		)
	}

	/// A transaction spending `spends` to one P2WSH output.
	fn spend_tx(spends: &[(OutPoint, TxOut)]) -> Transaction {
		Transaction {
			version: Version::TWO,
			lock_time: LockTime::ZERO,
			input: spends
				.iter()
				.map(|(op, _)| TxIn { previous_output: *op, ..Default::default() })
				.collect(),
			output: vec![TxOut {
				value: Amount::from_sat(150_000),
				script_pubkey: ScriptBuf::new_p2wsh(&bitcoin::WScriptHash::all_zeros()),
			}],
		}
	}

	fn psbt_with_witness_utxos(spends: &[(OutPoint, TxOut)]) -> Psbt {
		let mut psbt = Psbt::from_unsigned_tx(spend_tx(spends)).unwrap();
		for (i, (_, txout)) in spends.iter().enumerate() {
			psbt.inputs[i].witness_utxo = Some(txout.clone());
		}
		psbt
	}

	fn prevouts_of(spends: &[(OutPoint, TxOut)]) -> Vec<TxOut> {
		spends.iter().map(|(_, t)| t.clone()).collect()
	}

	fn last_broadcast(broadcaster: &crate::types::Broadcaster) -> Transaction {
		let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
		rt.block_on(async {
			let mut queue = broadcaster.get_broadcast_queue().await;
			queue.try_recv().unwrap().pop().unwrap()
		})
	}

	/// Prevouts of `tx` taken from the outputs the test wallet was funded with.
	fn funded_prevouts(t: &TestWallet, tx: &Transaction) -> Vec<TxOut> {
		tx.input
			.iter()
			.map(|i| t.utxos.iter().find(|(op, _)| *op == i.previous_output).unwrap().1.clone())
			.collect()
	}

	#[test]
	fn btc_wallet_still_bdk() {
		let t = test_wallet(false);
		let tx = t.wallet.sign_psbt_inner(psbt_with_witness_utxos(&t.utxos)).unwrap();
		for input in &tx.input {
			assert_eq!(input.witness.len(), 2);
			assert_eq!(*input.witness.nth(0).unwrap().last().unwrap(), 0x01);
		}
	}

	#[test]
	fn xbt_wallet_sign_psbt_unified() {
		let t = test_wallet(true);
		let mut spends = t.utxos.clone();
		spends.push(foreign_utxo());
		let tx = t.wallet.sign_psbt_inner(psbt_with_witness_utxos(&spends)).unwrap();
		let prevouts = prevouts_of(&spends);
		assert_unified(&tx, &prevouts, 0, &tx.input[0].witness);
		assert_unified(&tx, &prevouts, 1, &tx.input[1].witness);
		assert!(tx.input[2].witness.is_empty());

		// Without a foreign input's prevout the wallet cannot sign over all of them.
		let mut psbt = psbt_with_witness_utxos(&spends);
		psbt.inputs[2].witness_utxo = None;
		assert!(t.wallet.sign_psbt_inner(psbt).is_err());
	}

	#[test]
	fn xbt_wallet_sign_owned_inputs_unified() {
		let t = test_wallet(true);
		// A splice: a foreign input (standing in for the old funding output) and ours.
		let mut spends = vec![foreign_utxo()];
		spends.extend(t.utxos.iter().cloned());
		let prevouts = prevouts_of(&spends);
		let tx = t.wallet.sign_owned_inputs(spend_tx(&spends), &prevouts).unwrap();
		assert!(tx.input[0].witness.is_empty());
		assert_unified(&tx, &prevouts, 1, &tx.input[1].witness);
		assert_unified(&tx, &prevouts, 2, &tx.input[2].witness);

		// Missing or mismatched prevouts are refused rather than signed.
		assert!(t.wallet.sign_owned_inputs(spend_tx(&spends), &prevouts[..2]).is_err());
		let mut wrong = prevouts.clone();
		wrong[1].value += Amount::from_sat(1);
		assert!(t.wallet.sign_owned_inputs(spend_tx(&spends), &wrong).is_err());
	}

	#[test]
	fn xbt_wallet_funding_transaction_unified() {
		let t = test_wallet(true);
		let out = ScriptBuf::new_p2wsh(&bitcoin::WScriptHash::all_zeros());
		let tx = t
			.wallet
			.create_funding_transaction(
				out,
				Amount::from_sat(120_000),
				crate::fee_estimator::ConfirmationTarget::ChannelFunding,
				LockTime::ZERO,
			)
			.unwrap();
		let prevouts = funded_prevouts(&t, &tx);
		assert_eq!(tx.input.len(), 2);
		for i in 0..tx.input.len() {
			assert_unified(&tx, &prevouts, i, &tx.input[i].witness);
		}
	}

	#[test]
	fn xbt_wallet_send_and_sweep_unified() {
		let t = test_wallet(true);
		let secp = Secp256k1::new();
		let k = Xpriv::new_master(Network::Regtest, &[3u8; 64]).unwrap().private_key;
		let to = Address::p2wpkh(&CompressedPublicKey(k.public_key(&secp)), Network::Regtest);

		let send = OnchainSendAmount::ExactRetainingReserve {
			amount_sats: 20_000,
			cur_anchor_reserve_sats: 0,
		};
		t.wallet.send_to_address(&to, send, None).unwrap();
		let tx = last_broadcast(&t.broadcaster);
		let prevouts = funded_prevouts(&t, &tx);
		for i in 0..tx.input.len() {
			assert_unified(&tx, &prevouts, i, &tx.input[i].witness);
		}

		let t = test_wallet(true);
		t.wallet.send_to_address(&to, OnchainSendAmount::AllDrainingReserve, None).unwrap();
		let tx = last_broadcast(&t.broadcaster);
		assert_eq!(tx.input.len(), 2);
		let prevouts = funded_prevouts(&t, &tx);
		for i in 0..tx.input.len() {
			assert_unified(&tx, &prevouts, i, &tx.input[i].witness);
		}
	}
}
