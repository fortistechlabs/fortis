use super::*;
use crate::extract::{Funded, ParsedBlock, Spent, TxRows};
use crate::spendcf::{BlockSpend, SpendRec};
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
        spends: vec![],
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
    DbConfig { cache_mb: 8, spends_from: None }
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
        added_spends: vec![[5; 36]],
    };
    assert_eq!(Undo::decode(&u.encode()).unwrap(), u);
    assert!(Undo::decode(&u.encode()[..30]).is_err());
}

/// Every rendered transaction is cached, so the cache is capped: past its cap
/// the oldest half (the lowest txnums) goes. A dropped one is just rendered
/// from the node again when asked for.
#[test]
fn the_render_cache_keeps_to_its_cap() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir).with_render_cap(10);
    for n in 0..25u64 {
        db.render_put(n, format!("tx{n}").as_bytes()).unwrap();
    }
    let r = db.reader();
    let kept: Vec<u64> = (0..25).filter(|n| r.render_get(*n).unwrap().is_some()).collect();
    assert!(kept.len() <= 10, "{kept:?}");
    assert!(kept.contains(&24), "the newest stays: {kept:?}");
    assert!(!kept.contains(&0), "the oldest goes: {kept:?}");
    assert_eq!(r.render_get(24).unwrap().as_deref(), Some(&b"tx24"[..]));
}

/// An address dusted past the row cap still has a full balance: the largest
/// coins are listed, the rest are counted and summed (streamed, so memory
/// stays bounded however much dust there is), and a coin `skip` names (one
/// the mempool is spending) is in neither.
#[test]
fn past_the_cap_the_largest_coins_are_listed_and_the_rest_summed() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let mut txs = Vec::new();
    for n in 0..10u8 {
        // values 10, 20, …, 100
        txs.push(fund(txid(n + 1), P1, 0, (u64::from(n) + 1) * 10));
    }
    db.apply(&[blk(100, txs)], 0, Durability::Durable, false).unwrap();
    let r = db.reader();
    let spent_in_mempool = OutPoint { txid: txid(10), vout: 0 }; // the 100
    let (listed, rest) = r.utxos_largest(&P1, 3, |op| *op != spent_in_mempool).unwrap();
    let mut values: Vec<u64> = listed.iter().map(|(_, u)| u.value).collect();
    values.sort_unstable();
    assert_eq!(values, [70, 80, 90]);
    assert_eq!(rest, Unlisted { count: 6, value: 10 + 20 + 30 + 40 + 50 + 60 });
    // Under the cap: everything listed, nothing unlisted.
    let (all, none) = r.utxos_largest(&P1, 100, |_| true).unwrap();
    assert_eq!(all.len(), 10);
    assert_eq!(none, Unlisted::default());
}

fn spend(prev: u8, vout: u32, by: u8, vin: u32) -> BlockSpend {
    BlockSpend {
        prevout: OutPoint { txid: txid(prev), vout },
        txid: txid(by),
        vin,
    }
}

/// A block whose only content is `spends` (no indexed program).
fn blk_spending(height: u32, spends: Vec<BlockSpend>) -> ParsedBlock {
    ParsedBlock {
        spends,
        ..blk(height, vec![])
    }
}

fn open_spends(dir: &tempfile::TempDir, from: Option<u32>) -> Db {
    let cfg = DbConfig {
        spends_from: from,
        ..cfg()
    };
    Db::open(dir.path(), &cfg, "btc").unwrap()
}

fn op(prev: u8, vout: u32) -> OutPoint {
    OutPoint { txid: txid(prev), vout }
}

#[test]
fn commit_records_spends_from_start_height() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_spends(&dir, Some(11));
    assert_eq!(db.spends_from(), Some(11));
    assert_eq!(db.spends_watermark(), None);
    db.apply(&[blk_spending(10, vec![spend(1, 0, 0x10, 0)])], 0, Durability::Bulk, false)
        .unwrap();
    db.apply(
        &[
            blk_spending(11, vec![spend(2, 1, 0x11, 2)]),
            blk_spending(12, vec![spend(3, 5, 0x12, 0), spend(4, 0, 0x12, 1)]),
        ],
        0,
        Durability::Durable,
        true,
    )
    .unwrap();
    assert_eq!(db.spend_of(&op(1, 0)).unwrap(), None);
    assert_eq!(
        db.spend_of(&op(2, 1)).unwrap(),
        Some(SpendRec { txid: txid(0x11), vin: 2, height: 11 })
    );
    assert_eq!(
        db.spend_of(&op(3, 5)).unwrap(),
        Some(SpendRec { txid: txid(0x12), vin: 0, height: 12 })
    );
    assert_eq!(
        db.spend_of(&op(4, 0)).unwrap(),
        Some(SpendRec { txid: txid(0x12), vin: 1, height: 12 })
    );
    assert_eq!(db.spend_of(&op(3, 4)).unwrap(), None);
    assert_eq!(db.spends_watermark(), Some(12));
}

