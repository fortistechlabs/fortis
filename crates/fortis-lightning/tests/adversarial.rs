//! Task 19, step 2: adversarial-input property tests (proptest).
//!
//! Every test states an invariant that must hold for ANY input from an
//! untrusted source (a QR code, a link, a pasted string, an LNURL or Esplora
//! server): no panic, no hang (each case is timed), bounded memory (inputs are
//! size-capped by the strategies), and — where there is one — the safety
//! property the code is meant to guarantee. Seeded with the vectors the unit
//! tests already use. These need no network and no binaries: they run with
//! plain `cargo test -p fortis-lightning --test adversarial`.

use std::cell::RefCell;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fortis_lightning::ldk_node::bitcoin::{Address, Network, Transaction, Txid};
use fortis_lightning::lnurl::{self, HttpGet, LnurlPayParams};
use fortis_lightning::recover::{
    scan_static_outputs, static_scripts, sweep_static_outputs, Broadcaster, EsploraScan,
    HttpClient, HttpResponse, ScriptUtxoSource, StaticUtxo, UtxoRef, DUST_LIMIT_SAT,
};
use fortis_lightning::{parse_at, LnError, PaymentTarget};
use proptest::prelude::*;
use serde_json::Value;

/// A case that takes longer than this is a hang for our purposes.
const CASE_BUDGET: Duration = Duration::from_secs(5);

fn timed<T>(f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let out = f();
    assert!(t.elapsed() < CASE_BUDGET, "case took {:?}", t.elapsed());
    out
}

// ---------------------------------------------------------------------------
// Seeds: the vectors from the unit tests.
// ---------------------------------------------------------------------------

const B11_25M: &str = "lnbc25m1pvjluezpp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdq5vdhkven9v5sxyetpdeessp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygs9q5sqqqqqqqqqqqqqqqpqsq67gye39hfg3zd8rgc80k32tvy9xk2xunwm5lzexnvpx6fd77en8qaq424dxgt56cag2dpt359k3ssyhetktkpqh24jqnjyw6uqd08sgptq44qu";
const B11_NOAMT: &str = "lnbc1pvjluezsp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygspp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdpl2pkx2ctnv5sxxmmwwd5kgetjypeh2ursdae8g6twvus8g6rfwvs8qun0dfjkxaq9qrsgq357wnc5r2ueh7ck6q93dj32dlqnls087fxdwk8qakdyafkq3yap2r09nt4ndd0unm3z9u5t48y6ucv4r5sg7lk98c77ctvjczkspk5qprc90gx";
const OFFER: &str = "lno1pqps7sjqpgtyzm3qv4uxzmtsd3jjqer9wd3hy6tsw35k7msjzfpy7nz5yqcnygrfdej82um5wf5k2uckyypwa3eyt44h6txtxquqh7lz5djge4afgfjn7k4rgrkuag0jsd5xvxg";
const ADDR: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
const LNURL_BECH32_URL: &str = "https://service.example/lnurlp/bob";

fn seeds() -> Vec<String> {
    vec![
        B11_25M.into(),
        B11_NOAMT.into(),
        OFFER.into(),
        ADDR.into(),
        "1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2".into(),
        format!("bitcoin:{ADDR}?amount=0.001&lightning={B11_25M}"),
        format!("bitcoin:{ADDR}?amount=1"),
        format!("lightning:{B11_NOAMT}"),
        "bob@example.com".into(),
        "lnurlp://example.com/pay/bob".into(),
        LNURL_BECH32_URL.into(),
    ]
}

/// A seed with a handful of random edits: flipped, inserted, deleted bytes
/// and a random truncation.
fn mutated() -> impl Strategy<Value = String> {
    let seeds = seeds();
    (
        0..seeds.len(),
        proptest::collection::vec((any::<u16>(), any::<u8>(), 0u8..3), 0..6),
        proptest::option::of(any::<u16>()),
    )
        .prop_map(move |(i, edits, cut)| {
            let mut b = seeds[i].clone().into_bytes();
            for (pos, byte, kind) in edits {
                if b.is_empty() {
                    break;
                }
                let p = usize::from(pos) % b.len();
                match kind {
                    0 => b[p] = byte,
                    1 => b.insert(p, byte),
                    _ => {
                        b.remove(p);
                    }
                }
            }
            if let Some(c) = cut {
                let n = usize::from(c) % (b.len() + 1);
                b.truncate(n);
            }
            String::from_utf8_lossy(&b).into_owned()
        })
}

