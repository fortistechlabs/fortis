use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

fn store() -> Store {
    let dir = std::env::temp_dir().join(format!(
        "fortis-index-test-{}-{}",
        std::process::id(),
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    ));
    Store::open(dir.to_str().unwrap()).unwrap()
}

/// Pads a short, readable seed out to a real txid's length (32 bytes / 64
/// hex chars) -- still valid hex as long as the seed itself only uses hex
/// digits, which is all these tests need.
fn txid(seed: &str) -> String {
    format!("{seed:0>64}")
}

/// Same idea, padded to a real P2WPKH spk's length (22 bytes / 44 hex
/// chars) -- the encoding this store's keys require, unlike the old
/// SQLite version's free-form TEXT column.
fn spk(seed: &str) -> String {
    format!("{seed:0>44}")
}

fn tx(txid_seed: &str, ins: &[(&str, u32)], outs: &[(&str, u64)]) -> IndexedTx {
    IndexedTx {
        txid: txid(txid_seed),
        inputs: ins.iter().map(|(t, v)| TxIn { txid: txid(t), vout: *v }).collect(),
        outputs: outs
            .iter()
            .enumerate()
            .map(|(i, (s, v))| TxOut { vout: i as u32, spk_hex: spk(s), value_sat: *v })
            .collect(),
    }
}

#[test]
fn applies_outputs_and_tracks_the_tip() {
    let mut s = store();
    s.apply_block(100, &txid("100"), &[tx("aa", &[], &[("a0", 500), ("b0", 300)])]).unwrap();
    assert_eq!(s.tip().unwrap(), Some((100, txid("100"))));
    let us = s.utxos_for(&spk("a0")).unwrap();
    assert_eq!(us.len(), 1);
    assert_eq!(us[0].value_sat, 500);
    assert_eq!(us[0].height, 100);
}

#[test]
fn spending_an_output_removes_it_from_the_utxo_set_and_records_sender_history() {
    let mut s = store();
    s.apply_block(100, &txid("100"), &[tx("aa", &[], &[("a0", 500)])]).unwrap();
    s.apply_block(101, &txid("101"), &[tx("bb", &[("aa", 0)], &[("c0", 480)])]).unwrap();

    assert!(s.utxos_for(&spk("a0")).unwrap().is_empty()); // spent
    assert_eq!(s.utxos_for(&spk("c0")).unwrap().len(), 1);

    // spkA's history has both the funding tx and the spend
    let h = s.history_for(&spk("a0"), 10).unwrap();
    let ids: Vec<_> = h.iter().map(|x| x.txid.clone()).collect();
    assert!(ids.contains(&txid("aa")) && ids.contains(&txid("bb")));
}

#[test]
fn apply_blocks_batches_several_blocks_in_one_transaction() {
    // The sync loop's actual production path: a whole fetched round applied
    // as one call, including a spend (block 101) of an output created
    // earlier in the *same* batch (block 100) -- exercises the pending-
    // writes lookup working across blocks that haven't been committed yet,
    // unlike every other test here. This is the exact case a naive
    // WriteBatch port would silently get wrong (see store.rs's doc
    // comment on `apply_blocks`).
    let mut s = store();
    let b100 = tx("aa", &[], &[("a0", 500)]);
    let b101 = tx("bb", &[("aa", 0)], &[("c0", 480)]);
    let b102 = tx("cc", &[], &[("d0", 10)]);
    s.apply_blocks(&[
        (100, &txid("100"), std::slice::from_ref(&b100)),
        (101, &txid("101"), std::slice::from_ref(&b101)),
        (102, &txid("102"), std::slice::from_ref(&b102)),
    ])
    .unwrap();

    assert_eq!(s.tip().unwrap(), Some((102, txid("102"))));
    assert!(s.utxos_for(&spk("a0")).unwrap().is_empty()); // spent within the same batch
    assert_eq!(s.utxos_for(&spk("c0")).unwrap().len(), 1);
    assert_eq!(s.utxos_for(&spk("d0")).unwrap().len(), 1);
    let h = s.history_for(&spk("a0"), 10).unwrap();
    let ids: Vec<_> = h.iter().map(|x| x.txid.clone()).collect();
    assert!(ids.contains(&txid("aa")) && ids.contains(&txid("bb")));
}

