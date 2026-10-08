use super::*;
use crate::extract::{Funded, ParsedBlock, Spent, TxRows};
use bitcoin::hashes::Hash;
use bitcoin::{BlockHash, OutPoint, Txid};

const P1: Program = [1; 20];
const P2: Program = [2; 20];

fn hash(h: u32) -> BlockHash {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&h.to_be_bytes());
    BlockHash::from_byte_array(b)
}

fn txid(n: u8) -> Txid {
    Txid::from_byte_array([n; 32])
}

fn blk(height: u32, txs: Vec<TxRows>) -> ParsedBlock {
    ParsedBlock {
        height,
        hash: hash(height),
        prev: hash(height.wrapping_sub(1)),
        time: 1_700_000_000 + height,
        txs,
    }
}

fn fund(t: Txid, program: Program, vout: u32, value: u64) -> TxRows {
    TxRows {
        txid: t,
        funded: vec![Funded {
            program,
            vout,
            value,
        }],
        spent: vec![],
    }
}

fn cfg() -> DbConfig {
    DbConfig { cache_mb: 8 }
}

fn open(dir: &tempfile::TempDir) -> Db {
    Db::open(dir.path(), &cfg(), "btc").unwrap()
}

/// 100: A funds p1 vout1 500 · 101: B spends A:1 (program p1), funds p2 vout0 480.
fn a_then_b() -> Vec<ParsedBlock> {
    let (a, b) = (txid(0xaa), txid(0xbb));
    vec![
        blk(100, vec![fund(a, P1, 1, 500)]),
        blk(
            101,
            vec![TxRows {
                txid: b,
                funded: vec![Funded {
                    program: P2,
                    vout: 0,
                    value: 480,
                }],
                spent: vec![Spent {
                    program: P1,
                    prevout: OutPoint { txid: a, vout: 1 },
                }],
            }],
        ),
    ]
}

fn assert_a_then_b_state(db: &Db) {
    let r = db.reader();
    assert_eq!(r.history(&P1, 100).unwrap(), vec![1, 0]);
    assert!(r.utxos(&P1, 100).unwrap().is_empty());
    assert_eq!(
        r.utxos(&P2, 100).unwrap(),
        vec![(
            OutPoint {
                txid: txid(0xbb),
                vout: 0
            },
            UtxoVal {
                value: 480,
                height: 101
            }
        )]
    );
    assert_eq!(r.txid(0).unwrap(), Some(txid(0xaa)));
    assert_eq!(r.txid(1).unwrap(), Some(txid(0xbb)));
    assert_eq!(r.tip().unwrap(), Some((101, hash(101))));
    let blocks = db.load_blocks().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(
        (blocks[0].0, blocks[0].1.first_txnum, blocks[0].1.n_txs),
        (100, 0, 1)
    );
    assert_eq!(
        (blocks[1].0, blocks[1].1.first_txnum, blocks[1].1.n_txs),
        (101, 1, 1)
    );
    assert_eq!(blocks[1].1.hash, hash(101));
}

#[test]
fn fund_then_spend_across_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let blocks = a_then_b();
    let recs = db
        .apply(&blocks[..1], 0, Durability::Durable, false)
        .unwrap();
    assert_eq!(recs[0].first_txnum, 0);
    db.apply(&blocks[1..], 1, Durability::Durable, false)
        .unwrap();
    assert_a_then_b_state(&db);
}

#[test]
fn fund_and_spend_in_one_batch_matches_two_batches() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let recs = db.apply(&a_then_b(), 0, Durability::Bulk, false).unwrap();
    assert_eq!(recs.len(), 2);
    assert_a_then_b_state(&db);
}