// ---------------------------------------------------------------------------
// parse
// ---------------------------------------------------------------------------

fn check_parse(text: &str) {
    for net in [Network::Bitcoin, Network::Testnet, Network::Regtest] {
        let r = timed(|| parse_at(text, net, 1_700_000_000));
        // Whatever it returns, it must be a value we can print and compare.
        if let Ok(t) = r {
            let _ = format!("{t:?}");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    #[test]
    fn parse_survives_arbitrary_text(s in "\\PC{0,2000}") {
        check_parse(&s);
    }

    #[test]
    fn parse_survives_arbitrary_bytes(b in proptest::collection::vec(any::<u8>(), 0..2000)) {
        check_parse(&String::from_utf8_lossy(&b));
    }

    #[test]
    fn parse_survives_mutated_real_vectors(s in mutated()) {
        check_parse(&s);
    }

    #[test]
    fn parse_survives_hostile_bip21_shapes(
        addr in prop_oneof![Just(ADDR.to_string()), Just(String::new()), "\\PC{0,60}"],
        query in "[a-zA-Z0-9%=&:?/._@-]{0,300}",
    ) {
        check_parse(&format!("bitcoin:{addr}?{query}"));
        check_parse(&format!("bitcoin:{addr}?lightning={query}"));
    }
}

#[test]
fn parse_is_bounded_for_huge_and_nested_input() {
    // Far over the length limit, and deeply nested bitcoin: URIs.
    let huge = "a".repeat(5_000_000);
    check_parse(&huge);
    let mut nested = format!("bitcoin:{ADDR}");
    for _ in 0..200 {
        nested = format!(
            "bitcoin:{ADDR}?lightning={}",
            nested.replace('%', "%25").replace(':', "%3A").replace('?', "%3F").replace('=', "%3D")
        );
    }
    check_parse(&nested);
}

// ---------------------------------------------------------------------------
// LNURL
// ---------------------------------------------------------------------------

fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        any::<f64>().prop_map(|f| serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)),
        "\\PC{0,80}".prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 64, 6, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            proptest::collection::vec(("[a-zA-Z_]{1,12}", inner), 0..6)
                .prop_map(|kv| Value::Object(kv.into_iter().collect())),
        ]
    })
}

/// A believable pay request with attacker-chosen field values.
fn lnurl_like() -> impl Strategy<Value = Value> {
    (
        "\\PC{0,80}",
        prop_oneof![Just("payRequest".to_string()), "\\PC{0,12}"],
        any::<u64>(),
        any::<u64>(),
        prop_oneof![
            Just("[[\"text/plain\",\"coffee\"]]".to_string()),
            "\\PC{0,120}"
        ],
    )
        .prop_map(|(cb, tag, min, max, meta)| {
            serde_json::json!({
                "tag": tag, "callback": cb, "minSendable": min, "maxSendable": max, "metadata": meta
            })
        })
}

/// Records every URL asked for and answers with one canned JSON value.
struct Canned {
    answer: Value,
    asked: RefCell<Vec<String>>,
}

impl HttpGet for Canned {
    fn get_json(&self, url: &str) -> Result<Value, LnError> {
        self.asked.borrow_mut().push(url.to_string());
        Ok(self.answer.clone())
    }
}

