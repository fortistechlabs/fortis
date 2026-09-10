//! Fetch a spot USD price from an external source and normalise it to
//! `{ "USD": <number> }` — the shape the wallet reads. Handles a couple of
//! common response layouts so the source can be a mempool instance or an
//! exchange ticker without the client caring.

use anyhow::{anyhow, Result};
use serde_json::Value;

pub struct PriceSource {
    url: String,
    agent: ureq::Agent,
}

impl PriceSource {
    pub fn new(url: &str) -> Self {
        let mut b = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(15));
        if let Ok(tls) = native_tls::TlsConnector::new() {
            b = b.tls_connector(std::sync::Arc::new(tls));
        }
        Self { url: url.to_string(), agent: b.build() }
    }

    /// GET the source and pull out a positive USD price.
    pub fn fetch_usd(&self) -> Result<f64> {
        let body = self.agent.get(&self.url).call()?.into_string()?;
        let v: Value = serde_json::from_str(&body)?;
        extract_usd(&v).ok_or_else(|| anyhow!("no usable USD price in the response"))
    }
}

/// Known shapes:
/// - mempool.space / a mempool instance: `{ "USD": 79465, "EUR": … }`
/// - Kraken `Ticker`: `{ "error": [], "result": { "<PAIR>": { "c": ["79465.8", …] } } }`
///   (`c` = last trade closed: `[price, lot volume]`)
fn extract_usd(v: &Value) -> Option<f64> {
    let positive = |n: f64| (n.is_finite() && n > 0.0).then_some(n);

    // mempool-style
    if let Some(n) = v.get("USD").and_then(Value::as_f64).and_then(positive) {
        return Some(n);
    }

    // exchange ticker with a Kraken-style envelope
    if v.get("error").and_then(Value::as_array).is_some_and(|e| !e.is_empty()) {
        return None;
    }
    if let Some(result) = v.get("result").and_then(Value::as_object) {
        for data in result.values() {
            let last = data
                .get("c")
                .and_then(|c| c.get(0))
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<f64>().ok())
                .and_then(positive);
            if let Some(n) = last {
                return Some(n);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::extract_usd;
    use serde_json::json;

    #[test]
    fn reads_the_mempool_shape() {
        let v = json!({ "time": 1, "USD": 79465, "EUR": 68000 });
        assert_eq!(extract_usd(&v), Some(79465.0));
    }

    #[test]
    fn reads_the_kraken_shape() {
        let v = json!({
            "error": [],
            "result": { "XXBTZUSD": {
                "a": ["79466.0", "1", "1.0"],
                "c": ["79465.80000", "0.00012075"],
                "o": "80334.4"
            }}
        });
        assert_eq!(extract_usd(&v), Some(79465.8));
    }

    #[test]
    fn kraken_error_is_not_a_price() {
        let v = json!({ "error": ["EQuery:Unknown asset pair"], "result": {} });
        assert_eq!(extract_usd(&v), None);
    }

    #[test]
    fn junk_and_non_positive_values_are_rejected() {
        assert_eq!(extract_usd(&json!({ "USD": -1 })), None);
        assert_eq!(extract_usd(&json!({ "USD": 0 })), None);
        assert_eq!(extract_usd(&json!({ "nope": 1 })), None);
        assert_eq!(extract_usd(&json!({ "result": { "P": { "c": ["oops"] } } })), None);
    }
}
