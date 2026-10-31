//! LNURL-pay (LUD-06) and Lightning Address (LUD-16) resolution.
//!
//! All network access goes through [`HttpGet`] so it can be faked in tests.
//! Only `https://` URLs are fetched (`.onion` hosts may use `http://`); this
//! applies to the initial URL and to the server-supplied callback.

use std::io::Read;
use std::str::FromStr;
use std::time::Duration;

use ldk_node::bitcoin::hashes::{sha256, Hash};
use ldk_node::lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescriptionRef};
use serde_json::Value;
use url::{Host, Url};
use std::net::IpAddr;

use crate::error::LnError;
use crate::parse::PaymentTarget;

/// Maximum accepted response body size.
pub(crate) const MAX_BODY: usize = 64 * 1024;

pub trait HttpGet {
    fn get_json(&self, url: &str) -> Result<Value, LnError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LnurlPayParams {
    pub callback: String,
    pub min_msat: u64,
    pub max_msat: u64,
    /// The `text/plain` entry of `metadata`.
    pub description: String,
    /// The raw `metadata` string as served: the invoice from the callback
    /// must commit to its SHA-256 (LUD-06 description hash).
    pub metadata: String,
}

fn other(s: impl Into<String>) -> LnError {
    LnError::Other(s.into())
}

fn check_scheme(url: &str) -> Result<(), LnError> {
    if url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return Err(other("lnurl url has invalid characters"));
    }
    let u = Url::parse(url).map_err(|_| other("lnurl url invalid"))?;
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    if host.is_empty() {
        return Err(other("lnurl url has no host"));
    }
    match u.scheme() {
        "https" => {}
        "http" if host.ends_with(".onion") => return Ok(()),
        _ => return Err(other("lnurl url must be https")),
    }
    // Never reach into the user's own network (or the phone itself): an LNURL
    // comes from a QR code or a pasted string, so its host is not trusted.
    // `Url` has already normalised odd IPv4 spellings (0x7f.1, 2130706433).
    let public = match u.host() {
        Some(Host::Ipv4(ip)) => is_public_ip(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => is_public_ip(IpAddr::V6(ip)),
        Some(Host::Domain(d)) => is_public_domain(d),
        None => false,
    };
    if public {
        Ok(())
    } else {
        Err(other("lnurl host is not public"))
    }
}

/// A name that cannot be a public host: single-label names and the reserved
/// local-use suffixes.
fn is_public_domain(d: &str) -> bool {
    let d = d.trim_end_matches('.').to_ascii_lowercase();
    d.contains('.')
        && !["localhost", "local", "localdomain", "internal", "lan", "home.arpa", "intranet"]
            .iter()
            .any(|s| d == *s || d.ends_with(&format!(".{s}")))
}

/// Whether `ip` is a globally routable unicast address.
pub(crate) fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || o[0] == 0
                || o[0] >= 240
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64/10 CGNAT
                || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0/24
                || (o[0] == 198 && (o[1] & 0xfe) == 18)) // 198.18/15
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || s[..6] == [0, 0, 0, 0, 0, 0] // ::/96 (unspecified, loopback, IPv4-compatible)
                || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (s[0] & 0xffc0) == 0xfe80 // fe80::/10 link local
                || (s[0] == 0x2001 && s[1] == 0x0db8) // 2001:db8::/32 documentation
                || (s[0] == 0x2001 && s[1] == 0x0000) // 2001::/32 Teredo
                || s[0] == 0x2002 // 2002::/16 6to4
                || (s[0] == 0x0064 && s[1] == 0xff9b)) // 64:ff9b::/96 & 64:ff9b:1::/48 NAT64
        }
    }
}

/// DNS for [`UreqGet`]: a name that resolves to a non-public address is
/// refused (a public name pointing at the LAN).
struct PublicResolver;

impl ureq::Resolver for PublicResolver {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        let addrs: Vec<_> = std::net::ToSocketAddrs::to_socket_addrs(netloc)?.collect();
        if addrs.is_empty() || addrs.iter().any(|a| !is_public_ip(a.ip())) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "host resolves to a non-public address",
            ));
        }
        Ok(addrs)
    }
}

