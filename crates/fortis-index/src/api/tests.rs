use super::*;
use crate::chainstate::BlockTable;
use crate::db::{DbConfig, Durability};
use crate::extract::{Funded, ParsedBlock, Spent, TxRows};
use crate::render::TxSource;
use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::HeaderMap;
use bitcoin::hashes::Hash;
use bitcoin::{Amount, BlockHash, OutPoint, ScriptBuf, WPubkeyHash};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tower::ServiceExt;

/// A 19-zero-byte program ending in `last` (v1's `p2wpkh("a1")` shape).
fn prog(last: u8) -> Program {
    let mut p = [0u8; 20];
    p[19] = last;
    p
}

fn addr(p: &Program) -> String {
    let spk = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array(*p));
    Address::from_script(&spk, Network::Bitcoin)
        .unwrap()
        .to_string()
}

fn txid(n: u8) -> Txid {
    Txid::from_byte_array([n; 32])
}

fn bhash(h: u32) -> BlockHash {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&h.to_be_bytes());
    BlockHash::from_byte_array(b)
}

fn blk(height: u32, txs: Vec<TxRows>) -> ParsedBlock {
    ParsedBlock {
        height,
        hash: bhash(height),
        prev: bhash(height - 1),
        time: 1000 + height,
        txs,
    }
}

fn funds(id: Txid, outs: &[(Program, u32, u64)]) -> TxRows {
    TxRows {
        txid: id,
        funded: outs
            .iter()
            .map(|&(program, vout, value)| Funded {
                program,
                vout,
                value,
            })
            .collect(),
        spent: vec![],
    }
}

#[derive(Default)]
struct FakeTx {
    delay: Duration,
}

impl TxSource for FakeTx {
    fn tx_verbose(&self, txid: &Txid, block: Option<&BlockHash>) -> anyhow::Result<Value> {
        std::thread::sleep(self.delay);
        Ok(json!({
            "txid": txid.to_string(),
            "blockhash": block.map(|b| b.to_string()),
            "vin": [],
            "vout": [{ "value": 0.00001, "scriptPubKey": { "address": "bc1qdest" } }],
        }))
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    state: AppState,
}

fn rig_with(blocks: Vec<ParsedBlock>, mp: MempoolView, tx_delay: Duration) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), &DbConfig { cache_mb: 8 }, "btc").unwrap();
    db.apply(&blocks, 0, Durability::Durable, false).unwrap();
    let chain: SharedChain = Arc::new(ArcSwap::from_pointee(
        BlockTable::from_db(db.load_blocks().unwrap()).unwrap(),
    ));
    let renderer = Arc::new(Renderer::new(
        Arc::new(FakeTx { delay: tx_delay }),
        db.clone(),
        Network::Bitcoin,
    ));
    let state = AppState {
        db,
        chain,
        mempool: Arc::new(ArcSwap::from_pointee(mp)),
        renderer,
        rpc: Rpc::new("http://127.0.0.1:1", "u:p"),
        network: Network::Bitcoin,
        status: Arc::new(SyncStatus::default()),
        chain_id: "btc".into(),
        spends: Arc::new(crate::spends::Spends::new(Box::new(TxNode::new()))),
    };
    Rig { _dir: dir, state }
}

/// Three blocks over two of the wallet's addresses (`a`, `b`) plus one it
/// doesn't own (`z`), and a third wallet address (`d`) never used:
///   100  aa   pays a:500, b:300
///   101  bb   pays a:200 and b:100 in ONE tx
///   102  cc   spends aa:0 (a's 500) to z:480
fn scan_fixture() -> (Rig, Vec<(String, Program)>) {
    let (a, b, z) = (prog(0xa1), prog(0xb1), prog(0xf1));
    let blocks = vec![
        blk(100, vec![funds(txid(0xaa), &[(a, 0, 500), (b, 1, 300)])]),
        blk(101, vec![funds(txid(0xbb), &[(a, 0, 200), (b, 1, 100)])]),
        blk(
            102,
            vec![TxRows {
                txid: txid(0xcc),
                funded: vec![Funded {
                    program: z,
                    vout: 0,
                    value: 480,
                }],
                spent: vec![Spent {
                    program: a,
                    prevout: OutPoint {
                        txid: txid(0xaa),
                        vout: 0,
                    },
                }],
            }],
        ),
    ];
    let targets = [0xa1, 0xb1, 0xd1]
        .map(|l| (addr(&prog(l)), prog(l)))
        .to_vec();
    (
        rig_with(blocks, MempoolView::default(), Duration::ZERO),
        targets,
    )
}

