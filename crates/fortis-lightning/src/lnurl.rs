//! LNURL-pay (LUD-06) and Lightning Address (LUD-16) resolution.
//!
//! All network access goes through [`HttpGet`] so it can be faked in tests.
//! Only `https://` URLs are fetched (`.onion` hosts may use `http://`); this
//! applies to the initial URL and to the server-supplied callback.

use std::io::Read;
use std::str::FromStr;
use std::time::Duration;

use ldk_node::lightning_invoice::Bolt11Invoice;
use serde_json::Value;

use crate::error::LnError;
use crate::parse::PaymentTarget;

/// Maximum accepted response body size.
const MAX_BODY: usize = 64 * 1024;

pub trait HttpGet {
    fn get_json(&self, url: &str) -> Result<Value, LnError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LnurlPayParams {
    pub callback: String,
    pub min_msat: u64,
    pub max_msat: u64,
    pub description: String,
}

fn other(s: impl Into<String>) -> LnError {
    LnError::Other(s.into())
}

fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    let auth = auth.rsplit('@').next().unwrap_or(auth);
    auth.split(':').next().unwrap_or(auth)
}

fn check_scheme(url: &str) -> Result<(), LnError> {
    let l = url.to_ascii_lowercase();
    if l.starts_with("https://") {
        return Ok(());
    }
    if l.starts_with("http://") && host_of(&l).ends_with(".onion") {
        return Ok(());
    }
    Err(other("lnurl url must be https"))
}

fn fetch(http: &dyn HttpGet, url: &str) -> Result<Value, LnError> {
    check_scheme(url)?;
    let v = http.get_json(url)?;
    if v.get("status").and_then(Value::as_str) == Some("ERROR") {
        let reason = v.get("reason").and_then(Value::as_str).unwrap_or("lnurl error");
        return Err(other(reason));
    }
    Ok(v)
}

pub fn resolve(target: &PaymentTarget, http: &dyn HttpGet) -> Result<LnurlPayParams, LnError> {
    let url = match target {
        PaymentTarget::LnurlPay { url } => url.clone(),
        PaymentTarget::LightningAddress { user, domain } => {
            let scheme = if domain.to_ascii_lowercase().ends_with(".onion") {
                "http"
            } else {
                "https"
            };
            format!("{scheme}://{domain}/.well-known/lnurlp/{user}")
        }
        _ => return Err(other("not an lnurl target")),
    };
    let v = fetch(http, &url)?;
    if v.get("tag").and_then(Value::as_str) != Some("payRequest") {
        return Err(other("lnurl tag is not payRequest"));
    }
    let callback = v
        .get("callback")
        .and_then(Value::as_str)
        .ok_or_else(|| other("lnurl missing callback"))?
        .to_string();
    check_scheme(&callback)?;
    let min_msat = v
        .get("minSendable")
        .and_then(Value::as_u64)
        .ok_or_else(|| other("lnurl missing minSendable"))?;
    let max_msat = v
        .get("maxSendable")
        .and_then(Value::as_u64)
        .ok_or_else(|| other("lnurl missing maxSendable"))?;
    if min_msat > max_msat {
        return Err(other("lnurl min exceeds max"));
    }
    let metadata = v
        .get("metadata")
        .and_then(Value::as_str)
        .ok_or_else(|| other("lnurl missing metadata"))?;
    let entries: Vec<Vec<Value>> =
        serde_json::from_str(metadata).map_err(|_| other("lnurl bad metadata"))?;
    let description = entries
        .iter()
        .find(|e| e.first().and_then(Value::as_str) == Some("text/plain"))
        .and_then(|e| e.get(1))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(LnurlPayParams {
        callback,
        min_msat,
        max_msat,
        description,
    })
}

