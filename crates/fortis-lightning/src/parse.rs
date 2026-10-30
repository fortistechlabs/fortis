//! Payment parser (pure, no I/O).

use crate::error::ParseError;
use std::str::FromStr;
use std::time::Duration;

use ldk_node::bitcoin::bech32;
use ldk_node::bitcoin::constants::ChainHash;
use ldk_node::bitcoin::{Address, Network};
use ldk_node::lightning::offers::offer::{Amount as OfferAmount, Offer};
use ldk_node::lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescriptionRef};

/// 21 million BTC in satoshis.
const MAX_SAT: u64 = 21_000_000 * 100_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaymentTarget {
    Bolt11 {
        invoice: String,
        amount_msat: Option<u64>,
        description: String,
        expiry_secs: u64,
    },
    Bolt12Offer {
        offer: String,
        amount_msat: Option<u64>,
        description: String,
    },
    LnurlPay {
        url: String,
    },
    LightningAddress {
        user: String,
        domain: String,
    },
    OnChain {
        address: String,
        amount_sat: Option<u64>,
    },
    Unified {
        address: String,
        amount_sat: Option<u64>,
        lightning: Box<PaymentTarget>,
    },
}

pub fn parse(text: &str, network: Network) -> Result<PaymentTarget, ParseError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    parse_at(text, network, now)
}