#[test]
fn a_spend_seen_by_two_programs_in_one_tx_is_one_history_row_each() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let (a, b) = (txid(0xaa), txid(0xbb));
    let blocks = vec![
        blk(
            100,
            vec![TxRows {
                txid: a,
                funded: vec![
                    Funded {
                        program: P1,
                        vout: 0,
                        value: 1,
                    },
                    Funded {
                        program: P1,
                        vout: 1,
                        value: 2,
                    },
                    Funded {
                        program: P2,
                        vout: 2,
                        value: 3,
                    },
                ],
                spent: vec![],
            }],
        ),
        blk(
            101,
            vec![TxRows {
                txid: b,
                funded: vec![Funded {
                    program: P1,
                    vout: 0,
                    value: 5,
                }],
                spent: vec![
                    Spent {
                        program: P1,
                        prevout: OutPoint { txid: a, vout: 0 },
                    },
                    Spent {
                        program: P1,
                        prevout: OutPoint { txid: a, vout: 1 },
                    },
                    Spent {
                        program: P2,
                        prevout: OutPoint { txid: a, vout: 2 },
                    },
                ],
            }],
        ),
    ];
    db.apply(&blocks, 0, Durability::Durable, true).unwrap();
    let r = db.reader();
    assert_eq!(r.history(&P1, 100).unwrap(), vec![1, 0]);
    assert_eq!(r.history(&P2, 100).unwrap(), vec![1, 0]);
    assert_eq!(r.utxos(&P1, 100).unwrap().len(), 1);
    assert!(r.utxos(&P2, 100).unwrap().is_empty());
}

#[test]
fn history_is_newest_first_and_bounded_by_limit() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let txs = (0..5u8).map(|i| fund(txid(i + 1), P1, 0, 10)).collect();
    db.apply(&[blk(100, txs)], 0, Durability::Durable, false)
        .unwrap();
    let r = db.reader();
    assert_eq!(r.history(&P1, 3).unwrap(), vec![4, 3, 2]);
    assert_eq!(r.history(&P1, 100).unwrap(), vec![4, 3, 2, 1, 0]);
}

#[test]
fn refuses_a_database_for_the_other_chain() {
    let dir = tempfile::tempdir().unwrap();
    drop(open(&dir));
    let err = Db::open(dir.path(), &cfg(), "xbt@961640")
        .err()
        .expect("must refuse");
    match err.downcast_ref::<OpenError>() {
        Some(OpenError::ChainMismatch { found, expected }) => {
            assert_eq!((found.as_str(), expected.as_str()), ("btc", "xbt@961640"));
        }
        other => panic!("expected ChainMismatch, got {other:?} ({err:#})"),
    }
}

#[test]
fn refuses_a_v1_database() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut o = rocksdb::Options::default();
        o.create_if_missing(true);
        o.create_missing_column_families(true);
        rocksdb::DB::open_cf(&o, dir.path(), ["outputs"]).unwrap();
    }
    let err = Db::open(dir.path(), &cfg(), "btc")
        .err()
        .expect("must refuse");
    assert!(
        matches!(
            err.downcast_ref::<OpenError>(),
            Some(OpenError::Schema { .. })
        ),
        "{err:#}"
    );
}

#[test]
fn bulk_writes_survive_a_clean_close() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open(&dir);
        db.apply(&a_then_b(), 0, Durability::Bulk, false).unwrap();
    }
    let db = open(&dir);
    assert_a_then_b_state(&db);
}

#[test]
fn utxos_past_max_rows_is_too_heavy() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let funded = (0..11)
        .map(|v| Funded {
            program: P1,
            vout: v,
            value: 1,
        })
        .collect();
    db.apply(
        &[blk(
            100,
            vec![TxRows {
                txid: txid(1),
                funded,
                spent: vec![],
            }],
        )],
        0,
        Durability::Durable,
        false,
    )
    .unwrap();
    let r = db.reader();
    let err = r.utxos(&P1, 10).unwrap_err();
    assert!(err.downcast_ref::<TooHeavy>().is_some(), "{err:#}");
    assert_eq!(r.utxos(&P1, 11).unwrap().len(), 11);
}

#[test]
fn unused_program_lookup_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.apply(&a_then_b(), 0, Durability::Durable, false)
        .unwrap();
    let r = db.reader();
    let p = [9u8; 20];
    assert!(r.history(&p, 100).unwrap().is_empty());
    assert!(r.utxos(&p, 100).unwrap().is_empty());
    assert_eq!(
        r.utxo(
            &p,
            &OutPoint {
                txid: txid(0xaa),
                vout: 1
            }
        )
        .unwrap(),
        None
    );
    assert_eq!(r.txid(99).unwrap(), None);
    assert_eq!(r.render_get(0).unwrap(), None);
}