pub fn request_invoice(
    params: &LnurlPayParams,
    amount_msat: u64,
    http: &dyn HttpGet,
) -> Result<String, LnError> {
    if amount_msat < params.min_msat || amount_msat > params.max_msat {
        return Err(LnError::AmountOutOfRange {
            min_msat: params.min_msat,
            max_msat: params.max_msat,
        });
    }
    let sep = if params.callback.contains('?') { '&' } else { '?' };
    let url = format!("{}{sep}amount={amount_msat}", params.callback);
    let v = fetch(http, &url)?;
    let pr = v
        .get("pr")
        .and_then(Value::as_str)
        .ok_or_else(|| other("lnurl missing invoice"))?
        .trim()
        .to_string();
    let inv = Bolt11Invoice::from_str(&pr.to_ascii_lowercase())
        .map_err(|e| other(format!("lnurl bad invoice: {e}")))?;
    if inv.amount_milli_satoshis() != Some(amount_msat) {
        return Err(other("lnurl invoice amount mismatch"));
    }
    Ok(pr)
}

/// Reads at most [`MAX_BODY`] bytes; errors if the body is larger.
fn read_capped(r: impl Read, cap: usize) -> Result<Vec<u8>, LnError> {
    let mut buf = Vec::new();
    r.take(cap as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| other(format!("http read: {e}")))?;
    if buf.len() > cap {
        return Err(other("http response too large"));
    }
    Ok(buf)
}

/// Production blocking HTTP client (rustls, 10 s timeout, capped body).
pub struct UreqGet {
    agent: ureq::Agent,
}

impl UreqGet {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(10))
            .redirects(0)
            .build();
        Self { agent }
    }
}

