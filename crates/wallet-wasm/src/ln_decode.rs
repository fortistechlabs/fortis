//! Decoding BOLT11 invoices and BOLT12 offers for the Lightning tab (display and checks only).

use std::str::FromStr;

use lightning::offers::offer::{Amount as OfferAmount, Offer};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescriptionRef};
use serde::Serialize;

/// Largest text looked at (bytes), as `fortis-lightning`'s parser.
const MAX_INPUT_LEN: usize = 16 * 1024;
/// A JS number holds integers exactly up to 2^53.
const MAX_SAFE: u64 = 9_007_199_254_740_991;

#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InvoiceInfo {
    pub amount_msat: Option<u64>,
    pub description: Option<String>,
    /// Hex of the description hash (LNURL invoices carry a hash, not a text).
    pub description_hash: Option<String>,
    pub payment_hash: String,
    pub created_secs: u64,
    pub expiry_secs: u64,
    pub payee: String,
    /// `bitcoin`, `testnet`, `signet` or `regtest`.
    pub network: String,
    /// The invoice is for the XBT (BLAKE2b) chain: it carries the `option_blake2b` feature bit. An
    /// invoice for the BTC chain does not. A wallet must pay only its own chain's invoices.
    pub is_xbt: bool,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OfferInfo {
    pub amount_msat: Option<u64>,
    pub description: Option<String>,
    /// The offer is for the XBT (BLAKE2b) chain (`option_blake2b`).
    pub is_xbt: bool,
}

fn clean(text: &str) -> Result<String, String> {
    if text.len() > MAX_INPUT_LEN {
        return Err("input too long".into());
    }
    let t = text.trim();
    let t = match t.get(..10) {
        Some(h) if h.eq_ignore_ascii_case("lightning:") => t[10..].trim(),
        _ => t,
    };
    if t.is_empty() {
        return Err("empty".into());
    }
    Ok(t.to_ascii_lowercase())
}

fn safe(msat: u64) -> Result<u64, String> {
    if msat > MAX_SAFE { Err("amount too large".into()) } else { Ok(msat) }
}

pub fn decode_invoice(text: &str) -> Result<InvoiceInfo, String> {
    let inv = Bolt11Invoice::from_str(&clean(text)?).map_err(|e| e.to_string())?;
    let (description, description_hash) = match inv.description() {
        Bolt11InvoiceDescriptionRef::Direct(d) => (Some(d.to_string()), None),
        Bolt11InvoiceDescriptionRef::Hash(h) => (None, Some(hex::encode(h.0))),
    };
    let network = match inv.currency() {
        lightning_invoice::Currency::Bitcoin => "bitcoin",
        lightning_invoice::Currency::BitcoinTestnet => "testnet",
        lightning_invoice::Currency::Signet => "signet",
        lightning_invoice::Currency::Regtest | lightning_invoice::Currency::Simnet => "regtest",
    };
    Ok(InvoiceInfo {
        amount_msat: inv.amount_milli_satoshis().map(safe).transpose()?,
        description,
        description_hash,
        payment_hash: inv.payment_hash().to_string(),
        created_secs: inv.duration_since_epoch().as_secs(),
        expiry_secs: inv.expiry_time().as_secs(),
        payee: inv.recover_payee_pub_key().to_string(),
        network: network.into(),
        is_xbt: inv.features().is_some_and(|f| f.supports_blake2b()),
    })
}