async fn call(
    st: &AppState,
    method: &str,
    uri: &str,
    body: &str,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router(st.clone()).oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (parts.status, parts.headers, bytes)
}

async fn get_json(st: &AppState, uri: &str) -> (StatusCode, Value) {
    let (s, _, b) = call(st, "GET", uri, "").await;
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

#[test]
fn scan_reports_used_addresses_and_only_unspent_outputs() {
    let (r, targets) = scan_fixture();
    let v = view(&r.state.db, &r.state.chain).unwrap();
    let plan = scan_plan(&targets, 50, &v, &MempoolView::default())
        .ok()
        .unwrap();
    assert_eq!(plan.used, vec![targets[0].0.clone(), targets[1].0.clone()]);
    let mut got: Vec<(String, String, u64)> = plan
        .utxos
        .iter()
        .map(|u| {
            (
                u["address"].as_str().unwrap().into(),
                u["txid"].as_str().unwrap().into(),
                u["value"].as_u64().unwrap(),
            )
        })
        .collect();
    got.sort();
    let mut want = vec![
        (targets[0].0.clone(), txid(0xbb).to_string(), 200),
        (targets[1].0.clone(), txid(0xaa).to_string(), 300),
        (targets[1].0.clone(), txid(0xbb).to_string(), 100),
    ];
    want.sort();
    assert_eq!(got, want);
    assert!(plan.utxos.iter().all(|u| u["status"]["confirmed"] == true));
}

#[test]
fn scan_lists_each_tx_once_newest_first_even_when_it_touches_two_addresses() {
    let (r, targets) = scan_fixture();
    let v = view(&r.state.db, &r.state.chain).unwrap();
    let plan = scan_plan(&targets, 50, &v, &MempoolView::default())
        .ok()
        .unwrap();
    let ids: Vec<Txid> = plan
        .confirmed
        .iter()
        .map(|&n| v.reader.txid(n).unwrap().unwrap())
        .collect();
    assert_eq!(ids, vec![txid(0xcc), txid(0xbb), txid(0xaa)]);
    let heights: Vec<u32> = plan
        .confirmed
        .iter()
        .map(|&n| v.chain.height_of(n).unwrap())
        .collect();
    assert_eq!(heights, vec![102, 101, 100]);
}

#[test]
fn scan_caps_history_to_the_newest_n_overall() {
    let (r, targets) = scan_fixture();
    let v = view(&r.state.db, &r.state.chain).unwrap();
    let plan = scan_plan(&targets, 2, &v, &MempoolView::default())
        .ok()
        .unwrap();
    let ids: Vec<Txid> = plan
        .confirmed
        .iter()
        .map(|&n| v.reader.txid(n).unwrap().unwrap())
        .collect();
    assert_eq!(ids, vec![txid(0xcc), txid(0xbb)]);
}

#[tokio::test]
async fn scan_route_rejects_bad_input_before_touching_the_index() {
    let (r, _) = scan_fixture();
    for body in [
        "not json",
        r#"{"nope":1}"#,
        r#"{"addresses":[]}"#,
        r#"{"addresses":[7]}"#,
        r#"{"addresses":["not-an-address"]}"#,
        // valid mainnet P2PKH: real address, but not something this index stores
        r#"{"addresses":["1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2"]}"#,
    ] {
        let (s, _, b) = call(&r.state, "POST", "/scan", body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert!(v["error"].is_string());
    }
}

#[tokio::test]
async fn scan_route_answers_the_full_shape() {
    let (r, targets) = scan_fixture();
    let body =
        json!({ "addresses": targets.iter().map(|t| t.0.clone()).collect::<Vec<_>>() }).to_string();
    let (s, _, b) = call(&r.state, "POST", "/scan", &body).await;
    assert_eq!(s, StatusCode::OK);
    let v: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(v["tip"], 102);
    assert_eq!(v["failed"], json!([]));
    let ids: Vec<&str> = v["txs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["txid"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![
            txid(0xcc).to_string(),
            txid(0xbb).to_string(),
            txid(0xaa).to_string()
        ]
    );
    assert_eq!(
        v["txs"][0]["status"],
        json!({ "confirmed": true, "block_height": 102, "block_time": 1102 })
    );
}

/// Dusting an address past the row cap must not blank the wallet: the scan
/// still answers, with the largest coins listed and the rest summed under
/// `unlisted`, so the balance is whole.
#[tokio::test]
async fn a_dusted_address_still_scans_with_its_whole_balance() {
    let a = prog(0xa1);
    let mut outs = vec![(a, 0, 1_000_000)];
    for v in 1..=(MAX_ROWS as u32 + 1) {
        outs.push((a, v, 1));
    }
    let r = rig_with(vec![blk(100, vec![funds(txid(0xaa), &outs)])], MempoolView::default(), Duration::ZERO);
    let body = json!({ "addresses": [addr(&a)] }).to_string();
    let (s, _, b) = call(&r.state, "POST", "/scan", &body).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let v: Value = serde_json::from_slice(&b).unwrap();
    let utxos = v["utxos"].as_array().unwrap();
    assert_eq!(utxos.len(), MAX_ROWS);
    assert!(utxos.iter().any(|u| u["value"] == 1_000_000), "the big coin is listed");
    assert_eq!(v["unlisted"], json!([{ "address": addr(&a), "count": 2, "value": 2 }]));
    let listed: u64 = utxos.iter().map(|u| u["value"].as_u64().unwrap()).sum();
    assert_eq!(listed + 2, 1_000_000 + MAX_ROWS as u64 + 1, "the whole balance");
    assert_eq!(v["failed"], json!([]));
}

/// Internal detail — the node's address, file paths — never reaches a
/// client (the edge relays these bodies to the public); it goes to the log.
/// The rig's node is unreachable (`127.0.0.1:1`).
#[tokio::test]
async fn an_unreachable_node_is_reported_without_its_address() {
    let (r, _) = scan_fixture();
    let tx = txid(0x77);
    for (method, uri, body) in [
        ("POST", "/tx".to_string(), "0200".to_string()),
        ("GET", format!("/txout/{tx}/0"), String::new()),
    ] {
        let (s, _, b) = call(&r.state, method, &uri, &body).await;
        let text = String::from_utf8_lossy(&b);
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{method} {uri}: {text}");
        assert!(!text.contains("127.0.0.1") && !text.contains("http"), "{method} {uri}: {text}");
        assert!(text.contains("node"), "{method} {uri}: {text}");
    }
}

#[tokio::test]
async fn utxo_route_has_the_v1_shape() {
    let (p1, p2) = (prog(1), prog(2));
    let blocks = vec![
        blk(100, vec![funds(txid(0xaa), &[(p1, 1, 500)])]),
        blk(
            101,
            vec![TxRows {
                txid: txid(0xbb),
                funded: vec![Funded {
                    program: p2,
                    vout: 0,
                    value: 480,
                }],
                spent: vec![Spent {
                    program: p1,
                    prevout: OutPoint {
                        txid: txid(0xaa),
                        vout: 1,
                    },
                }],
            }],
        ),
    ];
    let r = rig_with(blocks, MempoolView::default(), Duration::ZERO);
    let (s, v) = get_json(&r.state, &format!("/address/{}/utxo", addr(&p2))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v,
        json!([{ "txid": txid(0xbb).to_string(), "vout": 0, "value": 480,
                 "status": { "confirmed": true, "block_height": 101 } }])
    );
    let (_, v) = get_json(&r.state, &format!("/address/{}/utxo", addr(&p1))).await;
    assert_eq!(v, json!([]));
    let (s, _, b) = call(&r.state, "GET", "/blocks/tip/height", "").await;
    assert_eq!((s, b), (StatusCode::OK, b"101".to_vec()));
    let (s, v) = get_json(&r.state, "/").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        (v["tip"].clone(), v["mode"].clone(), v["chain"].clone()),
        (json!(101), json!("bulk"), json!("btc"))
    );
}