fn address_url(user: &str, domain: &str) -> Result<String, LnError> {
    let d = domain.to_ascii_lowercase();
    let labels_ok = d
        .split('.')
        .all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    if user.is_empty() || !labels_ok {
        return Err(other("invalid lightning address"));
    }
    let scheme = if d.ends_with(".onion") {
        "http"
    } else {
        "https"
    };
    let mut u =
        Url::parse(&format!("{scheme}://{d}/")).map_err(|_| other("invalid lightning address"))?;
    u.path_segments_mut()
        .map_err(|_| other("invalid lightning address"))?
        .clear()
        .extend([".well-known", "lnurlp", user]);
    Ok(u.into())
}

fn fetch(http: &dyn HttpGet, url: &str) -> Result<Value, LnError> {
    check_scheme(url)?;
    let v = http.get_json(url)?;
    if v.get("status").and_then(Value::as_str) == Some("ERROR") {
        let reason = v
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("lnurl error");
        return Err(other(reason));
    }
    Ok(v)
}

pub fn resolve(target: &PaymentTarget, http: &dyn HttpGet) -> Result<LnurlPayParams, LnError> {
    let url = match target {
        PaymentTarget::LnurlPay { url } => url.clone(),
        PaymentTarget::LightningAddress { user, domain } => address_url(user, domain)?,
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
        metadata: metadata.to_string(),
    })
}

/// Fetches the invoice for `amount_msat` from the callback. The invoice must
/// be for exactly that amount and carry a description hash equal to
/// sha256(`params.metadata`) (LUD-06): a plain description, or another hash,
/// is `Other("lnurl_description_hash")`.
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
    let sep = if params.callback.contains('?') {
        '&'
    } else {
        '?'
    };
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
    let expect = sha256::Hash::hash(params.metadata.as_bytes());
    match inv.description() {
        Bolt11InvoiceDescriptionRef::Hash(h) if h.0 == expect => Ok(pr),
        _ => Err(other("lnurl_description_hash")),
    }
}