pub fn decode_offer(text: &str) -> Result<OfferInfo, String> {
    let offer = Offer::from_str(&clean(text)?).map_err(|e| format!("{e:?}"))?;
    let amount_msat = match offer.amount() {
        Some(OfferAmount::Bitcoin { amount_msats }) => Some(safe(amount_msats)?),
        _ => None,
    };
    Ok(OfferInfo {
        amount_msat,
        description: offer.description().map(|d| d.to_string()),
        is_xbt: offer.offer_features().supports_blake2b(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // BOLT 11 vectors from the lightning-invoice crate's own tests (also used by fortis-lightning).
    const B11_25M: &str = "lnbc25m1pvjluezpp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdq5vdhkven9v5sxyetpdeessp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygs9q5sqqqqqqqqqqqqqqqpqsq67gye39hfg3zd8rgc80k32tvy9xk2xunwm5lzexnvpx6fd77en8qaq424dxgt56cag2dpt359k3ssyhetktkpqh24jqnjyw6uqd08sgptq44qu";
    const B11_NOAMT: &str = "lnbc1pvjluezsp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygspp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdpl2pkx2ctnv5sxxmmwwd5kgetjypeh2ursdae8g6twvus8g6rfwvs8qun0dfjkxaq9qrsgq357wnc5r2ueh7ck6q93dj32dlqnls087fxdwk8qakdyafkq3yap2r09nt4ndd0unm3z9u5t48y6ucv4r5sg7lk98c77ctvjczkspk5qprc90gx";
    const OFFER: &str = "lno1pqps7sjqpgtyzm3qv4uxzmtsd3jjqer9wd3hy6tsw35k7msjzfpy7nz5yqcnygrfdej82um5wf5k2uckyypwa3eyt44h6txtxquqh7lz5djge4afgfjn7k4rgrkuag0jsd5xvxg";

    #[test]
    fn an_invoice_with_an_amount_decodes() {
        let i = decode_invoice(B11_25M).unwrap();
        assert_eq!(i.amount_msat, Some(2_500_000_000));
        assert_eq!(i.network, "bitcoin");
        assert_eq!(i.payment_hash.len(), 64);
        assert_eq!(i.payee.len(), 66);
        assert!(i.expiry_secs > 0 && i.created_secs > 0);
        assert_eq!(i.description_hash, None);
        assert!(i.description.is_some());
    }

    #[test]
    fn an_amountless_invoice_has_no_amount_and_upper_case_and_the_lightning_prefix_are_accepted() {
        assert_eq!(decode_invoice(B11_NOAMT).unwrap().amount_msat, None);
        let up = format!("  Lightning:{}  ", B11_25M.to_uppercase());
        assert_eq!(decode_invoice(&up).unwrap().amount_msat, Some(2_500_000_000));
    }

    #[test]
    fn junk_is_an_error_not_a_panic() {
        for s in ["", "   ", "lightning:", "lnbc", "lnbc1xyz", "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq", "\u{0}\u{ffff}"] {
            assert!(decode_invoice(s).is_err(), "{s:?}");
        }
        assert!(decode_invoice(&"a".repeat(20_000)).is_err());
        assert!(decode_offer("lno1junk").is_err());
        assert!(decode_offer("").is_err());
    }

    #[test]
    fn an_xbt_invoice_is_told_from_a_btc_one() {
        use wallet_core::bitcoin::hashes::{sha256, Hash};
        use wallet_core::bitcoin::secp256k1::{Secp256k1, SecretKey};
        use lightning::types::payment::PaymentSecret;
        use lightning_invoice::{Currency, InvoiceBuilder};
        let secp = Secp256k1::new();
        let key = SecretKey::from_slice(&[0x42; 32]).unwrap();
        let xbt = InvoiceBuilder::new(Currency::Bitcoin)
            .description("x".to_string())
            .payment_hash(sha256::Hash::hash(b"h"))
            .payment_secret(PaymentSecret([2; 32]))
            .current_timestamp()
            .min_final_cltv_expiry_delta(144)
            .amount_milli_satoshis(5_000)
            .require_blake2b()
            .build_signed(|m| secp.sign_ecdsa_recoverable(m, &key))
            .unwrap()
            .to_string();
        assert!(decode_invoice(&xbt).unwrap().is_xbt);
        assert!(!decode_invoice(B11_25M).unwrap().is_xbt);
    }

    #[test]
    fn an_offer_decodes() {
        let o = decode_offer(OFFER).unwrap();
        assert!(o.description.is_some());
        assert!(!o.is_xbt);
    }
}
