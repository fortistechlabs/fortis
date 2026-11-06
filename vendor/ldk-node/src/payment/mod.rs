// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Objects for different types of payments.

pub(crate) mod asynchronous;
mod bolt11;
mod bolt12;
mod onchain;
mod spontaneous;
pub(crate) mod store;
mod unified_qr;

pub use bolt11::Bolt11Payment;
pub use bolt12::Bolt12Payment;
pub use onchain::OnchainPayment;
pub use spontaneous::SpontaneousPayment;
pub use store::{
	ConfirmationStatus, LSPFeeLimits, PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus,
};
pub use unified_qr::{QrPaymentResult, UnifiedQrPayment};

use lightning::offers::offer::Offer;
use lightning_invoice::Bolt11Invoice;

use crate::error::Error;

// FORTIS XBT: XBT keeps BTC's chain hash and `lnbc` prefix, so `option_blake2b` (bits 512/513) is
// the only thing that tells an XBT invoice or offer from a BTC one. Paying across the two chains
// loses the money, so every send calls one of these first. "Supports" (either bit) counts as
// marked, so an invoice carrying only the odd bit 513 is still XBT; a BTC node refuses either.

/// Refuses a BOLT 11 invoice for the other chain: an XBT node (`xbt`) needs `option_blake2b`, a BTC
/// node refuses it. An invoice without a features field is a BTC invoice.
pub(crate) fn check_chain_bolt11(inv: &Bolt11Invoice, xbt: bool) -> Result<(), Error> {
	let marked = inv.features().map_or(false, |f| f.supports_blake2b());
	if marked == xbt {
		Ok(())
	} else {
		Err(Error::WrongChain)
	}
}

/// As [`check_chain_bolt11`], for a BOLT 12 offer.
pub(crate) fn check_chain_offer(offer: &Offer, xbt: bool) -> Result<(), Error> {
	if offer.offer_features().supports_blake2b() == xbt {
		Ok(())
	} else {
		Err(Error::WrongChain)
	}
}

#[cfg(test)]
mod xbt_chain_tests {
	use super::*;

	use bitcoin::hashes::{sha256, Hash};
	use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
	use lightning::offers::offer::OfferBuilder;
	use lightning_invoice::{Currency, InvoiceBuilder, RawTaggedField, TaggedField};
	use lightning_types::payment::PaymentSecret;

	fn key() -> SecretKey {
		SecretKey::from_slice(&[0x42; 32]).unwrap()
	}

	/// An amountless invoice; `blake2b` sets bit 512, or only the odd bit 513 when `odd_only`.
	fn invoice(blake2b: bool, odd_only: bool) -> Bolt11Invoice {
		let secp = Secp256k1::new();
		let mut b = InvoiceBuilder::new(Currency::Bitcoin)
			.description("x".into())
			.payment_hash(sha256::Hash::from_slice(&[1; 32]).unwrap())
			.payment_secret(PaymentSecret([2; 32]))
			.duration_since_epoch(std::time::Duration::from_secs(1_700_000_000))
			.min_final_cltv_expiry_delta(18);
		if blake2b && !odd_only {
			b = b.require_blake2b();
		}
		let mut raw = b.build_raw().unwrap();
		for f in raw.data.tagged_fields.iter_mut() {
			if let RawTaggedField::KnownSemantics(TaggedField::Features(feat)) = f {
				if odd_only {
					feat.set_blake2b_optional();
				}
			}
		}
		let signed = raw.sign::<_, ()>(|h| Ok(secp.sign_ecdsa_recoverable(h, &key()))).unwrap();
		Bolt11Invoice::from_signed(signed).unwrap()
	}

	/// The same invoice with field 9 removed, as `Bolt11Invoice::from_signed` sees it.
	fn without_feature_field() -> Result<Bolt11Invoice, lightning_invoice::Bolt11SemanticError> {
		let secp = Secp256k1::new();
		let mut raw = invoice(false, false).into_signed_raw().raw_invoice().clone();
		raw.data.tagged_fields.retain(|f| {
			!matches!(f, RawTaggedField::KnownSemantics(TaggedField::Features(_)))
		});
		let signed = raw.sign::<_, ()>(|h| Ok(secp.sign_ecdsa_recoverable(h, &key()))).unwrap();
		Bolt11Invoice::from_signed(signed)
	}

	fn btc_offer() -> Offer {
		let pk = PublicKey::from_secret_key(&Secp256k1::new(), &key());
		OfferBuilder::new(pk).description("x".into()).build().unwrap()
	}

	/// An offer with `option_blake2b` (bit 512): TLVs description(10), features(12), signing
	/// pubkey(22), spelled out because the builder's setter is crate-private.
	fn xbt_offer(bit: u8) -> Offer {
		let pk = PublicKey::from_secret_key(&Secp256k1::new(), &key());
		let mut tlv = vec![10, 1, b'x', 12, 65, bit];
		tlv.extend_from_slice(&[0u8; 64]);
		tlv.extend_from_slice(&[22, 33]);
		tlv.extend_from_slice(&pk.serialize());
		Offer::try_from(tlv).unwrap()
	}

	#[test]
	fn xbt_refuses_btc_invoice() {
		assert_eq!(check_chain_bolt11(&invoice(false, false), true), Err(Error::WrongChain));
		assert_eq!(check_chain_bolt11(&invoice(true, false), true), Ok(()));
	}

	#[test]
	fn btc_refuses_xbt_invoice() {
		assert_eq!(check_chain_bolt11(&invoice(true, false), false), Err(Error::WrongChain));
		assert_eq!(check_chain_bolt11(&invoice(false, false), false), Ok(()));
	}

	#[test]
	fn xbt_refuses_invoice_without_feature_field() {
		// An invoice with a payment secret but no field 9 cannot be built as a `Bolt11Invoice`
		// (`from_signed` requires the features), so it never reaches a send.
		assert_eq!(
			without_feature_field().unwrap_err(),
			lightning_invoice::Bolt11SemanticError::InvalidFeatures
		);
		// The smallest invoice that can exist: amountless, `var_onion` and `payment_secret` only.
		let inv = invoice(false, false);
		assert!(inv.amount_milli_satoshis().is_none());
		assert!(!inv.features().unwrap().supports_blake2b());
		assert_eq!(check_chain_bolt11(&inv, true), Err(Error::WrongChain));
		assert_eq!(check_chain_bolt11(&inv, false), Ok(()));
	}

	#[test]
	fn odd_bit_alone_counts_as_xbt() {
		let inv = invoice(true, true);
		assert_eq!(check_chain_bolt11(&inv, true), Ok(()));
		assert_eq!(check_chain_bolt11(&inv, false), Err(Error::WrongChain));
	}

	#[test]
	fn xbt_refuses_btc_offer() {
		assert_eq!(check_chain_offer(&btc_offer(), true), Err(Error::WrongChain));
		assert_eq!(check_chain_offer(&btc_offer(), false), Ok(()));
	}

	#[test]
	fn btc_refuses_xbt_offer() {
		for bit in [1u8, 2u8] {
			let o = xbt_offer(bit);
			assert_eq!(check_chain_offer(&o, true), Ok(()));
			assert_eq!(check_chain_offer(&o, false), Err(Error::WrongChain));
		}
	}
}
