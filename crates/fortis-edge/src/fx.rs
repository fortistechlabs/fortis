//! Exchange rates for showing a balance in currencies other than USD. The
//! coin prices stay USD (see `price`); this turns them into [`CODES`].
//!
//! Two feeds:
//! - **live** (`--fx-live-url`): Coinbase's public `exchange-rates?currency=USD`
//!   (updated every minute, no key), fetched every [`LIVE_EVERY`]. Used for a
//!   currency whenever it has a good answer.
//! - **ECB** (`--fx-url`): the European Central Bank's daily reference rates
//!   (`eurofxref-daily.xml`, quoted per 1 EUR, working days ~16:00 CET). Used
//!   for a currency only when the live feed fails for it: unreachable or junk
//!   ([`parse_coinbase`]), its last good answer older than [`LIVE_MAX_AGE`],
//!   the currency missing, or its rate more than [`MAX_DEVIATION`] away from
//!   the ECB's (a broken or tampered feed, not a market move).
//!
//! A rate is only ever left out, never guessed: with neither feed good for a
//! currency, `/v1/prices` leaves it out (USD alone when both are down), and a
//! wallet set to it shows no value.

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

/// The currencies `/v1/prices` offers besides USD — the wallets' list
/// (Android `data/Fiat.kt`, web `src/fiat.js`), which is the ECB's.
pub const CODES: [&str; 29] = [
    "EUR", "GBP", "CHF", "JPY", "CAD", "AUD", "NZD", "SEK", "NOK", "DKK", "ISK", "PLN", "CZK", "HUF",
    "RON", "TRY", "BRL", "MXN", "CNY", "HKD", "SGD", "KRW", "INR", "IDR", "MYR", "PHP", "THB", "ILS",
    "ZAR",
];

/// How often the live feed is asked.
pub const LIVE_EVERY: Duration = Duration::from_secs(600);
/// A live answer older than this (the feed has been failing) isn't used.
pub const LIVE_MAX_AGE: Duration = Duration::from_secs(3600);
/// A live rate further than this from the ECB's isn't used for that currency.
pub const MAX_DEVIATION: f64 = 0.10;

/// The live feed's rates (units per 1 USD, [`CODES`] only) and when they came.
#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    pub at: u64,
    pub per_usd: BTreeMap<String, f64>,
}

/// Coinbase `GET /v2/exchange-rates?currency=USD` →
/// `{"data":{"currency":"USD","rates":{"EUR":"0.889…",…}}}`. Keeps [`CODES`]
/// with a positive finite rate; anything else in it is ignored. Not USD-based,
/// or no usable rate at all, is an error.
pub fn parse_coinbase(body: &str) -> Result<BTreeMap<String, f64>> {
    let v: Value = serde_json::from_str(body)?;
    let data = v.get("data").ok_or_else(|| anyhow!("no data"))?;
    if data.get("currency").and_then(Value::as_str) != Some("USD") {
        bail!("rates aren't per USD");
    }
    let rates = data.get("rates").and_then(Value::as_object).ok_or_else(|| anyhow!("no rates"))?;
    let mut out = BTreeMap::new();
    for code in CODES {
        let r = match rates.get(code) {
            Some(Value::String(s)) => s.parse::<f64>().ok(),
            Some(Value::Number(n)) => n.as_f64(),
            _ => None,
        };
        if let Some(r) = r.filter(|r| r.is_finite() && *r > 0.0) {
            out.insert(code.to_string(), r);
        }
    }
    if out.is_empty() {
        bail!("no usable rate");
    }
    Ok(out)
}

/// Units per 1 USD for each of [`CODES`] that has a good rate: the live one
/// when it's fresh and (if the ECB has the currency) within [`MAX_DEVIATION`]
/// of the ECB's, else the ECB's when that's fresh, else none.
pub fn combine(live: Option<&Live>, ecb: Option<&Rates>, now_secs: u64) -> BTreeMap<String, f64> {
    let live = live.filter(|l| now_secs.saturating_sub(l.at) <= LIVE_MAX_AGE.as_secs());
    let ecb = ecb.filter(|r| fresh(r, (now_secs / 86_400) as i64));
    let mut out = BTreeMap::new();
    for code in CODES {
        let anchor = ecb.and_then(|r| r.per_usd.get(code)).copied();
        let l = live.and_then(|l| l.per_usd.get(code)).copied();
        let pick = match (l, anchor) {
            (Some(l), Some(a)) if (l / a - 1.0).abs() <= MAX_DEVIATION => Some(l),
            (Some(l), None) => Some(l),
            (_, a) => a,
        };
        if let Some(r) = pick {
            out.insert(code.to_string(), r);
        }
    }
    out
}

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