#[test]
fn empty_database_has_no_tip() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    assert_eq!(db.reader().tip().unwrap(), None);
    assert!(db.load_blocks().unwrap().is_empty());
}

#[test]
fn rollback_restores_the_previous_state_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let blocks = a_then_b();
    db.apply(&blocks[..1], 0, Durability::Durable, true)
        .unwrap();
    let recs = db
        .apply(&blocks[1..], 1, Durability::Durable, true)
        .unwrap();
    db.render_put(1, b"x").unwrap();
    db.rollback(101, &recs[0], Some((100, hash(100)))).unwrap();
    let r = db.reader();
    assert_eq!(r.history(&P1, 100).unwrap(), vec![0]);
    assert_eq!(
        r.utxos(&P1, 100).unwrap(),
        vec![(
            OutPoint {
                txid: txid(0xaa),
                vout: 1
            },
            UtxoVal {
                value: 500,
                height: 100
            }
        )]
    );
    assert!(r.utxos(&P2, 100).unwrap().is_empty());
    assert!(r.history(&P2, 100).unwrap().is_empty());
    assert_eq!(r.txid(1).unwrap(), None);
    assert_eq!(r.render_get(1).unwrap(), None);
    assert_eq!(db.load_blocks().unwrap().len(), 1);
    assert_eq!(r.tip().unwrap(), Some((100, hash(100))));
}

#[test]
fn rollback_of_a_block_that_funds_and_spends_the_same_output_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let (a, b) = (txid(0xaa), txid(0xbb));
    db.apply(
        &[blk(100, vec![fund(txid(1), P2, 0, 7)])],
        0,
        Durability::Durable,
        true,
    )
    .unwrap();
    let recs = db
        .apply(
            &[blk(
                101,
                vec![
                    fund(a, P1, 0, 500),
                    TxRows {
                        txid: b,
                        funded: vec![],
                        spent: vec![Spent {
                            program: P1,
                            prevout: OutPoint { txid: a, vout: 0 },
                        }],
                    },
                ],
            )],
            1,
            Durability::Durable,
            true,
        )
        .unwrap();
    db.rollback(101, &recs[0], Some((100, hash(100)))).unwrap();
    let r = db.reader();
    assert!(r.utxos(&P1, 100).unwrap().is_empty());
    assert!(r.history(&P1, 100).unwrap().is_empty());
    assert_eq!(r.utxos(&P2, 100).unwrap().len(), 1);
}

#[test]
fn rollback_to_empty_clears_the_tip() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let recs = db
        .apply(&a_then_b()[..1], 0, Durability::Durable, true)
        .unwrap();
    db.rollback(100, &recs[0], None).unwrap();
    assert_eq!(db.reader().tip().unwrap(), None);
    assert!(db.load_blocks().unwrap().is_empty());
    assert!(db.reader().utxos(&P1, 100).unwrap().is_empty());
}

#[test]
fn rollback_without_undo_is_no_undo() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let recs = db.apply(&a_then_b(), 0, Durability::Bulk, false).unwrap();
    let err = db
        .rollback(101, &recs[1], Some((100, hash(100))))
        .unwrap_err();
    match err.downcast_ref::<NoUndo>() {
        Some(NoUndo { height }) => assert_eq!(*height, 101),
        None => panic!("expected NoUndo, got {err:#}"),
    }
}

#[test]
fn prune_drops_old_undo_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let recs = db.apply(&a_then_b(), 0, Durability::Durable, true).unwrap();
    db.prune_undo(101).unwrap();
    db.rollback(101, &recs[1], Some((100, hash(100)))).unwrap();
    let err = db.rollback(100, &recs[0], None).unwrap_err();
    assert!(err.downcast_ref::<NoUndo>().is_some(), "{err:#}");
}

#[test]
fn undo_record_round_trips() {
    let u = Undo {
        first_txnum: 1 << 33,
        n_txs: 3,
        added_history: vec![[1; 25], [2; 25]],
        added_utxo: vec![[3; 56]],
        deleted_utxo: vec![(
            [4; 56],
            UtxoVal {
                value: 9,
                height: 8,
            },
        )],
    };
    assert_eq!(Undo::decode(&u.encode()).unwrap(), u);
    assert!(Undo::decode(&u.encode()[..30]).is_err());
}