impl Default for UreqGet {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpGet for UreqGet {
    fn get_json(&self, url: &str) -> Result<Value, LnError> {
        let resp = self
            .agent
            .get(url)
            .set("Accept", "application/json")
            .call()
            .map_err(|e| other(format!("http: {e}")))?;
        let body = read_capped(resp.into_reader(), MAX_BODY)?;
        serde_json::from_slice(&body).map_err(|_| other("http: invalid json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake {
        map: HashMap<String, Value>,
        seen: RefCell<Vec<String>>,
    }
    impl HttpGet for Fake {
        fn get_json(&self, url: &str) -> Result<Value, LnError> {
            self.seen.borrow_mut().push(url.to_string());
            self.map.get(url).cloned().ok_or_else(|| other("404"))
        }
    }

    // 0.025 BTC (2_500_000_000 msat) mainnet invoice (BOLT11 spec example).
    const INV_25M: &str = "lnbc25m1pvjluezpp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdq5vdhkven9v5sxyetpdeessp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygs9q5sqqqqqqqqqqqqqqqpqsq67gye39hfg3zd8rgc80k32tvy9xk2xunwm5lzexnvpx6fd77en8qaq424dxgt56cag2dpt359k3ssyhetktkpqh24jqnjyw6uqd08sgptq44qu";

    fn params(cb: &str) -> LnurlPayParams {
        LnurlPayParams {
            callback: cb.into(),
            min_msat: 1000,
            max_msat: 3_000_000_000,
            description: String::new(),
        }
    }

    fn pay_request(cb: &str) -> Value {
        serde_json::json!({
            "tag": "payRequest", "callback": cb,
            "minSendable": 1000, "maxSendable": 5000000,
            "metadata": "[[\"text/identifier\",\"name@example.com\"],[\"text/plain\",\"coffee\"]]"
        })
    }

    #[test]
    fn address_resolves_to_well_known_url() {
        let mut f = Fake::default();
        f.map.insert(
            "https://example.com/.well-known/lnurlp/name".into(),
            pay_request("https://example.com/cb"),
        );
        let t = PaymentTarget::LightningAddress {
            user: "name".into(),
            domain: "example.com".into(),
        };
        resolve(&t, &f).unwrap();
        assert_eq!(
            f.seen.borrow().as_slice(),
            ["https://example.com/.well-known/lnurlp/name"]
        );
    }

    #[test]
    fn params_parsed() {
        let mut f = Fake::default();
        f.map.insert("https://e.com/p".into(), pay_request("https://e.com/cb"));
        let t = PaymentTarget::LnurlPay { url: "https://e.com/p".into() };
        let p = resolve(&t, &f).unwrap();
        assert_eq!(p.callback, "https://e.com/cb");
        assert_eq!((p.min_msat, p.max_msat), (1000, 5_000_000));
        assert_eq!(p.description, "coffee");
    }

    #[test]
    fn wrong_tag_and_error_status_rejected() {
        let mut f = Fake::default();
        let mut v = pay_request("https://e.com/cb");
        v["tag"] = "withdrawRequest".into();
        f.map.insert("https://e.com/a".into(), v);
        f.map.insert(
            "https://e.com/b".into(),
            serde_json::json!({"status":"ERROR","reason":"nope"}),
        );
        let a = PaymentTarget::LnurlPay { url: "https://e.com/a".into() };
        let b = PaymentTarget::LnurlPay { url: "https://e.com/b".into() };
        assert!(matches!(resolve(&a, &f), Err(LnError::Other(_))));
        assert_eq!(resolve(&b, &f), Err(LnError::Other("nope".into())));
    }

    #[test]
    fn amount_below_min_errors_amount_out_of_range() {
        let f = Fake::default();
        let e = request_invoice(&params("https://e.com/cb"), 999, &f).unwrap_err();
        assert_eq!(
            e,
            LnError::AmountOutOfRange { min_msat: 1000, max_msat: 3_000_000_000 }
        );
        assert!(f.seen.borrow().is_empty());
    }

    #[test]
    fn callback_invoice_amount_must_match() {
        let mut f = Fake::default();
        f.map.insert(
            "https://e.com/cb?amount=5000000".into(),
            serde_json::json!({ "pr": INV_25M }),
        );
        let e = request_invoice(&params("https://e.com/cb"), 5_000_000, &f).unwrap_err();
        assert!(matches!(e, LnError::Other(_)));
    }

    #[test]
    fn callback_invoice_matching_amount_ok_and_existing_query() {
        let mut f = Fake::default();
        f.map.insert(
            "https://e.com/cb?k=v&amount=2500000000".into(),
            serde_json::json!({ "pr": INV_25M }),
        );
        let pr = request_invoice(&params("https://e.com/cb?k=v"), 2_500_000_000, &f);
        assert_eq!(pr.unwrap(), INV_25M);
    }

    #[test]
    fn http_only_rejected() {
        let f = Fake::default();
        let t = PaymentTarget::LnurlPay { url: "http://e.com/p".into() };
        assert!(matches!(resolve(&t, &f), Err(LnError::Other(_))));
        assert!(f.seen.borrow().is_empty());
        // hostile http callback
        let mut f = Fake::default();
        f.map.insert("https://e.com/p".into(), pay_request("http://e.com/cb"));
        let t = PaymentTarget::LnurlPay { url: "https://e.com/p".into() };
        assert!(matches!(resolve(&t, &f), Err(LnError::Other(_))));
        // callback check also enforced at request time
        let e = request_invoice(&params("http://e.com/cb"), 5000, &f);
        assert!(matches!(e, Err(LnError::Other(_))));
        assert!(!f.seen.borrow().iter().any(|u| u.contains("cb")));
    }

    #[test]
    fn onion_may_use_http() {
        let mut f = Fake::default();
        f.map.insert("http://abc.onion/p".into(), pay_request("http://abc.onion/cb"));
        let t = PaymentTarget::LnurlPay { url: "http://abc.onion/p".into() };
        assert!(resolve(&t, &f).is_ok());
        assert!(check_scheme("http://evil.com/x.onion").is_err());
        assert!(check_scheme("http://evil.com@abc.onion.evil.com/").is_err());
    }

    #[test]
    fn body_cap() {
        assert_eq!(read_capped(&[1u8; 10][..], 10).unwrap().len(), 10);
        assert!(read_capped(&[1u8; 11][..], 10).is_err());
    }
}