/// The `/v1/prices` body: `USD` as before, plus every currency in `per_usd`
/// (see [`combine`]), rounded to 4 decimals, plenty for a per-coin price.
pub fn prices_body(usd: f64, per_usd: &BTreeMap<String, f64>) -> Value {
    let mut m = Map::new();
    m.insert("USD".into(), json!(usd));
    for (code, r) in per_usd {
        let v = (usd * r * 1e4).round() / 1e4;
        if v.is_finite() && v > 0.0 {
            m.insert(code.clone(), json!(v));
        }
    }
    Value::Object(m)
}

/// Keeps both feeds refreshed on background threads, like
/// `price::PriceCache`: `current()` never touches the network.
pub struct FxCache {
    live: Arc<Mutex<Option<Live>>>,
    ecb: Arc<Mutex<Option<Rates>>>,
}

fn agent() -> ureq::Agent {
    let mut b = ureq::AgentBuilder::new().timeout(Duration::from_secs(15));
    if let Ok(tls) = native_tls::TlsConnector::new() {
        b = b.tls_connector(Arc::new(tls));
    }
    b.build()
}

/// GET `url` on a loop: `every` after a good answer, `retry` after a bad one
/// (which keeps the last good value; its age decides whether it's used).
fn poll<T: Send + 'static>(
    url: Option<&str>,
    every: Duration,
    retry: Duration,
    parse: fn(&str) -> Result<T>,
) -> Arc<Mutex<Option<T>>> {
    let slot = Arc::new(Mutex::new(None));
    let Some(url) = url.map(str::to_string) else { return slot };
    let bg = slot.clone();
    let agent = agent();
    std::thread::spawn(move || loop {
        let got = agent
            .get(&url)
            .call()
            .map_err(anyhow::Error::from)
            .and_then(|r| Ok(r.into_string()?))
            .and_then(|body| parse(&body));
        let sleep_for = match got {
            Ok(v) => {
                *bg.lock().unwrap() = Some(v);
                every
            }
            Err(e) => {
                eprintln!("fx: {url}: {e:#}");
                retry
            }
        };
        std::thread::sleep(sleep_for);
    });
    slot
}