/// Same as [`parse`] with an explicit clock (unix seconds) for the expiry
/// check, so callers and tests are deterministic.
pub fn parse_at(text: &str, network: Network, now_secs: u64) -> Result<PaymentTarget, ParseError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ParseError::Empty);
    }
    let text = strip_prefix_ci(text, "lightning:").unwrap_or(text).trim();
    if text.is_empty() {
        return Err(ParseError::Empty);
    }

    if let Some(rest) = strip_prefix_ci(text, "bitcoin:") {
        return parse_bip21(rest, network, now_secs);
    }
    if let Some(rest) = strip_prefix_ci(text, "lnurlp://") {
        return lnurlp_scheme(rest);
    }

    let lower = text.to_ascii_lowercase();
    if lower.starts_with("lnurl1") {
        return parse_lnurl(&lower);
    }
    if lower.starts_with("lno1") {
        return parse_offer(&lower, network);
    }
    if lower.starts_with("ln") && lower.contains('1') && !lower.contains('@') {
        return parse_bolt11(&lower, network, now_secs);
    }
    if text.contains('@') {
        return parse_lightning_address(text);
    }
    match Address::from_str(text) {
        Ok(unchecked) => {
            let addr = unchecked
                .require_network(network)
                .map_err(|_| ParseError::WrongNetwork)?;
            Ok(PaymentTarget::OnChain {
                address: addr.to_string(),
                amount_sat: None,
            })
        }
        Err(_) => Err(ParseError::Unrecognised),
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

fn parse_bolt11(lower: &str, network: Network, now_secs: u64) -> Result<PaymentTarget, ParseError> {
    let inv = Bolt11Invoice::from_str(lower).map_err(|e| ParseError::Invalid(e.to_string()))?;
    if inv.network() != network {
        return Err(ParseError::WrongNetwork);
    }
    if inv.would_expire(Duration::from_secs(now_secs)) {
        return Err(ParseError::Expired);
    }
    let description = match inv.description() {
        Bolt11InvoiceDescriptionRef::Direct(d) => d.to_string(),
        Bolt11InvoiceDescriptionRef::Hash(_) => String::new(),
    };
    Ok(PaymentTarget::Bolt11 {
        invoice: lower.to_string(),
        amount_msat: inv.amount_milli_satoshis(),
        description,
        expiry_secs: inv.expiry_time().as_secs(),
    })
}

fn parse_offer(lower: &str, network: Network) -> Result<PaymentTarget, ParseError> {
    let offer = Offer::from_str(lower).map_err(|e| ParseError::Invalid(format!("{e:?}")))?;
    if !offer.supports_chain(ChainHash::using_genesis_block(network)) {
        return Err(ParseError::WrongNetwork);
    }
    let amount_msat = match offer.amount() {
        Some(OfferAmount::Bitcoin { amount_msats }) => Some(amount_msats),
        _ => None,
    };
    Ok(PaymentTarget::Bolt12Offer {
        offer: lower.to_string(),
        amount_msat,
        description: offer
            .description()
            .map(|d| d.to_string())
            .unwrap_or_default(),
    })
}

fn parse_lnurl(lower: &str) -> Result<PaymentTarget, ParseError> {
    let (hrp, data) = bech32::decode(lower).map_err(|e| ParseError::Invalid(e.to_string()))?;
    if hrp.as_str() != "lnurl" {
        return Err(ParseError::Invalid("bad lnurl prefix".into()));
    }
    let url =
        String::from_utf8(data).map_err(|_| ParseError::Invalid("lnurl is not utf-8".into()))?;
    let url = url.trim().to_string();
    let l = url.to_ascii_lowercase();
    if !(l.starts_with("https://") || l.starts_with("http://")) {
        return Err(ParseError::Invalid("lnurl is not http(s)".into()));
    }
    Ok(PaymentTarget::LnurlPay { url })
}

/// `lnurlp://host/path` -> `https://host/path` (`http://` for `.onion`).
fn lnurlp_scheme(rest: &str) -> Result<PaymentTarget, ParseError> {
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() {
        return Err(ParseError::Invalid("lnurlp url has no host".into()));
    }
    let scheme = if host.to_ascii_lowercase().ends_with(".onion") {
        "http"
    } else {
        "https"
    };
    Ok(PaymentTarget::LnurlPay {
        url: format!("{scheme}://{rest}"),
    })
}

fn parse_lightning_address(text: &str) -> Result<PaymentTarget, ParseError> {
    let Some((user, domain)) = text.split_once('@') else {
        return Err(ParseError::Unrecognised);
    };
    let (user, domain) = (user.to_ascii_lowercase(), domain.to_ascii_lowercase());
    let user_ok = !user.is_empty()
        && user
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.+".contains(&b));
    let labels_ok = domain.split('.').all(|l| {
        !l.is_empty()
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    });
    if !user_ok || !labels_ok || !domain.contains('.') {
        return Err(ParseError::Unrecognised);
    }
    Ok(PaymentTarget::LightningAddress { user, domain })
}

fn parse_bip21(rest: &str, network: Network, now_secs: u64) -> Result<PaymentTarget, ParseError> {
    let rest = rest.trim_start_matches("//");
    let (addr_part, query) = rest.split_once('?').unwrap_or((rest, ""));

    let mut amount_sat = None;
    let mut lightning = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match percent_decode(k)?.to_ascii_lowercase().as_str() {
            "amount" => amount_sat = Some(btc_to_sat(&percent_decode(v)?)?),
            "lightning" => lightning = Some(percent_decode(v)?),
            "lno" if lightning.is_none() => lightning = Some(percent_decode(v)?),
            _ => {}
        }
    }
    let lightning = lightning
        .and_then(|l| parse_at(&l, network, now_secs).ok())
        .filter(|t| {
            matches!(
                t,
                PaymentTarget::Bolt11 { .. } | PaymentTarget::Bolt12Offer { .. }
            )
        });

    if addr_part.is_empty() {
        // `bitcoin:?lightning=...` carries no on-chain fallback.
        return lightning.ok_or(ParseError::Unrecognised);
    }
    let addr = Address::from_str(addr_part)
        .map_err(|e| ParseError::Invalid(e.to_string()))?
        .require_network(network)
        .map_err(|_| ParseError::WrongNetwork)?
        .to_string();
    Ok(match lightning {
        Some(l) => PaymentTarget::Unified {
            address: addr,
            amount_sat,
            lightning: Box::new(l),
        },
        None => PaymentTarget::OnChain {
            address: addr,
            amount_sat,
        },
    })
}

/// Decimal BTC -> satoshis with exact integer math (at most 8 decimals).
fn btc_to_sat(s: &str) -> Result<u64, ParseError> {
    let bad = || ParseError::Invalid("bad amount".into());
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    let digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if (whole.is_empty() && frac.is_empty()) || !digits(whole) || !digits(frac) || frac.len() > 8 {
        return Err(bad());
    }
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| bad())?
    };
    let frac: u64 = format!("{frac:0<8}").parse().map_err(|_| bad())?;
    let sat = whole
        .checked_mul(100_000_000)
        .and_then(|w| w.checked_add(frac))
        .ok_or_else(bad)?;
    if sat > MAX_SAT {
        return Err(bad());
    }
    Ok(sat)
}