/// Reads at most [`MAX_BODY`] bytes; errors if the body is larger.
pub(crate) fn read_capped(r: impl Read, cap: usize) -> Result<Vec<u8>, LnError> {
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
            .resolver(PublicResolver)
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
pub(crate) mod tests {
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

    const META: &str = "[[\"text/plain\",\"coffee\"]]";

    fn params(cb: &str) -> LnurlPayParams {
        LnurlPayParams {
            callback: cb.into(),
            min_msat: 1000,
            max_msat: 3_000_000_000,
            description: "coffee".into(),
            metadata: META.into(),
        }
    }

    /// A signed regtest invoice for `amount_msat` committing to sha256(`metadata`)
    /// (or, with `plain`, carrying that text as a plain description).
    pub(crate) fn signed_invoice(amount_msat: u64, metadata: &str, plain: bool) -> String {
        use ldk_node::bitcoin::hashes::{sha256, Hash};
        use ldk_node::bitcoin::secp256k1::{Secp256k1, SecretKey};
        use ldk_node::lightning::types::payment::PaymentSecret;
        use ldk_node::lightning_invoice::{Currency, InvoiceBuilder};
        let secp = Secp256k1::new();
        let key = SecretKey::from_slice(&[0x42; 32]).unwrap();
        let b = InvoiceBuilder::new(Currency::Regtest);
        let b = if plain {
            b.description(metadata.into())
        } else {
            b.description_hash(sha256::Hash::hash(metadata.as_bytes()))
        };
        b.payment_hash(sha256::Hash::hash(&amount_msat.to_be_bytes()))
            .payment_secret(PaymentSecret([2; 32]))
            .current_timestamp()
            .min_final_cltv_expiry_delta(144)
            .amount_milli_satoshis(amount_msat)
            .build_signed(|m| secp.sign_ecdsa_recoverable(m, &key))
            .unwrap()
            .to_string()
    }

    fn pay_request(cb: &str) -> Value {
        serde_json::json!({
            "tag": "payRequest", "callback": cb,
            "minSendable": 1000, "maxSendable": 5000000,
            "metadata": "[[\"text/identifier\",\"name@example.com\"],[\"text/plain\",\"coffee\"]]"
        })
    }

    #[test]
    fn private_and_local_hosts_are_refused() {
        for bad in [
            "https://127.0.0.1/p",
            "https://localhost/p",
            "https://printer.local/p",
            "https://nas/p",
            "https://10.1.2.3/p",
            "https://192.168.0.5/p",
            "https://172.16.0.1/p",
            "https://169.254.169.254/latest",
            "https://100.64.0.1/p",
            "https://0x7f.1/p",
            "https://2130706433/p",
            "https://[::1]/p",
            "https://[fd00::1]/p",
            "https://[fe80::1]/p",
            "https://[::ffff:127.0.0.1]/p",
            "https://[::127.0.0.1]/p",
            "https://[2002:7f00:1::1]/p",
            "https://[2001:0000:4136:e378:8000:63bf:3fff:fdd2]/p",
            "https://[64:ff9b:1::10.0.0.1]/p",
            "https://0.0.0.0/p",
        ] {
            assert!(check_scheme(bad).is_err(), "{bad} must be refused");
        }
        for good in [
            "https://example.com/p",
            "https://8.8.8.8/p",
            "https://[2606:4700::1111]/p",
            "http://abcdefgh.onion/p",
        ] {
            assert!(check_scheme(good).is_ok(), "{good} must pass");
        }
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
        f.map
            .insert("https://e.com/p".into(), pay_request("https://e.com/cb"));
        let t = PaymentTarget::LnurlPay {
            url: "https://e.com/p".into(),
        };
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
        let a = PaymentTarget::LnurlPay {
            url: "https://e.com/a".into(),
        };
        let b = PaymentTarget::LnurlPay {
            url: "https://e.com/b".into(),
        };
        assert!(matches!(resolve(&a, &f), Err(LnError::Other(_))));
        assert_eq!(resolve(&b, &f), Err(LnError::Other("nope".into())));
    }

    #[test]
    fn amount_below_min_errors_amount_out_of_range() {
        let f = Fake::default();
        let e = request_invoice(&params("https://e.com/cb"), 999, &f).unwrap_err();
        assert_eq!(
            e,
            LnError::AmountOutOfRange {
                min_msat: 1000,
                max_msat: 3_000_000_000
            }
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
        let inv = signed_invoice(2_500_000_000, META, false);
        f.map.insert(
            "https://e.com/cb?k=v&amount=2500000000".into(),
            serde_json::json!({ "pr": inv }),
        );
        let pr = request_invoice(&params("https://e.com/cb?k=v"), 2_500_000_000, &f);
        assert_eq!(pr.unwrap(), inv);
    }

    fn invoice_with(inv: String) -> Result<String, LnError> {
        let mut f = Fake::default();
        f.map.insert(
            "https://e.com/cb?amount=5000".into(),
            serde_json::json!({ "pr": inv }),
        );
        request_invoice(&params("https://e.com/cb"), 5000, &f)
    }

    #[test]
    fn description_hash_matching_metadata_ok() {
        let inv = signed_invoice(5000, META, false);
        assert_eq!(invoice_with(inv.clone()), Ok(inv));
    }

    #[test]
    fn description_hash_mismatch_rejected() {
        let inv = signed_invoice(5000, "[[\"text/plain\",\"tea\"]]", false);
        assert_eq!(
            invoice_with(inv),
            Err(LnError::Other("lnurl_description_hash".into()))
        );
    }

    #[test]
    fn plain_description_instead_of_hash_rejected() {
        // Even when the plain text is the metadata itself.
        let inv = signed_invoice(5000, META, true);
        assert_eq!(
            invoice_with(inv),
            Err(LnError::Other("lnurl_description_hash".into()))
        );
    }

    #[test]
    fn metadata_kept_raw() {
        let mut f = Fake::default();
        let req = pay_request("https://e.com/cb");
        let raw = req["metadata"].as_str().unwrap().to_string();
        f.map.insert("https://e.com/p".into(), req);
        let t = PaymentTarget::LnurlPay {
            url: "https://e.com/p".into(),
        };
        assert_eq!(resolve(&t, &f).unwrap().metadata, raw);
    }

    #[test]
    fn http_only_rejected() {
        let f = Fake::default();
        let t = PaymentTarget::LnurlPay {
            url: "http://e.com/p".into(),
        };
        assert!(matches!(resolve(&t, &f), Err(LnError::Other(_))));
        assert!(f.seen.borrow().is_empty());
        // hostile http callback
        let mut f = Fake::default();
        f.map
            .insert("https://e.com/p".into(), pay_request("http://e.com/cb"));
        let t = PaymentTarget::LnurlPay {
            url: "https://e.com/p".into(),
        };
        assert!(matches!(resolve(&t, &f), Err(LnError::Other(_))));
        // callback check also enforced at request time
        let e = request_invoice(&params("http://e.com/cb"), 5000, &f);
        assert!(matches!(e, Err(LnError::Other(_))));
        assert!(!f.seen.borrow().iter().any(|u| u.contains("cb")));
    }

    #[test]
    fn onion_may_use_http() {
        let mut f = Fake::default();
        f.map.insert(
            "http://abc.onion/p".into(),
            pay_request("http://abc.onion/cb"),
        );
        let t = PaymentTarget::LnurlPay {
            url: "http://abc.onion/p".into(),
        };
        assert!(resolve(&t, &f).is_ok());
        assert!(check_scheme("http://evil.com/x.onion").is_err());
        assert!(check_scheme("http://evil.com@abc.onion.evil.com/").is_err());
    }

    #[test]
    fn scheme_bypasses_rejected() {
        for bad in [
            "http://evil.com\\@abc.onion/p",
            "http://evil.com\\.onion/p",
            "http://evil.com/x.onion",
            "https://",
            "http:///p",
            "http://",
            "https://a b.com/",
            "https://a.com/\n",
        ] {
            assert!(check_scheme(bad).is_err(), "{bad}");
            // as first URL
            let f = Fake::default();
            let t = PaymentTarget::LnurlPay { url: bad.into() };
            assert!(resolve(&t, &f).is_err(), "{bad}");
            assert!(f.seen.borrow().is_empty(), "{bad}");
            // as callback
            let mut f = Fake::default();
            f.map.insert("https://e.com/p".into(), pay_request(bad));
            let t = PaymentTarget::LnurlPay {
                url: "https://e.com/p".into(),
            };
            assert!(resolve(&t, &f).is_err(), "{bad}");
            assert!(request_invoice(&params(bad), 5000, &f).is_err(), "{bad}");
        }
        // userinfo trick: host is evil.com, over https: allowed; with http it is not
        assert!(check_scheme("https://good.com@evil.com/").is_ok());
        assert!(check_scheme("http://good.onion@evil.com/").is_err());
        assert!(check_scheme("http://evil.com@good.onion/").is_ok());
    }

    #[test]
    fn address_url_validated_and_escaped() {
        for bad in [
            "a\\b.com", "a@b.com", "a.com?x", "a.com#x", "a.com/x", "a b.com", "a..com", "",
        ] {
            let f = Fake::default();
            let t = PaymentTarget::LightningAddress {
                user: "u".into(),
                domain: bad.into(),
            };
            assert!(resolve(&t, &f).is_err(), "{bad:?}");
            assert!(f.seen.borrow().is_empty(), "{bad:?}");
        }
        let f = Fake::default();
        let t = PaymentTarget::LightningAddress {
            user: String::new(),
            domain: "a.com".into(),
        };
        assert!(resolve(&t, &f).is_err());
        assert!(f.seen.borrow().is_empty());

        let mut f = Fake::default();
        f.map.insert(
            "https://a.com/.well-known/lnurlp/x%2Fy%3Fz%23w".into(),
            pay_request("https://a.com/cb"),
        );
        let t = PaymentTarget::LightningAddress {
            user: "x/y?z#w".into(),
            domain: "a.com".into(),
        };
        resolve(&t, &f).unwrap();
        assert_eq!(
            f.seen.borrow().as_slice(),
            ["https://a.com/.well-known/lnurlp/x%2Fy%3Fz%23w"]
        );
    }

    #[test]
    fn body_cap() {
        assert_eq!(read_capped(&[1u8; 10][..], 10).unwrap().len(), 10);
        assert!(read_capped(&[1u8; 11][..], 10).is_err());
    }
}