#[test]
fn apply_blocks_with_an_empty_slice_is_a_harmless_no_op() {
    let mut s = store();
    s.apply_blocks(&[]).unwrap();
    assert_eq!(s.tip().unwrap(), None);
}

#[test]
fn rollback_undoes_blocks_and_unspends() {
    let mut s = store();
    s.apply_block(100, &txid("100"), &[tx("aa", &[], &[("a0", 500)])]).unwrap();
    s.apply_block(101, &txid("101"), &[tx("bb", &[("aa", 0)], &[("c0", 480)])]).unwrap();

    s.rollback_from(101).unwrap();

    assert_eq!(s.tip().unwrap(), Some((100, txid("100"))));
    // the spend at 101 is undone: spkA's coin is spendable again
    assert_eq!(s.utxos_for(&spk("a0")).unwrap().len(), 1);
    // and everything created at 101 is gone
    assert!(s.utxos_for(&spk("c0")).unwrap().is_empty());
    assert!(s.history_for(&spk("c0"), 10).unwrap().is_empty());
    let h = s.history_for(&spk("a0"), 10).unwrap();
    assert_eq!(h.iter().map(|x| x.txid.clone()).collect::<Vec<_>>(), vec![txid("aa")]);
}

#[test]
fn rollback_to_start_clears_everything() {
    let mut s = store();
    s.apply_block(100, &txid("100"), &[tx("aa", &[], &[("a0", 1)])]).unwrap();
    s.apply_block(101, &txid("101"), &[tx("bb", &[], &[("b0", 1)])]).unwrap();
    s.rollback_from(100).unwrap();
    assert_eq!(s.tip().unwrap(), None);
    assert!(s.utxos_for(&spk("a0")).unwrap().is_empty());
}

#[test]
fn history_is_newest_first_and_capped() {
    let mut s = store();
    for h in 100..110 {
        s.apply_block(h, &txid(&format!("{h:x}")), &[tx(&format!("{h:x}1"), &[], &[("f0", 10)])]).unwrap();
    }
    let rows = s.history_for(&spk("f0"), 3).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].height, 109);
    assert_eq!(rows[0].block_hash, txid("6d")); // 109 in hex
    assert!(rows[0].height > rows[1].height);
}

#[test]
fn a_spend_and_creation_within_the_same_rolled_back_range_leaves_no_trace() {
    // A coin created at 100 and spent at 101, with the whole range rolled
    // back from 100: this must fully vanish (no phantom utxo_by_spk entry,
    // no dangling spent-marker), not just "come back unspent."
    let mut s = store();
    s.apply_block(100, &txid("100"), &[tx("aa", &[], &[("a0", 500)])]).unwrap();
    s.apply_block(101, &txid("101"), &[tx("bb", &[("aa", 0)], &[("c0", 480)])]).unwrap();

    s.rollback_from(100).unwrap();

    assert_eq!(s.tip().unwrap(), None);
    assert!(s.utxos_for(&spk("a0")).unwrap().is_empty());
    assert!(s.utxos_for(&spk("c0")).unwrap().is_empty());
    assert!(s.history_for(&spk("a0"), 10).unwrap().is_empty());
    assert!(s.output_at(&txid("aa"), 0).unwrap().is_none());
}

#[test]
fn rollback_past_migrated_data_with_no_undo_log_is_refused_not_silently_wrong() {
    // Simulates data written by something other than apply_blocks (e.g. a
    // migration) that never logged an undo record for its height -- a
    // rollback reaching that far back must error, not silently delete
    // `blocks` entries it has no way to correctly undo.
    let mut s = store();
    // Write a block "by hand" the way a migration would: no undo_log entry.
    {
        let cf_blocks = s.cf(CF_BLOCKS);
        let mut batch = WriteBatch::default();
        batch.put_cf(&cf_blocks, blocks_key(100), hex::decode(txid("100")).unwrap());
        s.db.write(batch).unwrap();
    }
    s.apply_block(101, &txid("101"), &[tx("bb", &[], &[("b0", 1)])]).unwrap();

    let err = s.rollback_from(100).unwrap_err();
    assert!(err.to_string().contains("no undo-log entry"), "unexpected error: {err}");
}