/// An independent statement of "a host the phone may contact": not local, not
/// private, not link-local, not carrier-grade NAT, not a bare name.
fn host_is_ok_to_contact(u: &str) -> bool {
    let Ok(url) = url::Url::parse(u) else { return false };
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    if host.ends_with(".onion") {
        return true; // plain http is allowed for onion services only
    }
    if url.scheme() != "https" {
        return false;
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => {
                let o = v4.octets();
                !(v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_unspecified()
                    || v4.is_broadcast()
                    || v4.is_multicast()
                    || o[0] == 0
                    || (o[0] == 100 && (o[1] & 0xc0) == 64))
            }
            IpAddr::V6(v6) => {
                let s = v6.segments();
                !(v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || (s[0] & 0xfe00) == 0xfc00
                    || (s[0] & 0xffc0) == 0xfe80
                    || v6.to_ipv4_mapped().is_some())
            }
        };
    }
    host.contains('.') && host != "localhost" && !host.ends_with(".localhost") && !host.ends_with(".local")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    /// For any user/domain, either nothing is fetched, or every URL fetched
    /// is one the phone may contact: the SSRF control holds for all inputs.
    #[test]
    fn a_lightning_address_never_makes_the_phone_contact_a_private_host(
        user in "\\PC{0,30}",
        domain in prop_oneof!["\\PC{0,60}", "[a-z0-9.\\[\\]:-]{0,40}", Just("127.0.0.1".to_string()),
                              Just("localhost".to_string()), Just("[::1]".to_string()),
                              Just("10.0.0.5".to_string()), Just("169.254.169.254".to_string()),
                              Just("0x7f.1".to_string()), Just("2130706433".to_string())],
        answer in lnurl_like(),
    ) {
        let http = Canned { answer, asked: RefCell::new(vec![]) };
        let target = PaymentTarget::LightningAddress { user, domain };
        let _ = timed(|| lnurl::resolve(&target, &http));
        for u in http.asked.borrow().iter() {
            prop_assert!(host_is_ok_to_contact(u), "contacted {u}");
        }
    }

    /// The same for a full LNURL, and for the callback a hostile server names.
    #[test]
    fn an_lnurl_and_its_callback_never_reach_a_private_host(
        url in prop_oneof!["\\PC{0,100}", Just("https://127.0.0.1/p".to_string()),
                           Just("https://example.com/p".to_string()),
                           Just("http://example.com/p".to_string())],
        answer in lnurl_like(),
        amount in any::<u64>(),
    ) {
        let http = Canned { answer, asked: RefCell::new(vec![]) };
        let target = PaymentTarget::LnurlPay { url };
        if let Ok(params) = timed(|| lnurl::resolve(&target, &http)) {
            let _ = timed(|| lnurl::request_invoice(&params, amount, &http));
        }
        for u in http.asked.borrow().iter() {
            prop_assert!(host_is_ok_to_contact(u), "contacted {u}");
        }
    }

    /// Whatever JSON a server answers, resolving and requesting an invoice
    /// neither panics nor hangs.
    #[test]
    fn lnurl_handling_survives_arbitrary_json(answer in arb_json(), amount in any::<u64>()) {
        let http = Canned { answer: answer.clone(), asked: RefCell::new(vec![]) };
        let target = PaymentTarget::LnurlPay { url: "https://example.com/p".into() };
        let _ = timed(|| lnurl::resolve(&target, &http));
        let params = LnurlPayParams {
            callback: "https://example.com/cb".into(),
            min_msat: 0,
            max_msat: u64::MAX,
            description: String::new(),
            metadata: "[]".into(),
        };
        let _ = timed(|| lnurl::request_invoice(&params, amount, &http));
    }
}

// ---------------------------------------------------------------------------
// Esplora scan and sweep
// ---------------------------------------------------------------------------

/// An Esplora that answers every request with one canned status and body.
struct CannedHttp {
    status: u16,
    body: Vec<u8>,
}

impl HttpClient for CannedHttp {
    fn get(&self, url: &str) -> Result<HttpResponse, String> {
        // The tip height lookup gets a number so the scripts are reached.
        if url.ends_with("/blocks/tip/height") {
            return Ok(HttpResponse { status: 200, body: b"800000".to_vec() });
        }
        Ok(HttpResponse { status: self.status, body: self.body.clone() })
    }
    fn post(&self, _: &str, _: &str, _: &str) -> Result<HttpResponse, String> {
        Ok(HttpResponse { status: self.status, body: self.body.clone() })
    }
}