#[tokio::test]
async fn no_response_carries_cors_headers_and_errors_stay_json() {
    // Only the edge talks to the index; it adds its own CORS for the public.
    // Without it here, a web page open on this machine can't read the index.
    let (r, _) = scan_fixture();
    for (method, uri, want) in [
        ("GET", "/nope", StatusCode::NOT_FOUND),
        ("GET", "/address/not-an-address/utxo", StatusCode::BAD_REQUEST),
        ("DELETE", "/scan", StatusCode::NOT_FOUND),
        ("OPTIONS", "/scan", StatusCode::NOT_FOUND),
        ("GET", "/blocks/tip/height", StatusCode::OK),
    ] {
        let (s, h, b) = call(&r.state, method, uri, "").await;
        assert_eq!(s, want, "{method} {uri}");
        assert!(h.keys().all(|k| !k.as_str().starts_with("access-control-")), "{method} {uri}: {h:?}");
        if s.is_client_error() {
            let v: Value = serde_json::from_slice(&b).unwrap();
            assert!(v["error"].is_string(), "{method} {uri}");
        }
    }
}

/// DNS rebinding reaches 127.0.0.1 under a hostname a web page controls; the
/// edge and local tools use an IP or `localhost`. A request for any other
/// hostname is refused.
#[tokio::test]
async fn a_request_addressed_to_another_hostname_is_refused() {
    let (r, _) = scan_fixture();
    for (host, want) in [
        ("127.0.0.1:8095", StatusCode::OK),
        ("127.0.0.1", StatusCode::OK),
        ("localhost:8094", StatusCode::OK),
        ("LOCALHOST", StatusCode::OK),
        ("[::1]:8095", StatusCode::OK),
        ("192.168.0.124:8095", StatusCode::OK),
        ("rebind.attacker.example:8095", StatusCode::FORBIDDEN),
        ("localhost.attacker.example", StatusCode::FORBIDDEN),
        ("127.0.0.1.nip.io", StatusCode::FORBIDDEN),
    ] {
        let req = Request::builder()
            .method("GET")
            .uri("/blocks/tip/height")
            .header("host", host)
            .body(Body::empty())
            .unwrap();
        let resp = router(r.state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), want, "{host}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heavy_address_is_refused_without_stalling_others() {
    let p = prog(9);
    let outs: Vec<(Program, u32, u64)> = (0..=MAX_ROWS as u32).map(|v| (p, v, 1)).collect();
    let r = rig_with(
        vec![blk(100, vec![funds(txid(1), &outs)])],
        MempoolView::default(),
        Duration::ZERO,
    );
    let st = r.state.clone();
    let heavy =
        tokio::spawn(async move { get_json(&st, &format!("/address/{}/utxo", addr(&p))).await });
    let t = Instant::now();
    let (s, _, _) = call(&r.state, "GET", "/blocks/tip/height", "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        t.elapsed() < Duration::from_millis(100),
        "{:?}",
        t.elapsed()
    );
    let (s, v) = heavy.await.unwrap();
    assert_eq!(
        (s, v),
        (
            StatusCode::BAD_REQUEST,
            json!({ "error": "address too heavy" })
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_node_rpc_does_not_block_other_requests() {
    let p = prog(1);
    let r = rig_with(
        vec![blk(100, vec![funds(txid(1), &[(p, 0, 5)])])],
        MempoolView::default(),
        Duration::from_millis(500),
    );
    let st = r.state.clone();
    let slow =
        tokio::spawn(async move { get_json(&st, &format!("/address/{}/txs", addr(&p))).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let t = Instant::now();
    let (s, _, _) = call(&r.state, "GET", "/blocks/tip/height", "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        t.elapsed() < Duration::from_millis(100),
        "{:?}",
        t.elapsed()
    );
    let (s, v) = slow.await.unwrap();
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v[0]["txid"], txid(1).to_string());
}

#[tokio::test]
async fn pending_utxos_show_unconfirmed_and_hide_mempool_spent() {
    let p = prog(1);
    let blocks = vec![blk(
        100,
        vec![funds(txid(0xaa), &[(p, 0, 500), (p, 1, 700)])],
    )];
    // Mempool tx dd spends aa:0 and pays p 450 at vout 3.
    let dd = TxRows {
        txid: txid(0xdd),
        funded: vec![Funded {
            program: p,
            vout: 3,
            value: 450,
        }],
        spent: vec![Spent {
            program: p,
            prevout: OutPoint {
                txid: txid(0xaa),
                vout: 0,
            },
        }],
    };
    let mp = MempoolView::build(HashMap::from([(txid(0xdd), Arc::new(dd))]));
    let r = rig_with(blocks, mp, Duration::ZERO);
    let (s, v) = get_json(&r.state, &format!("/address/{}/utxo", addr(&p))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v,
        json!([
            { "txid": txid(0xaa).to_string(), "vout": 1, "value": 700, "status": { "confirmed": true, "block_height": 100 } },
            { "txid": txid(0xdd).to_string(), "vout": 3, "value": 450, "status": { "confirmed": false } },
        ])
    );
    // /txs lists the pending tx first, then the confirmed one.
    let (_, v) = get_json(&r.state, &format!("/address/{}/txs", addr(&p))).await;
    let ids: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["txid"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![txid(0xdd).to_string(), txid(0xaa).to_string()]);
    assert_eq!(v[0]["status"], json!({ "confirmed": false }));
}

#[tokio::test]
async fn a_non_p2wpkh_address_has_no_rows() {
    let (r, _) = scan_fixture();
    let (s, v) = get_json(&r.state, "/address/1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2/utxo").await;
    assert_eq!((s, v), (StatusCode::OK, json!([])));
}

#[tokio::test]
async fn a_just_mined_tx_still_in_the_mempool_view_is_listed_once() {
    // Block 100 confirmed aa:0 to p; the mempool view (not yet refreshed) still
    // holds aa as pending.
    let p = prog(1);
    let aa = funds(txid(0xaa), &[(p, 0, 500)]);
    let mp = MempoolView::build(HashMap::from([(txid(0xaa), Arc::new(aa.clone()))]));
    let r = rig_with(vec![blk(100, vec![aa])], mp, Duration::ZERO);
    let (_, v) = get_json(&r.state, &format!("/address/{}/utxo", addr(&p))).await;
    assert_eq!(
        v,
        json!([{ "txid": txid(0xaa).to_string(), "vout": 0, "value": 500,
                 "status": { "confirmed": true, "block_height": 100 } }])
    );
}

#[tokio::test]
async fn pending_txs_rendered_per_request_are_capped() {
    let p = prog(1);
    let txs: HashMap<Txid, Arc<TxRows>> = (0..150u32)
        .map(|i| {
            let mut b = [0u8; 32];
            b[..4].copy_from_slice(&i.to_be_bytes());
            let id = Txid::from_byte_array(b);
            (id, Arc::new(funds(id, &[(p, 0, 1)])))
        })
        .collect();
    let r = rig_with(
        vec![blk(100, vec![])],
        MempoolView::build(txs),
        Duration::ZERO,
    );
    let (s, v) = get_json(&r.state, &format!("/address/{}/txs", addr(&p))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), PENDING_MAX);
    let body = json!({ "addresses": [addr(&p)] }).to_string();
    let (_, _, b) = call(&r.state, "POST", "/scan", &body).await;
    let v: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(v["txs"].as_array().unwrap().len(), PENDING_MAX);
}

#[test]
fn txout_null_is_404_spent_or_unknown() {
    let e = txout_reply(&Value::Null).unwrap_err();
    assert_eq!(e.0, StatusCode::NOT_FOUND);
    assert_eq!(e.1, "spent or unknown");
}

#[test]
fn txout_maps_gettxout_to_sats_and_script() {
    let raw = json!({
        "bestblock": "00",
        "confirmations": 3,
        "value": 0.00123457,
        "scriptPubKey": { "asm": "x", "hex": "0014abcd", "type": "witness_v0_keyhash" },
        "coinbase": false,
    });
    let Json(v) = txout_reply(&raw).unwrap();
    assert_eq!(
        v,
        json!({ "value_sat": 123_457u64, "scriptpubkey_hex": "0014abcd", "confirmations": 3 })
    );
}

#[test]
fn txout_mempool_query_maps_to_gettxout_include_mempool() {
    let t = txid(1);
    let args = |q: Option<&str>| gettxout_args(&t, 2, q).unwrap();
    // Default: mempool included, so a mempool spend reads as spent.
    assert_eq!(args(None), json!([t.to_string(), 2, true]));
    assert_eq!(args(Some("")), json!([t.to_string(), 2, true]));
    assert_eq!(args(Some("mempool=true")), json!([t.to_string(), 2, true]));
    // Confirmed chain only: still there while only a mempool spend exists.
    assert_eq!(args(Some("mempool=false")), json!([t.to_string(), 2, false]));
    assert_eq!(args(Some("x=1&mempool=false")), json!([t.to_string(), 2, false]));
    let e = gettxout_args(&t, 2, Some("mempool=no")).unwrap_err();
    assert_eq!(e.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn txout_bad_mempool_param_is_400_without_asking_the_node() {
    let r = rig_with(vec![], MempoolView::default(), Duration::ZERO);
    let path = format!("/txout/{}/0?mempool=maybe", txid(1));
    let (s, _, _) = call(&r.state, "GET", &path, "").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn txout_bad_txid_is_400_without_asking_the_node() {
    let r = rig_with(vec![], MempoolView::default(), Duration::ZERO);
    let (s, _, _) = call(&r.state, "GET", "/txout/zz/0", "").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

// ---- /tx/:txid, /tx/:txid/hex, /tx/:txid/outspend/:vout (swap HTLC lookups) ----

/// A node that knows one mined funding tx (block 700, which is main-chain),
/// whose vout 0 a mined spend took (block 702) and whose vout 1 is unspent.
struct TxNode {
    funding: bitcoin::Transaction,
    spend: bitcoin::Transaction,
    /// Mined in a block that is no longer on the main chain.
    stale: bitcoin::Transaction,
}

impl TxNode {
    fn new() -> Self {
        use bitcoin::{absolute::LockTime, transaction::Version, Sequence, TxIn, TxOut, Witness};
        let out = |sat: u64, spk: ScriptBuf| TxOut { value: Amount::from_sat(sat), script_pubkey: spk };
        let funding = bitcoin::Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(txid(0x11), 0),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![
                out(24_053, ScriptBuf::from_hex("00203c5b9955e14f8a35f2b8d7f2b394675e1fb26d342bb81024c86b81bdb7defdef").unwrap()),
                out(400, ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array(prog(0x42)))),
            ],
        };
        let spend = bitcoin::Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(funding.compute_txid(), 0),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![out(23_000, ScriptBuf::new())],
        };
        let mut stale = spend.clone();
        stale.output[0].value = Amount::from_sat(22_000);
        TxNode { funding, spend, stale }
    }

    fn verbose(&self, t: &bitcoin::Transaction, block: BlockHash) -> Value {
        json!({
            "txid": t.compute_txid().to_string(),
            "blockhash": block.to_string(),
            "blocktime": 1_791_761_376u64,
            "vin": t.input.iter().map(|i| json!({
                "txid": i.previous_output.txid.to_string(), "vout": i.previous_output.vout,
            })).collect::<Vec<_>>(),
            "vout": t.output.iter().map(|o| json!({
                "value": o.value.to_btc(),
                "scriptPubKey": { "hex": o.script_pubkey.to_hex_string() },
            })).collect::<Vec<_>>(),
        })
    }
}

impl crate::spends::SpendNode for TxNode {
    fn tx(&self, id: &Txid) -> anyhow::Result<Option<Value>> {
        Ok(if *id == self.funding.compute_txid() {
            Some(self.verbose(&self.funding, bhash(700)))
        } else if *id == self.spend.compute_txid() {
            Some(self.verbose(&self.spend, bhash(702)))
        } else if *id == self.stale.compute_txid() {
            Some(self.verbose(&self.stale, bhash(999))) // not on the main chain
        } else {
            None
        })
    }
    fn raw_tx(&self, id: &Txid) -> anyhow::Result<Option<String>> {
        Ok((*id == self.spend.compute_txid()).then(|| bitcoin::consensus::encode::serialize_hex(&self.spend)))
    }
    fn unspent(&self, op: &OutPoint) -> anyhow::Result<bool> {
        Ok(*op == OutPoint::new(self.funding.compute_txid(), 1))
    }
    fn known_spender(&self, _: &OutPoint) -> anyhow::Result<Option<Txid>> {
        Ok(None)
    }
    fn height_of(&self, h: &BlockHash) -> anyhow::Result<Option<u32>> {
        Ok((700..=703).find(|&n| bhash(n) == *h))
    }
    fn tip(&self) -> anyhow::Result<u32> {
        Ok(703)
    }
    fn block(&self, height: u32) -> anyhow::Result<(BlockHash, Vec<bitcoin::Transaction>)> {
        Ok((bhash(height), if height == 702 { vec![self.spend.clone()] } else { vec![] }))
    }
}

fn tx_rig() -> (Rig, TxNode) {
    let mut r = rig_with(vec![], MempoolView::default(), Duration::ZERO);
    r.state.spends = Arc::new(crate::spends::Spends::new(Box::new(TxNode::new())));
    (r, TxNode::new())
}

#[tokio::test]
async fn tx_route_gives_outputs_with_script_hex_and_the_confirmed_height() {
    let (r, n) = tx_rig();
    let (s, v) = get_json(&r.state, &format!("/tx/{}", n.funding.compute_txid())).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["vout"][0]["value"], 24_053);
    assert_eq!(
        v["vout"][0]["scriptpubkey"],
        "00203c5b9955e14f8a35f2b8d7f2b394675e1fb26d342bb81024c86b81bdb7defdef"
    );
    assert_eq!(v["status"]["confirmed"], true);
    assert_eq!(v["status"]["block_height"], 700);
}

#[tokio::test]
async fn tx_routes_404_an_unknown_tx() {
    let (r, _) = tx_rig();
    for path in ["", "/hex", "/outspend/0"] {
        let (s, _) = get_json(&r.state, &format!("/tx/{}{path}", txid(0x77))).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "/tx/…{path}");
    }
}

#[tokio::test]
async fn hex_route_gives_the_raw_tx() {
    let (r, n) = tx_rig();
    let (s, _, b) = call(&r.state, "GET", &format!("/tx/{}/hex", n.spend.compute_txid()), "").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(String::from_utf8(b).unwrap(), bitcoin::consensus::encode::serialize_hex(&n.spend));
}

#[tokio::test]
async fn outspend_route_reports_unspent_spent_and_unknown_outputs() {
    let (r, n) = tx_rig();
    let f = n.funding.compute_txid();
    let (s, v) = get_json(&r.state, &format!("/tx/{f}/outspend/1")).await;
    assert_eq!((s, v), (StatusCode::OK, json!({ "spent": false })));
    let (s, v) = get_json(&r.state, &format!("/tx/{f}/outspend/0")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["spent"], true);
    assert_eq!(v["txid"], n.spend.compute_txid().to_string());
    assert_eq!(v["status"], json!({ "confirmed": true, "block_height": 702, "block_hash": bhash(702).to_string() }));
    let (s, _) = get_json(&r.state, &format!("/tx/{f}/outspend/5")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_tx_in_a_block_off_the_main_chain_reads_unconfirmed() {
    let (r, n) = tx_rig();
    let (s, v) = get_json(&r.state, &format!("/tx/{}", n.stale.compute_txid())).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["status"], json!({ "confirmed": false }));
}

/// An outspend the index won't search (older than its window) is a final 422,
/// not a 503 that callers would retry forever.
#[tokio::test]
async fn an_outspend_outside_the_search_window_is_a_final_422() {
    struct OldNode(TxNode);
    impl crate::spends::SpendNode for OldNode {
        fn tx(&self, id: &Txid) -> anyhow::Result<Option<Value>> { self.0.tx(id) }
        fn raw_tx(&self, id: &Txid) -> anyhow::Result<Option<String>> { self.0.raw_tx(id) }
        fn unspent(&self, op: &OutPoint) -> anyhow::Result<bool> { self.0.unspent(op) }
        fn known_spender(&self, op: &OutPoint) -> anyhow::Result<Option<Txid>> { self.0.known_spender(op) }
        fn height_of(&self, h: &BlockHash) -> anyhow::Result<Option<u32>> { self.0.height_of(h) }
        fn tip(&self) -> anyhow::Result<u32> { Ok(700 + crate::spends::MAX_SCAN_BLOCKS + 1) }
        fn block(&self, h: u32) -> anyhow::Result<(BlockHash, Vec<bitcoin::Transaction>)> { self.0.block(h) }
    }
    let mut r = rig_with(vec![], MempoolView::default(), Duration::ZERO);
    let n = TxNode::new();
    let f = n.funding.compute_txid();
    r.state.spends = Arc::new(crate::spends::Spends::new(Box::new(OldNode(n))));
    let (s, _) = get_json(&r.state, &format!("/tx/{f}/outspend/0")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}
