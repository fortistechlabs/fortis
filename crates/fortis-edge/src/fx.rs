//! Exchange rates for showing a balance in currencies other than USD. The
//! coin prices stay USD (see `price`); this turns them into the other
//! currencies with the European Central Bank's daily reference rates
//! (`eurofxref-daily.xml`: ~30 currencies, quoted per 1 EUR, published on
//! working days around 16:00 CET).
//!
//! A rate is only ever left out, never guessed: a feed that doesn't parse,
//! has no USD, quotes a EUR/USD outside a sane band, or is older than
//! [`MAX_AGE_DAYS`] gives no rates, and `/v1/prices` answers USD alone, as
//! it always did. A wallet set to another currency then shows no value.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Days a reference date may lag today before its rates are dropped. The ECB
/// publishes no rates at weekends or on TARGET holidays (Christmas to 1 Jan
/// is the longest gap, 4–5 days), so this only trips on a feed that stopped.
pub const MAX_AGE_DAYS: i64 = 10;

/// EUR/USD outside this band means a broken feed, not a market move.
const EURUSD_SANE: std::ops::RangeInclusive<f64> = 0.5..=2.0;

/// Units of each currency per 1 USD, as of `day` (days since 1970-01-01).
/// Includes EUR; never includes USD itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Rates {
    pub day: i64,
    pub per_usd: BTreeMap<String, f64>,
}

/// Parse the ECB daily feed into per-USD rates.
pub fn parse_ecb(xml: &str) -> Result<Rates> {
    let day = attr(xml, "time").ok_or_else(|| anyhow!("no reference date"))?;
    let day = days_from_iso(day).ok_or_else(|| anyhow!("bad reference date {day:?}"))?;

    let mut per_eur = BTreeMap::new();
    let mut rest = xml;
    while let Some(i) = rest.find("currency=") {
        rest = &rest[i..];
        let code = attr(rest, "currency").ok_or_else(|| anyhow!("unreadable currency"))?;
        let rate = attr(rest, "rate").ok_or_else(|| anyhow!("no rate for {code}"))?;
        rest = &rest["currency=".len()..];
        if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_uppercase()) {
            bail!("bad currency code {code:?}");
        }
        let rate: f64 = rate.parse().map_err(|_| anyhow!("bad rate for {code}"))?;
        if !(rate.is_finite() && rate > 0.0) {
            bail!("bad rate for {code}");
        }
        if per_eur.insert(code.to_string(), rate).is_some() {
            bail!("{code} quoted twice");
        }
    }

    let eurusd = *per_eur.get("USD").ok_or_else(|| anyhow!("no USD rate"))?;
    if !EURUSD_SANE.contains(&eurusd) {
        bail!("EUR/USD {eurusd} is outside {EURUSD_SANE:?}");
    }
    per_eur.remove("USD");
    let mut per_usd: BTreeMap<String, f64> = per_eur.into_iter().map(|(c, r)| (c, r / eurusd)).collect();
    per_usd.insert("EUR".to_string(), 1.0 / eurusd);
    Ok(Rates { day, per_usd })
}

/// The value of the first `name='…'` (or `name="…"`) attribute in `s`.
fn attr<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("{name}=");
    let start = s.find(&key)? + key.len();
    let quote = s[start..].chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let body = &s[start + 1..];
    Some(&body[..body.find(quote)?])
}

