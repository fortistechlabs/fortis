use super::*;
use crate::chainstate::BlockTable;
use crate::db::{DbConfig, Durability};
use crate::extract::{Funded, ParsedBlock, Spent, TxRows};
use crate::render::TxSource;
use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::HeaderMap;
use bitcoin::hashes::Hash;
use bitcoin::{BlockHash, OutPoint, ScriptBuf, WPubkeyHash};
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
async fn every_response_carries_cors_headers_including_errors() {
    let (r, _) = scan_fixture();
    for (method, uri, want) in [
        ("GET", "/nope", StatusCode::NOT_FOUND),
        (
            "GET",
            "/address/not-an-address/utxo",
            StatusCode::BAD_REQUEST,
        ),
        ("DELETE", "/scan", StatusCode::NOT_FOUND),
        ("OPTIONS", "/scan", StatusCode::NO_CONTENT),
        ("GET", "/blocks/tip/height", StatusCode::OK),
    ] {
        let (s, h, b) = call(&r.state, method, uri, "").await;
        assert_eq!(s, want, "{method} {uri}");
        assert_eq!(h["access-control-allow-origin"], "*");
        assert_eq!(h["access-control-allow-methods"], "GET, POST, OPTIONS");
        assert_eq!(h["access-control-allow-headers"], "content-type");
        if s.is_client_error() {
            let v: Value = serde_json::from_slice(&b).unwrap();
            assert!(v["error"].is_string(), "{method} {uri}");
        }
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