fn esplora_body() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        proptest::collection::vec(any::<u8>(), 0..400),
        arb_json().prop_map(|v| v.to_string().into_bytes()),
        // A plausible UTXO list with hostile numbers.
        proptest::collection::vec(
            (any::<u64>(), any::<u64>(), any::<bool>(), any::<u64>()),
            0..5
        )
        .prop_map(|us| {
            let arr: Vec<Value> = us
                .into_iter()
                .map(|(vout, value, confirmed, height)| {
                    serde_json::json!({
                        "txid": "aa00000000000000000000000000000000000000000000000000000000000001",
                        "vout": vout, "value": value,
                        "status": { "confirmed": confirmed, "block_height": height }
                    })
                })
                .collect();
            Value::Array(arr).to_string().into_bytes()
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 120, ..ProptestConfig::default() })]

    #[test]
    fn an_esplora_scan_survives_any_answer(
        status in prop_oneof![Just(200u16), Just(404), Just(429), Just(500), any::<u16>()],
        body in esplora_body(),
    ) {
        let scripts: Vec<_> = static_scripts(&[7u8; 64], Network::Regtest)
            .into_iter().take(12).map(|(_, s)| s).collect();
        let scan = EsploraScan::with_client("https://esplora.test/api", Arc::new(CannedHttp { status, body }))
            .with_backoff(Duration::ZERO)
            .with_pause(Duration::ZERO);
        if let Ok(found) = timed(|| scan.utxos_for_scripts(&scripts)) {
            for (i, _) in found {
                prop_assert!(i < scripts.len(), "index {i} out of range");
            }
        }
    }
}

/// A source that answers a batch with attacker-chosen UTXOs.
struct Hostile(Mutex<Vec<(usize, u8, u64, u32)>>);

impl ScriptUtxoSource for Hostile {
    fn utxos_for_scripts(&self, _: &[fortis_lightning::ldk_node::bitcoin::ScriptBuf])
        -> Result<Vec<(usize, UtxoRef)>, LnError> {
        let rows = self.0.lock().unwrap().clone();
        Ok(rows
            .into_iter()
            .map(|(i, n, value_sat, confirmations)| {
                (
                    i,
                    UtxoRef {
                        outpoint: fortis_lightning::ldk_node::bitcoin::OutPoint {
                            txid: Txid::from_raw_hash(
                                fortis_lightning::ldk_node::bitcoin::hashes::Hash::from_byte_array([n; 32]),
                            ),
                            vout: 0,
                        },
                        value_sat,
                        confirmations,
                    },
                )
            })
            .collect())
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 120, ..ProptestConfig::default() })]

    /// Whatever a source claims, an accepted scan only holds sane UTXOs.
    #[test]
    fn a_scan_result_only_ever_holds_sane_utxos(
        rows in proptest::collection::vec((any::<usize>(), 0u8..6, any::<u64>(), any::<u32>()), 0..30),
    ) {
        let src = Hostile(Mutex::new(rows));
        if let Ok(found) = timed(|| scan_static_outputs(&[7u8; 64], Network::Regtest, &src, &mut |_, _| {})) {
            let mut seen = std::collections::HashSet::new();
            for u in found {
                prop_assert!(u.value_sat > 0 && u.value_sat <= 21_000_000 * 100_000_000);
                prop_assert!(u.script_index < 2000);
                prop_assert!(seen.insert(u.outpoint), "duplicate outpoint");
            }
        }
    }
}

struct Recording(RefCell<Vec<Transaction>>);

impl Broadcaster for Recording {
    fn broadcast(&self, tx: &Transaction) -> Result<Txid, LnError> {
        self.0.borrow_mut().push(tx.clone());
        Ok(tx.compute_txid())
    }
}

