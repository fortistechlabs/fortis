use super::*;

fn store() -> Store {
    Store::open(":memory:").unwrap()
}

fn tx(txid: &str, ins: &[(&str, u32)], outs: &[(&str, u64)]) -> IndexedTx {
    IndexedTx {
        txid: txid.into(),
        inputs: ins.iter().map(|(t, v)| TxIn { txid: (*t).into(), vout: *v }).collect(),
        outputs: outs
            .iter()
            .enumerate()
            .map(|(i, (s, v))| TxOut { vout: i as u32, spk_hex: (*s).into(), value_sat: *v })
            .collect(),
    }
}

#[test]
fn applies_outputs_and_tracks_the_tip() {
    let mut s = store();
    s.apply_block(100, "h100", &[tx("aa", &[], &[("spkA", 500), ("spkB", 300)])]).unwrap();
    assert_eq!(s.tip().unwrap(), Some((100, "h100".into())));
    let us = s.utxos_for("spkA").unwrap();
    assert_eq!(us.len(), 1);
    assert_eq!(us[0].value_sat, 500);
    assert_eq!(us[0].height, 100);
}

#[test]
fn spending_an_output_removes_it_from_the_utxo_set_and_records_sender_history() {
    let mut s = store();
    s.apply_block(100, "h100", &[tx("aa", &[], &[("spkA", 500)])]).unwrap();
    s.apply_block(101, "h101", &[tx("bb", &[("aa", 0)], &[("spkC", 480)])]).unwrap();

    assert!(s.utxos_for("spkA").unwrap().is_empty()); // spent
    assert_eq!(s.utxos_for("spkC").unwrap().len(), 1);

    // spkA's history has both the funding tx and the spend
    let h = s.history_for("spkA", 10).unwrap();
    let ids: Vec<_> = h.iter().map(|x| x.txid.as_str()).collect();
    assert!(ids.contains(&"aa") && ids.contains(&"bb"));
}

#[test]
fn apply_blocks_batches_several_blocks_in_one_transaction() {
    // The sync loop's actual production path: a whole fetched round applied
    // as one call, including a spend (block 101) of an output created
    // earlier in the *same* batch (block 100) — exercises spk_of's lookup
    // working across blocks that haven't been through separate apply_block
    // transactions, unlike every other test here.
    let mut s = store();
    let b100 = tx("aa", &[], &[("spkA", 500)]);
    let b101 = tx("bb", &[("aa", 0)], &[("spkC", 480)]);
    let b102 = tx("cc", &[], &[("spkD", 10)]);
    s.apply_blocks(&[
        (100, "h100", std::slice::from_ref(&b100)),
        (101, "h101", std::slice::from_ref(&b101)),
        (102, "h102", std::slice::from_ref(&b102)),
    ])
    .unwrap();

    assert_eq!(s.tip().unwrap(), Some((102, "h102".into())));
    assert!(s.utxos_for("spkA").unwrap().is_empty()); // spent within the same batch
    assert_eq!(s.utxos_for("spkC").unwrap().len(), 1);
    assert_eq!(s.utxos_for("spkD").unwrap().len(), 1);
    let h = s.history_for("spkA", 10).unwrap();
    let ids: Vec<_> = h.iter().map(|x| x.txid.as_str()).collect();
    assert!(ids.contains(&"aa") && ids.contains(&"bb"));
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
    s.apply_block(100, "h100", &[tx("aa", &[], &[("spkA", 500)])]).unwrap();
    s.apply_block(101, "h101", &[tx("bb", &[("aa", 0)], &[("spkC", 480)])]).unwrap();

    s.rollback_from(101).unwrap();

    assert_eq!(s.tip().unwrap(), Some((100, "h100".into())));
    // the spend at 101 is undone: spkA's coin is spendable again
    assert_eq!(s.utxos_for("spkA").unwrap().len(), 1);
    // and everything created at 101 is gone
    assert!(s.utxos_for("spkC").unwrap().is_empty());
    assert!(s.history_for("spkC", 10).unwrap().is_empty());
    let h = s.history_for("spkA", 10).unwrap();
    assert_eq!(h.iter().map(|x| x.txid.as_str()).collect::<Vec<_>>(), vec!["aa"]);
}

#[test]
fn rollback_to_start_clears_everything() {
    let mut s = store();
    s.apply_block(100, "h100", &[tx("aa", &[], &[("spkA", 1)])]).unwrap();
    s.apply_block(101, "h101", &[tx("bb", &[], &[("spkB", 1)])]).unwrap();
    s.rollback_from(100).unwrap();
    assert_eq!(s.tip().unwrap(), None);
    assert!(s.utxos_for("spkA").unwrap().is_empty());
}

#[test]
fn history_is_newest_first_and_capped() {
    let mut s = store();
    for h in 100..110 {
        s.apply_block(h, &format!("h{h}"), &[tx(&format!("t{h}"), &[], &[("spkX", 10)])]).unwrap();
    }
    let rows = s.history_for("spkX", 3).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].height, 109);
    assert_eq!(rows[0].block_hash, "h109");
    assert!(rows[0].height > rows[1].height);
}