/// The watermark only advances over contiguous heights from the start: a
/// block past a gap is recorded but doesn't claim the gap is complete.
#[test]
fn watermark_does_not_jump_a_gap() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_spends(&dir, Some(11));
    db.apply(&[blk_spending(13, vec![spend(2, 1, 0x13, 0)])], 0, Durability::Durable, false)
        .unwrap();
    assert!(db.spend_of(&op(2, 1)).unwrap().is_some());
    assert_eq!(db.spends_watermark(), None);
}

#[test]
fn disconnect_removes_spends() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_spends(&dir, Some(11));
    let r11 = db
        .apply(&[blk_spending(11, vec![spend(2, 1, 0x11, 0)])], 0, Durability::Durable, true)
        .unwrap();
    let r12 = db
        .apply(&[blk_spending(12, vec![spend(3, 0, 0x12, 0)])], 0, Durability::Durable, true)
        .unwrap();
    assert_eq!(db.spends_watermark(), Some(12));
    db.rollback(12, &r12[0], Some((11, r11[0].hash))).unwrap();
    assert_eq!(db.spend_of(&op(3, 0)).unwrap(), None);
    assert!(db.spend_of(&op(2, 1)).unwrap().is_some());
    assert_eq!(db.spends_watermark(), Some(11));
    // Re-connecting 12 (or its replacement) resumes from the lowered mark.
    db.apply(&[blk_spending(12, vec![spend(5, 0, 0x22, 0)])], 0, Durability::Durable, true)
        .unwrap();
    assert_eq!(db.spends_watermark(), Some(12));
    assert!(db.spend_of(&op(5, 0)).unwrap().is_some());
}

#[test]
fn no_spends_when_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_spends(&dir, None);
    db.apply(&[blk_spending(11, vec![spend(2, 1, 0x11, 0)])], 0, Durability::Durable, true)
        .unwrap();
    assert_eq!(db.spends_from(), None);
    assert_eq!(db.spend_of(&op(2, 1)).unwrap(), None);
    assert_eq!(db.spends_watermark(), None);
}

/// A database made before `spends` existed (no such column family) opens
/// without a rebuild: the column family is created, empty, with no watermark.
#[test]
fn an_old_database_gains_an_empty_spends_on_open() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        let old: Vec<&str> = ALL_CFS.iter().copied().filter(|c| *c != CF_SPENDS).collect();
        let raw = DB::open_cf(&opts, dir.path(), &old).unwrap();
        let meta = raw.cf_handle(CF_META).unwrap();
        raw.put_cf(meta, META_SCHEMA, SCHEMA_VERSION.to_be_bytes()).unwrap();
        raw.put_cf(meta, META_CHAIN, b"btc").unwrap();
    }
    let db = open_spends(&dir, Some(0));
    assert_eq!(db.spends_watermark(), None);
    assert_eq!(db.spend_of(&op(2, 1)).unwrap(), None);
    db.apply(&[blk_spending(0, vec![spend(2, 1, 0x11, 0)])], 0, Durability::Durable, true)
        .unwrap();
    assert_eq!(db.spends_watermark(), Some(0));
}

/// Undo records written before `spends` existed have no spends section and
/// still decode; one with spends round-trips.
#[test]
fn undo_with_spends_round_trips_and_old_records_decode() {
    let old = Undo {
        first_txnum: 5,
        n_txs: 1,
        added_history: vec![[1; 25]],
        ..Undo::default()
    };
    let bytes = old.encode();
    assert_eq!(bytes.len(), 5 + 4 + 4 + 25 + 4 + 4, "no spends: the old layout exactly");
    assert_eq!(Undo::decode(&bytes).unwrap(), old);
    let new = Undo {
        added_spends: vec![[7; 36], [8; 36]],
        ..old
    };
    assert_eq!(Undo::decode(&new.encode()).unwrap(), new);
    assert!(Undo::decode(&new.encode()[..new.encode().len() - 1]).is_err());
}

/// The mark is tied to the start height it was counted from: lowering
/// `spends_from` leaves the heights below the old start unrecorded, so the
/// mark reads as none until the new start is reached again.
#[test]
fn a_changed_start_height_voids_the_watermark() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open_spends(&dir, Some(11));
        db.apply(
            &[
                blk_spending(11, vec![spend(2, 1, 0x11, 0)]),
                blk_spending(12, vec![spend(3, 0, 0x12, 0)]),
            ],
            0,
            Durability::Durable,
            true,
        )
        .unwrap();
        assert_eq!(db.spends_watermark(), Some(12));
    }
    assert_eq!(open_spends(&dir, Some(5)).spends_watermark(), None);
    assert_eq!(open_spends(&dir, None).spends_watermark(), None);
    assert_eq!(open_spends(&dir, Some(11)).spends_watermark(), Some(12));
}