fn percent_decode(s: &str) -> Result<String, ParseError> {
    let bad = || ParseError::Invalid("bad percent-encoding".into());
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3).ok_or_else(bad)?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| bad())?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ldk_node::bitcoin::bech32::{self, Bech32, Hrp};
    use ldk_node::lightning_invoice::Bolt11Invoice;
    use std::str::FromStr;

    // BOLT 11 vectors taken from the lightning-invoice crate's own tests.
    const B11_100P: &str = "lnbc100p1psj9jhxdqud3jxktt5w46x7unfv9kz6mn0v3jsnp4q0d3p2sfluzdx45tqcsh2pu5qc7lgq0xs578ngs6s0s68ua4h7cvspp5q6rmq35js88zp5dvwrv9m459tnk2zunwj5jalqtyxqulh0l5gflssp5nf55ny5gcrfl30xuhzj3nphgj27rstekmr9fw3ny5989s300gyus9qyysgqcqpcrzjqw2sxwe993h5pcm4dxzpvttgza8zhkqxpgffcrf5v25nwpr3cmfg7z54kuqq8rgqqqqqqqq2qqqqq9qq9qrzjqd0ylaqclj9424x9m8h2vcukcgnm6s56xfgu3j78zyqzhgs4hlpzvznlugqq9vsqqqqqqqlgqqqqqeqq9qrzjqwldmj9dha74df76zhx6l9we0vjdquygcdt3kssupehe64g6yyp5yz5rhuqqwccqqyqqqqlgqqqqjcqq9qrzjqf9e58aguqr0rcun0ajlvmzq3ek63cw2w282gv3z5uupmuwvgjtq2z55qsqqg6qqqyqqqrtnqqqzq3cqygrzjqvphmsywntrrhqjcraumvc4y6r8v4z5v593trte429v4hredj7ms5z52usqq9ngqqqqqqqlgqqqqqqgq9qrzjq2v0vp62g49p7569ev48cmulecsxe59lvaw3wlxm7r982zxa9zzj7z5l0cqqxusqqyqqqqlgqqqqqzsqygarl9fh38s0gyuxjjgux34w75dnc6xp2l35j7es3jd4ugt3lu0xzre26yg5m7ke54n2d5sym4xcmxtl8238xxvw5h5h5j5r6drg6k6zcqj0fcwg";
    const B11_25M: &str = "lnbc25m1pvjluezpp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdq5vdhkven9v5sxyetpdeessp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygs9q5sqqqqqqqqqqqqqqqpqsq67gye39hfg3zd8rgc80k32tvy9xk2xunwm5lzexnvpx6fd77en8qaq424dxgt56cag2dpt359k3ssyhetktkpqh24jqnjyw6uqd08sgptq44qu";
    const B11_NOAMT: &str = "lnbc1pvjluezsp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygspp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdpl2pkx2ctnv5sxxmmwwd5kgetjypeh2ursdae8g6twvus8g6rfwvs8qun0dfjkxaq9qrsgq357wnc5r2ueh7ck6q93dj32dlqnls087fxdwk8qakdyafkq3yap2r09nt4ndd0unm3z9u5t48y6ucv4r5sg7lk98c77ctvjczkspk5qprc90gx";
    // lightning-0.2.7 offers test vector.
    const OFFER: &str = "lno1pqps7sjqpgtyzm3qv4uxzmtsd3jjqer9wd3hy6tsw35k7msjzfpy7nz5yqcnygrfdej82um5wf5k2uckyypwa3eyt44h6txtxquqh7lz5djge4afgfjn7k4rgrkuag0jsd5xvxg";
    const ADDR: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
    const BASE58: &str = "1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2";

    /// A time 10 seconds after the invoice was created, so it is unexpired.
    fn just_after(inv: &str) -> u64 {
        Bolt11Invoice::from_str(inv)
            .unwrap()
            .duration_since_epoch()
            .as_secs()
            + 10
    }

    fn b11(inv: &str) -> PaymentTarget {
        parse_at(inv, Network::Bitcoin, just_after(inv)).unwrap()
    }

    #[test]
    fn bolt11_plain() {
        match b11(B11_25M) {
            PaymentTarget::Bolt11 {
                invoice,
                amount_msat,
                description,
                expiry_secs,
            } => {
                assert_eq!(invoice, B11_25M);
                assert_eq!(amount_msat, Some(2_500_000_000));
                assert_eq!(description, "coffee beans");
                assert_eq!(expiry_secs, 3600);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bolt11_uppercase_and_prefix() {
        let now = just_after(B11_25M);
        let lower = parse_at(B11_25M, Network::Bitcoin, now).unwrap();
        let upper = format!("LIGHTNING:{}", B11_25M.to_uppercase());
        assert_eq!(parse_at(&upper, Network::Bitcoin, now).unwrap(), lower);
        let mixed = format!("  Lightning:{B11_25M}\n");
        assert_eq!(parse_at(&mixed, Network::Bitcoin, now).unwrap(), lower);
    }

    #[test]
    fn bolt11_no_amount_is_none() {
        match b11(B11_NOAMT) {
            PaymentTarget::Bolt11 { amount_msat, .. } => assert_eq!(amount_msat, None),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn subsat_amount_msat_kept() {
        // 100p = 0.0000000001 BTC = 10 msat.
        match b11(B11_100P) {
            PaymentTarget::Bolt11 {
                amount_msat,
                description,
                ..
            } => {
                assert_eq!(amount_msat, Some(10));
                assert!(!description.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn wrong_network_rejected() {
        let now = just_after(B11_25M);
        for n in [Network::Testnet, Network::Regtest, Network::Signet] {
            assert_eq!(parse_at(B11_25M, n, now), Err(ParseError::WrongNetwork));
        }
    }

    #[test]
    fn expired_rejected() {
        // Spec invoices are from 2017/2021 and long expired; `parse` uses the system clock.
        assert_eq!(parse(B11_25M, Network::Bitcoin), Err(ParseError::Expired));
        let late = Bolt11Invoice::from_str(B11_25M)
            .unwrap()
            .duration_since_epoch()
            .as_secs()
            + 3601;
        assert_eq!(
            parse_at(B11_25M, Network::Bitcoin, late),
            Err(ParseError::Expired)
        );
    }

    #[test]
    fn bad_checksum_is_invalid() {
        let mut s = B11_25M.to_string();
        s.pop();
        s.push('q');
        assert!(matches!(
            parse_at(&s, Network::Bitcoin, 0),
            Err(ParseError::Invalid(_))
        ));
    }

    #[test]
    fn bolt12_offer() {
        let t = parse(OFFER, Network::Bitcoin).unwrap();
        assert!(matches!(&t, PaymentTarget::Bolt12Offer { offer, .. } if offer == OFFER));
        assert_eq!(
            parse(
                &format!("lightning:{}", OFFER.to_uppercase()),
                Network::Bitcoin
            )
            .unwrap(),
            t
        );
    }

    fn lnurl(url: &str) -> String {
        bech32::encode::<Bech32>(Hrp::parse("lnurl").unwrap(), url.as_bytes()).unwrap()
    }

    #[test]
    fn lnurl_bech32_and_lnurlp_scheme() {
        let url = "https://example.com/.well-known/lnurlp/alice";
        let want = PaymentTarget::LnurlPay { url: url.into() };
        let enc = lnurl(url);
        assert_eq!(parse(&enc, Network::Bitcoin).unwrap(), want);
        assert_eq!(
            parse(
                &format!("lightning:{}", enc.to_uppercase()),
                Network::Bitcoin
            )
            .unwrap(),
            want
        );
        assert_eq!(
            parse(
                "lnurlp://example.com/.well-known/lnurlp/alice",
                Network::Bitcoin
            )
            .unwrap(),
            want
        );
        assert_eq!(
            parse("lnurlp://abc.onion/pay", Network::Bitcoin).unwrap(),
            PaymentTarget::LnurlPay {
                url: "http://abc.onion/pay".into()
            }
        );
    }

    #[test]
    fn lnurl_non_http_rejected() {
        let enc = lnurl("ftp://example.com/x");
        assert!(matches!(
            parse(&enc, Network::Bitcoin),
            Err(ParseError::Invalid(_))
        ));
    }

    #[test]
    fn lightning_address() {
        let want = PaymentTarget::LightningAddress {
            user: "name".into(),
            domain: "example.com".into(),
        };
        assert_eq!(parse("name@example.com", Network::Bitcoin).unwrap(), want);
        assert_eq!(
            parse("  lightning:Name@Example.com \r\n", Network::Bitcoin).unwrap(),
            want
        );
        assert_eq!(
            parse("not an@address", Network::Bitcoin),
            Err(ParseError::Unrecognised)
        );
        assert_eq!(
            parse("a@nodot", Network::Bitcoin),
            Err(ParseError::Unrecognised)
        );
    }

    #[test]
    fn bip21_onchain_only() {
        let t = parse(
            &format!("bitcoin:{ADDR}?amount=0.00012345&label=x%20y"),
            Network::Bitcoin,
        )
        .unwrap();
        assert_eq!(
            t,
            PaymentTarget::OnChain {
                address: ADDR.into(),
                amount_sat: Some(12_345)
            }
        );
        let t = parse(
            &format!("bitcoin:{ADDR}?amount=20999999.97690000"),
            Network::Bitcoin,
        )
        .unwrap();
        assert_eq!(
            t,
            PaymentTarget::OnChain {
                address: ADDR.into(),
                amount_sat: Some(2_099_999_997_690_000)
            }
        );
        let t = parse(&format!("bitcoin:{ADDR}?amount=1"), Network::Bitcoin).unwrap();
        assert_eq!(
            t,
            PaymentTarget::OnChain {
                address: ADDR.into(),
                amount_sat: Some(100_000_000)
            }
        );
        let t = parse(&format!("bitcoin:{ADDR}"), Network::Bitcoin).unwrap();
        assert_eq!(
            t,
            PaymentTarget::OnChain {
                address: ADDR.into(),
                amount_sat: None
            }
        );
        for bad in ["0.123456789", "-1", "1e-3", "abc", ".", "", "21000001"] {
            assert!(
                matches!(
                    parse(&format!("bitcoin:{ADDR}?amount={bad}"), Network::Bitcoin),
                    Err(ParseError::Invalid(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn bip21_with_lightning_param() {
        let now = just_after(B11_25M);
        let uri = format!("bitcoin:{ADDR}?amount=0.025&lightning={B11_25M}");
        let t = parse_at(&uri, Network::Bitcoin, now).unwrap();
        match t {
            PaymentTarget::Unified {
                address,
                amount_sat,
                lightning,
            } => {
                assert_eq!(address, ADDR);
                assert_eq!(amount_sat, Some(2_500_000));
                assert_eq!(
                    *lightning,
                    parse_at(B11_25M, Network::Bitcoin, now).unwrap()
                );
            }
            other => panic!("{other:?}"),
        }
        // An unusable lightning param (expired here) falls back to on-chain only.
        let t = parse(&uri, Network::Bitcoin).unwrap();
        assert!(matches!(
            t,
            PaymentTarget::OnChain {
                amount_sat: Some(2_500_000),
                ..
            }
        ));
    }

    #[test]
    fn bip21_uppercase_scheme() {
        let up = format!("BITCOIN:{}?AMOUNT=0.5", ADDR.to_uppercase());
        let t = parse(&up, Network::Bitcoin).unwrap();
        assert!(matches!(
            t,
            PaymentTarget::OnChain {
                amount_sat: Some(50_000_000),
                ..
            }
        ));
    }

    #[test]
    fn plain_address() {
        assert_eq!(
            parse(&format!(" {ADDR}\n"), Network::Bitcoin).unwrap(),
            PaymentTarget::OnChain {
                address: ADDR.into(),
                amount_sat: None
            }
        );
        assert_eq!(
            parse(BASE58, Network::Bitcoin).unwrap(),
            PaymentTarget::OnChain {
                address: BASE58.into(),
                amount_sat: None
            }
        );
        assert_eq!(parse(ADDR, Network::Testnet), Err(ParseError::WrongNetwork));
    }

    #[test]
    fn garbage_and_empty() {
        assert_eq!(parse("", Network::Bitcoin), Err(ParseError::Empty));
        assert_eq!(parse(" \n\t", Network::Bitcoin), Err(ParseError::Empty));
        assert_eq!(
            parse("lightning:", Network::Bitcoin),
            Err(ParseError::Empty)
        );
        assert_eq!(
            parse("hello world", Network::Bitcoin),
            Err(ParseError::Unrecognised)
        );
        assert_eq!(
            parse("https://example.com", Network::Bitcoin),
            Err(ParseError::Unrecognised)
        );
    }
}