fn parse_live(body: &str) -> Result<Live> {
    Ok(Live { at: now_secs(), per_usd: parse_coinbase(body)? })
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl FxCache {
    /// `live_url`: Coinbase exchange rates (every [`LIVE_EVERY`], retry after
    /// a minute); `ecb_url`: the ECB daily XML (hourly, retry after 5 min).
    pub fn spawn(live_url: Option<&str>, ecb_url: Option<&str>) -> Self {
        Self {
            live: poll(live_url, LIVE_EVERY, Duration::from_secs(60), parse_live),
            ecb: poll(ecb_url, Duration::from_secs(3600), Duration::from_secs(300), parse_ecb),
        }
    }

    /// Units per 1 USD for every currency that has a good rate now.
    pub fn current(&self) -> BTreeMap<String, f64> {
        let live = self.live.lock().unwrap().clone();
        let ecb = self.ecb.lock().unwrap().clone();
        combine(live.as_ref(), ecb.as_ref(), now_secs())
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
        let v = prices_body(655.0, &r.per_usd);
        assert_eq!(v["USD"], json!(655.0));
        assert_eq!(v["EUR"], json!(584.404)); // 655 / 1.1208 = 584.40397…
        assert_eq!(v["JPY"], json!(103474.5717));
        assert_eq!(v.as_object().unwrap().len(), 5);
    }

    #[test]
    fn no_rates_means_usd_alone() {
        assert_eq!(prices_body(655.0, &BTreeMap::new()), json!({ "USD": 655.0 }));
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

    const CB: &str = r#"{"data":{"currency":"USD","rates":{"EUR":"0.8893646143476433","GBP":"0.7500443333333333","JPY":"158.0481666666666667","CHF":"0.82","BTC":"0.0000123","XAU":"0.0003","AED":"3.6725"}}}"#;

    fn live(at: u64) -> Live {
        Live { at, per_usd: parse_coinbase(CB).unwrap() }
    }
    fn ecb() -> Rates {
        parse_ecb(ECB).unwrap()
    }
    /// noon on the ECB file's reference day
    fn noon() -> u64 {
        ecb().day as u64 * 86_400 + 43_200
    }

    #[test]
    fn reads_coinbase_and_keeps_only_offered_currencies() {
        let r = parse_coinbase(CB).unwrap();
        assert_eq!(r.keys().collect::<Vec<_>>(), ["CHF", "EUR", "GBP", "JPY"]); // no BTC, XAU, AED
        assert!(close(r["EUR"], 0.8893646143476433));
        assert!(close(r["JPY"], 158.048_166_666_666_67));
    }

    #[test]
    fn a_broken_coinbase_answer_is_a_failure() {
        assert!(parse_coinbase(&CB.replace(r#""currency":"USD""#, r#""currency":"EUR""#)).is_err());
        assert!(parse_coinbase(r#"{"data":{"currency":"USD","rates":{}}}"#).is_err());
        assert!(parse_coinbase(r#"{"data":{"currency":"USD","rates":{"EUR":"-1","GBP":"0","JPY":"NaN","CHF":"x"}}}"#).is_err());
        assert!(parse_coinbase(r#"{"errors":[{"id":"not_found"}]}"#).is_err());
        assert!(parse_coinbase("<html>rate limited</html>").is_err());
        // a bad rate is dropped, the good ones kept
        let r = parse_coinbase(r#"{"data":{"currency":"USD","rates":{"EUR":"0.89","GBP":"-5"}}}"#).unwrap();
        assert_eq!(r.keys().collect::<Vec<_>>(), ["EUR"]);
    }

    #[test]
    fn live_rates_win_when_good() {
        let c = combine(Some(&live(noon())), Some(&ecb()), noon() + 60);
        assert!(close(c["EUR"], 0.8893646143476433));
        assert!(close(c["GBP"], 0.7500443333333333));
        assert!(close(c["JPY"], 158.048_166_666_666_67));
    }

    #[test]
    fn a_currency_live_lacks_comes_from_the_ecb() {
        let mut l = live(noon());
        l.per_usd.remove("GBP");
        let c = combine(Some(&l), Some(&ecb()), noon());
        assert!(close(c["GBP"], 0.84610 / 1.1208));
        assert!(close(c["EUR"], 0.8893646143476433));
    }

    #[test]
    fn a_live_rate_far_from_the_ecb_is_not_used() {
        // CHF: live 0.82 vs ECB 0.9305/1.1208 = 0.8302 → 1.2% off, used
        let c = combine(Some(&live(noon())), Some(&ecb()), noon());
        assert!(close(c["CHF"], 0.82));
        // a tampered EUR, 20% off: the ECB's instead
        let mut l = live(noon());
        l.per_usd.insert("EUR".into(), 0.8922 * 1.2);
        let c = combine(Some(&l), Some(&ecb()), noon());
        assert!(close(c["EUR"], 1.0 / 1.1208));
        // exactly at the limit is still used; just past it isn't
        let anchor = 1.0 / 1.1208;
        l.per_usd.insert("EUR".into(), anchor * (1.0 + MAX_DEVIATION) * 0.999_999);
        assert!(!close(combine(Some(&l), Some(&ecb()), noon())["EUR"], anchor));
        l.per_usd.insert("EUR".into(), anchor * (1.0 + MAX_DEVIATION) * 1.000_001);
        assert!(close(combine(Some(&l), Some(&ecb()), noon())["EUR"], anchor));
    }

    #[test]
    fn stale_live_rates_fall_back_to_the_ecb() {
        let at = noon();
        let fresh_c = combine(Some(&live(at)), Some(&ecb()), at + LIVE_MAX_AGE.as_secs());
        assert!(close(fresh_c["EUR"], 0.8893646143476433));
        let stale_c = combine(Some(&live(at)), Some(&ecb()), at + LIVE_MAX_AGE.as_secs() + 1);
        assert!(close(stale_c["EUR"], 1.0 / 1.1208));
    }

    #[test]
    fn with_the_ecb_down_live_rates_stand_alone() {
        let c = combine(Some(&live(noon())), None, noon());
        assert_eq!(c.keys().collect::<Vec<_>>(), ["CHF", "EUR", "GBP", "JPY"]);
        // an ECB file too old to trust counts as down
        let old = noon() + (MAX_AGE_DAYS as u64 + 1) * 86_400;
        let c = combine(Some(&live(old)), Some(&ecb()), old);
        assert!(close(c["EUR"], 0.8893646143476433));
    }

    #[test]
    fn with_both_down_only_usd_is_left() {
        assert!(combine(None, None, noon()).is_empty());
        let old = noon() + (MAX_AGE_DAYS as u64 + 1) * 86_400;
        assert!(combine(Some(&live(noon())), Some(&ecb()), old).is_empty());
        assert_eq!(prices_body(655.0, &combine(None, None, noon())), json!({ "USD": 655.0 }));
    }

    #[test]
    fn only_offered_currencies_come_from_the_ecb_too() {
        let r = parse_ecb(&ECB.replace("currency='CHF'", "currency='AED'")).unwrap();
        assert!(r.per_usd.contains_key("AED"));
        assert!(!combine(None, Some(&r), noon()).contains_key("AED"));
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
