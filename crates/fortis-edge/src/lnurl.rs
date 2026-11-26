//! LUD-06 / LUD-16 responses for Lightning addresses: the pay request a wallet fetches first, the error
//! shape wallets show, and the metadata whose hash every invoice must carry.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::names::Chain;

/// The metadata string for `name`: LUD-16 asks for `text/identifier`, LUD-06 for `text/plain`. It is
/// returned to the payer as is, and its sha256 is the description hash of the invoice, so the same
/// function must make both.
pub fn metadata(name: &str, chain: Chain, domain: &str) -> String {
    json!([
        ["text/identifier", format!("{name}@{domain}")],
        ["text/plain", format!("Pay {name} on Fortis ({} Lightning)", chain.label())],
    ])
    .to_string()
}

/// sha256 of `metadata`, hex.
pub fn description_hash_hex(metadata: &str) -> String {
    hex::encode(Sha256::digest(metadata.as_bytes()))
}

/// The first response (`GET /.well-known/lnurlp/<name>`).
pub fn pay_request(name: &str, chain: Chain, domain: &str, min_msat: u64, max_msat: u64) -> Value {
    json!({
        "tag": "payRequest",
        "callback": format!("https://{domain}/lnurlp/callback/{name}"),
        "minSendable": min_msat,
        "maxSendable": max_msat,
        "metadata": metadata(name, chain, domain),
        "commentAllowed": 0,
    })
}

/// LUD-06's error shape (answered with HTTP 200, so wallets show the reason).
pub fn error(reason: &str) -> Value {
    json!({ "status": "ERROR", "reason": reason })
}

/// The second response: the invoice.
pub fn invoice_response(pr: &str) -> Value {
    json!({ "pr": pr, "routes": [] })
}

/// `amount=<msat>` from a query string: digits only, more than 0.
pub fn parse_amount(query: &str) -> Option<u64> {
    let v = query.split('&').find_map(|kv| kv.strip_prefix("amount="))?;
    if v.is_empty() || v.len() > 18 || !v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    v.parse().ok().filter(|a| *a > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pay_request_has_the_shape_lud06_and_lud16_ask_for() {
        let v = pay_request("donal", Chain::Xbt, "fortis.cash", 1_000, 50_000_000);
        assert_eq!(v["tag"], "payRequest");
        assert_eq!(v["callback"], "https://fortis.cash/lnurlp/callback/donal");
        assert_eq!(v["minSendable"], 1_000);
        assert_eq!(v["maxSendable"], 50_000_000);
        assert_eq!(v["commentAllowed"], 0);
        // `metadata` is a string holding a JSON array with text/plain and the LUD-16 identifier.
        let m: Value = serde_json::from_str(v["metadata"].as_str().unwrap()).unwrap();
        assert_eq!(m[0], json!(["text/identifier", "donal@fortis.cash"]));
        assert_eq!(m[1], json!(["text/plain", "Pay donal on Fortis (XBT Lightning)"]));
    }

    #[test]
    fn the_description_hash_is_the_sha256_of_the_exact_metadata_string() {
        let v = pay_request("donal", Chain::Btc, "fortis.cash", 1_000, 2_000);
        let shown = v["metadata"].as_str().unwrap();
        assert_eq!(description_hash_hex(shown), description_hash_hex(&metadata("donal", Chain::Btc, "fortis.cash")));
        // sha256("abc") — the function is plain sha256, hex.
        assert_eq!(description_hash_hex("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        // The chain is in the metadata, so the two chains' hashes differ.
        assert_ne!(description_hash_hex(&metadata("donal", Chain::Btc, "fortis.cash")), description_hash_hex(&metadata("donal", Chain::Xbt, "fortis.cash")));
    }

    #[test]
    fn errors_and_invoices_have_the_published_shape() {
        assert_eq!(error("not found"), json!({ "status": "ERROR", "reason": "not found" }));
        assert_eq!(invoice_response("lnbc1"), json!({ "pr": "lnbc1", "routes": [] }));
    }

    #[test]
    fn the_amount_is_digits_only_and_above_zero() {
        assert_eq!(parse_amount("amount=1000"), Some(1_000));
        assert_eq!(parse_amount("x=1&amount=42"), Some(42));
        for bad in ["", "amount=", "amount=0", "amount=-5", "amount=1.5", "amount=1e3", "amount=0x10", "amount= 5", "amount=1234567890123456789", "amountx=5"] {
            assert_eq!(parse_amount(bad), None, "{bad}");
        }
    }
}