/// Rolling back a block recorded past a gap (the mark below it) deletes its
/// rows and leaves the mark where it was.
#[test]
fn rollback_past_a_gap_deletes_rows_and_keeps_the_mark() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_spends(&dir, Some(11));
    let r11 = db
        .apply(&[blk_spending(11, vec![spend(2, 1, 0x11, 0)])], 0, Durability::Durable, true)
        .unwrap();
    let r13 = db
        .apply(&[blk_spending(13, vec![spend(3, 0, 0x13, 0)])], 0, Durability::Durable, true)
        .unwrap();
    assert_eq!(db.spends_watermark(), Some(11));
    assert!(db.spend_of(&op(3, 0)).unwrap().is_some());
    db.rollback(13, &r13[0], Some((11, r11[0].hash))).unwrap();
    assert_eq!(db.spend_of(&op(3, 0)).unwrap(), None);
    assert!(db.spend_of(&op(2, 1)).unwrap().is_some());
    assert_eq!(db.spends_watermark(), Some(11));
}

/// Rolling back the start block itself leaves no mark.
#[test]
fn rollback_of_the_start_block_clears_the_mark() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_spends(&dir, Some(11));
    let r11 = db
        .apply(&[blk_spending(11, vec![spend(2, 1, 0x11, 0)])], 0, Durability::Durable, true)
        .unwrap();
    db.rollback(11, &r11[0], None).unwrap();
    assert_eq!(db.spends_watermark(), None);
    assert_eq!(db.spend_of(&op(2, 1)).unwrap(), None);
}

/// A backfill batch is written only when the watermark is still what it was
/// planned from and every block is still the index's block at its height —
/// so a block applied, or rolled back, while it was being fetched never
/// leaves the mark claiming heights it doesn't cover.
#[test]
fn a_backfill_batch_is_refused_when_the_index_moved() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open_spends(&dir, None);
        db.apply(&(10..=14).map(|h| blk(h, vec![])).collect::<Vec<_>>(), 0, Durability::Durable, true)
            .unwrap();
    }
    let db = open_spends(&dir, Some(10));
    let batch = |hs: std::ops::RangeInclusive<u32>| -> Vec<_> {
        hs.map(|h| (h, hash(h), vec![spend(h as u8, 0, 0x40 + h as u8, 0)])).collect()
    };
    // Planned from another watermark: refused.
    assert_eq!(
        db.write_backfill(Some(11), &batch(12..=13)).unwrap(),
        Err(BackfillRefused::MarkMoved { planned: Some(11), now: None })
    );
    // A block that isn't the index's (reorged, or not yet indexed): refused.
    let mut wrong = batch(10..=11);
    wrong[1].1 = hash(99);
    assert_eq!(
        db.write_backfill(None, &wrong).unwrap(),
        Err(BackfillRefused::NotIndexed { height: 11, node: hash(99), index: Some(hash(11)) })
    );
    assert_eq!(
        db.write_backfill(None, &batch(10..=15)).unwrap(),
        Err(BackfillRefused::NotIndexed { height: 15, node: hash(15), index: None })
    );
    assert_eq!(db.spends_watermark(), None);
    assert_eq!(db.spend_of(&op(10, 0)).unwrap(), None, "nothing written");
    // As planned: written, mark moved.
    assert_eq!(db.write_backfill(None, &batch(10..=12)).unwrap(), Ok(()));
    assert_eq!(db.spends_watermark(), Some(12));
    assert_eq!(db.spend_of(&op(11, 0)).unwrap(), Some(SpendRec { txid: txid(0x4b), vin: 0, height: 11 }));
    // The same plan again (another writer got there first): refused.
    assert!(db.write_backfill(None, &batch(10..=12)).unwrap().is_err());
    // Rolled back below the mark: the old plan is refused, the new one fits.
    let tip = db.load_blocks().unwrap();
    let (r13, r14) = (tip[3].1, tip[4].1);
    db.rollback(14, &r14, Some((13, r13.hash))).unwrap();
    db.rollback(13, &r13, Some((12, tip[2].1.hash))).unwrap();
    assert!(matches!(
        db.write_backfill(Some(12), &batch(13..=14)).unwrap(),
        Err(BackfillRefused::NotIndexed { height: 13, index: None, .. })
    ));
    assert_eq!(db.spends_watermark(), Some(12));
}