/// `YYYY-MM-DD` → days since 1970-01-01 (proleptic Gregorian).
fn days_from_iso(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let (y, m, d) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn today() -> i64 {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    (secs / 86_400) as i64
}

/// The `/v1/prices` body: `USD` as before, plus every currency `rates`
/// converts to (rounded to 4 decimals, plenty for a per-coin price).
pub fn prices_body(usd: f64, rates: Option<&Rates>) -> Value {
    let mut m = Map::new();
    m.insert("USD".into(), json!(usd));
    for (code, r) in rates.map(|r| &r.per_usd).into_iter().flatten() {
        let v = (usd * r * 1e4).round() / 1e4;
        if v.is_finite() && v > 0.0 {
            m.insert(code.clone(), json!(v));
        }
    }
    Value::Object(m)
}

/// Refreshes the rates on a background thread, like `price::PriceCache`:
/// `current()` never touches the network.
pub struct FxCache {
    current: Arc<Mutex<Option<Rates>>>,
}

impl FxCache {
    /// Refetches every `interval`; a failure retries after 5 minutes and
    /// keeps the last good rates (until they're too old to serve).
    pub fn spawn(url: &str, interval: Duration) -> Self {
        let mut b = ureq::AgentBuilder::new().timeout(Duration::from_secs(15));
        if let Ok(tls) = native_tls::TlsConnector::new() {
            b = b.tls_connector(Arc::new(tls));
        }
        let agent = b.build();
        let url = url.to_string();
        let current = Arc::new(Mutex::new(None));
        let bg = current.clone();
        std::thread::spawn(move || loop {
            let got = agent
                .get(&url)
                .call()
                .map_err(anyhow::Error::from)
                .and_then(|r| Ok(r.into_string()?))
                .and_then(|body| parse_ecb(&body));
            let sleep_for = match got {
                Ok(r) => {
                    *bg.lock().unwrap() = Some(r);
                    interval
                }
                Err(e) => {
                    eprintln!("fx: {url}: {e:#}");
                    Duration::from_secs(300)
                }
            };
            std::thread::sleep(sleep_for);
        });
        Self { current }
    }

    /// The last good rates, unless they're older than [`MAX_AGE_DAYS`].
    pub fn current(&self) -> Option<Rates> {
        let r = self.current.lock().unwrap().clone()?;
        fresh(&r, today()).then_some(r)
    }
}

fn fresh(r: &Rates, today: i64) -> bool {
    today - r.day <= MAX_AGE_DAYS
}

#[cfg(test)]
mod tests {
    use super::*;

    const ECB: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<gesmes:Envelope xmlns:gesmes="http://www.gesmes.org/xml/2002-08-01" xmlns="http://www.ecb.int/vocabulary/2002-08-01/eurofxref">
	<gesmes:subject>Reference rates</gesmes:subject>
	<Cube>
		<Cube time='2026-10-15'>
			<Cube currency='USD' rate='1.1208'/>
			<Cube currency='JPY' rate='177.06'/>
			<Cube currency='GBP' rate='0.84610'/>
			<Cube currency='CHF' rate='0.9305'/>
		</Cube>
	</Cube>
</gesmes:Envelope>"#;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9 * b.abs().max(1.0)
    }

    #[test]
    fn converts_the_ecb_feed_to_per_usd() {
        let r = parse_ecb(ECB).unwrap();
        assert_eq!(r.day, days_from_iso("2026-10-15").unwrap());
        assert_eq!(r.per_usd.keys().collect::<Vec<_>>(), ["CHF", "EUR", "GBP", "JPY"]);
        assert!(close(r.per_usd["EUR"], 1.0 / 1.1208));
        assert!(close(r.per_usd["GBP"], 0.84610 / 1.1208));
        assert!(close(r.per_usd["JPY"], 177.06 / 1.1208));
    }

    #[test]
    fn the_body_adds_currencies_to_usd() {
        let r = parse_ecb(ECB).unwrap();
        let v = prices_body(655.0, Some(&r));
        assert_eq!(v["USD"], json!(655.0));
        assert_eq!(v["EUR"], json!(584.404)); // 655 / 1.1208 = 584.40397…
        assert_eq!(v["JPY"], json!(103474.5717));
        assert_eq!(v.as_object().unwrap().len(), 5);
    }

    #[test]
    fn no_rates_means_usd_alone() {
        assert_eq!(prices_body(655.0, None), json!({ "USD": 655.0 }));
    }

    #[test]
    fn double_quotes_are_read_too() {
        let r = parse_ecb(r#"<Cube time="2026-10-15"><Cube currency="USD" rate="1.12"/><Cube currency="GBP" rate="0.85"/></Cube>"#).unwrap();
        assert!(close(r.per_usd["GBP"], 0.85 / 1.12));
    }

    #[test]
    fn a_broken_feed_gives_no_rates() {
        let no_usd = ECB.replace("currency='USD' rate='1.1208'", "currency='SEK' rate='11.17'");
        assert!(parse_ecb(&no_usd).is_err());
        assert!(parse_ecb(&ECB.replace("1.1208", "11.208")).is_err()); // EUR/USD out of band
        assert!(parse_ecb(&ECB.replace("177.06", "-1")).is_err());
        assert!(parse_ecb(&ECB.replace("177.06", "NaN")).is_err());
        assert!(parse_ecb(&ECB.replace("177.06", "inf")).is_err());
        assert!(parse_ecb(&ECB.replace("'JPY'", "'jpy'")).is_err());
        assert!(parse_ecb(&ECB.replace("'GBP'", "'JPY'")).is_err()); // quoted twice
        assert!(parse_ecb(&ECB.replace("2026-10-15", "15/10/2026")).is_err());
        assert!(parse_ecb("<html>maintenance</html>").is_err());
        assert!(parse_ecb("").is_err());
    }

    #[test]
    fn old_rates_are_not_served() {
        let r = parse_ecb(ECB).unwrap();
        assert!(fresh(&r, r.day));
        assert!(fresh(&r, r.day + MAX_AGE_DAYS));
        assert!(!fresh(&r, r.day + MAX_AGE_DAYS + 1));
    }

    #[test]
    fn civil_days() {
        assert_eq!(days_from_iso("1970-01-01"), Some(0));
        assert_eq!(days_from_iso("2000-03-01"), Some(11_017));
        assert_eq!(days_from_iso("2026-10-15"), Some(20_741));
        assert_eq!(days_from_iso("2026-13-01"), None);
        assert_eq!(days_from_iso("26-10-15"), None);
    }
}