/// Small UTXOs at feerates that put the fee anywhere from a few percent to
/// nearly all of the value: the range the half-the-value cap decides in.
fn sweep_case(
    utxos: Vec<(u8, u32, u64, u32)>,
    feerate: u64,
) -> Result<(), TestCaseError> {
    let mut seen = std::collections::HashSet::new();
    let utxos: Vec<StaticUtxo> = utxos
        .into_iter()
        .filter(|(n, ..)| seen.insert(*n))
        .map(|(n, idx, value_sat, confirmations)| StaticUtxo {
            outpoint: fortis_lightning::ldk_node::bitcoin::OutPoint {
                txid: Txid::from_raw_hash(
                    fortis_lightning::ldk_node::bitcoin::hashes::Hash::from_byte_array([n; 32]),
                ),
                vout: 0,
            },
            value_sat,
            script_index: idx % 2100, // some out of range on purpose
            confirmations,
        })
        .collect();
    let to: Address = {
        use fortis_lightning::ldk_node::bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
        let secp = Secp256k1::new();
        let sk = SecretKey::from_slice(&[0x42; 32]).unwrap();
        Address::p2wpkh(
            &fortis_lightning::ldk_node::bitcoin::CompressedPublicKey(PublicKey::from_secret_key(&secp, &sk)),
            Network::Regtest,
        )
    };
    let rec = Recording(RefCell::new(vec![]));
    let r = timed(|| sweep_static_outputs(&[7u8; 64], Network::Regtest, &utxos, &to, feerate, &rec));
    for tx in rec.0.borrow().iter() {
        prop_assert!(r.is_ok());
        prop_assert_eq!(tx.output.len(), 1);
        prop_assert_eq!(&tx.output[0].script_pubkey, &to.script_pubkey());
        let out = tx.output[0].value.to_sat();
        prop_assert!(out >= DUST_LIMIT_SAT, "dust output {out}");
        let total: u64 = utxos.iter()
            .filter(|u| tx.input.iter().any(|i| i.previous_output == u.outpoint))
            .map(|u| u.value_sat).sum();
        prop_assert!(total >= out);
        prop_assert!((total - out) * 2 <= total, "fee {} of {}", total - out, total);
        prop_assert!(tx.weight().to_wu() <= 400_000);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 120, ..ProptestConfig::default() })]

    #[test]
    fn a_sweep_decides_the_fee_cap_correctly_on_small_utxos(
        utxos in proptest::collection::vec((0u8..8, 0u32..4, 600u64..40_000, 1u32..3), 1..4),
        feerate in 1u64..400,
    ) {
        // Script indices 0..4 are in range, confirmations are at least one.
        sweep_case(utxos, feerate)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 60, ..ProptestConfig::default() })]

    /// For any UTXO set and feerate: no panic, and a broadcast sweep pays only
    /// our address, never loses more than half of what it rescues to fees,
    /// never makes dust, and stays a standard-size transaction.
    #[test]
    fn a_sweep_never_burns_more_than_half_and_never_pays_elsewhere(
        utxos in proptest::collection::vec(
            (0u8..40, any::<u32>(), 0u64..30_000_000, 0u32..3), 0..12),
        feerate in prop_oneof![0u64..60, 60u64..2_000, 2_000u64..20_000],
    ) {
        let mut seen = std::collections::HashSet::new();
        let utxos: Vec<StaticUtxo> = utxos
            .into_iter()
            .filter(|(n, ..)| seen.insert(*n))
            .map(|(n, idx, value_sat, confirmations)| StaticUtxo {
                outpoint: fortis_lightning::ldk_node::bitcoin::OutPoint {
                    txid: Txid::from_raw_hash(
                        fortis_lightning::ldk_node::bitcoin::hashes::Hash::from_byte_array([n; 32]),
                    ),
                    vout: 0,
                },
                value_sat,
                script_index: idx % 2100, // some out of range on purpose
                confirmations,
            })
            .collect();
        let to: Address = {
            use fortis_lightning::ldk_node::bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
            let secp = Secp256k1::new();
            let sk = SecretKey::from_slice(&[0x42; 32]).unwrap();
            Address::p2wpkh(
                &fortis_lightning::ldk_node::bitcoin::CompressedPublicKey(PublicKey::from_secret_key(&secp, &sk)),
                Network::Regtest,
            )
        };
        let rec = Recording(RefCell::new(vec![]));
        let r = timed(|| sweep_static_outputs(&[7u8; 64], Network::Regtest, &utxos, &to, feerate, &rec));
        for tx in rec.0.borrow().iter() {
            prop_assert!(r.is_ok());
            prop_assert_eq!(tx.output.len(), 1);
            prop_assert_eq!(&tx.output[0].script_pubkey, &to.script_pubkey());
            let out = tx.output[0].value.to_sat();
            prop_assert!(out >= DUST_LIMIT_SAT, "dust output {out}");
            let total: u64 = utxos.iter()
                .filter(|u| tx.input.iter().any(|i| i.previous_output == u.outpoint))
                .map(|u| u.value_sat).sum();
            prop_assert!(total >= out);
            prop_assert!((total - out) * 2 <= total, "fee {} of {}", total - out, total);
            prop_assert!(tx.weight().to_wu() <= 400_000);
        }
    }
}
