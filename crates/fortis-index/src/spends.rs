//! Who spent one output — for the Esplora routes swaps need
//! (`GET /tx/:txid`, `/tx/:txid/hex`, `/tx/:txid/outspend/:vout`). A swap's
//! HTLC is P2WSH, which the address index never stores, so its spend is found
//! from the node: the mempool (or the node's own spender index, when it has
//! one), else by scanning blocks forward from the one that funded it.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use anyhow::Result;
use bitcoin::{BlockHash, OutPoint, Transaction, Txid};
use serde_json::{json, Value};

/// Most blocks one output's spend is searched for past its funding block. A
/// swap HTLC is spent within its locktime — 48 h, about 288 blocks, at most.
pub const MAX_SCAN_BLOCKS: u32 = 2016;
/// Most blocks one request scans; the rest is answered "still searching" and
/// the next request resumes where this one stopped.
pub const SCAN_BUDGET: u32 = 50;
/// Searches running at once, across all outputs: each fetches whole blocks
/// from the node, so a burst of requests must not starve it.
pub const MAX_CONCURRENT_SCANS: usize = 2;
/// Outputs remembered (spenders, found spends, scan progress). When full, scan
/// progress goes first: found spenders are what live swaps keep asking for.
const CACHE_MAX: usize = 10_000;

/// The node calls a spend lookup needs.
pub trait SpendNode: Send + Sync {
    /// `getrawtransaction <txid> 1`; `None` if the node doesn't know it.
    fn tx(&self, txid: &Txid) -> Result<Option<Value>>;
    /// `getrawtransaction <txid> 0` (hex); `None` if the node doesn't know it.
    fn raw_tx(&self, txid: &Txid) -> Result<Option<String>>;
    /// In the UTXO set, mempool included (a mempool spend makes it `false`).
    fn unspent(&self, op: &OutPoint) -> Result<bool>;
    /// A spender the node knows directly: from its mempool, or from its own
    /// spender index (`-txospenderindex`) when it has one.
    fn known_spender(&self, op: &OutPoint) -> Result<Option<Txid>>;
    /// The main-chain height of `hash`; `None` if it isn't on the main chain.
    fn height_of(&self, hash: &BlockHash) -> Result<Option<u32>>;
    fn tip(&self) -> Result<u32>;
    /// The main-chain block at `height`: its hash and transactions.
    fn block(&self, height: u32) -> Result<(BlockHash, Vec<Transaction>)>;
}

/// An output's spend, in the shape `GET /tx/:txid/outspend/:vout` answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outspend {
    Unspent,
    /// `block`: `(hash, height)` once mined; `None` while in the mempool.
    Spent {
        txid: Txid,
        vin: u32,
        block: Option<(BlockHash, u32)>,
    },
}

impl Outspend {
    /// Esplora's outspend JSON.
    pub fn json(&self) -> Value {
        match self {
            Outspend::Unspent => json!({ "spent": false }),
            Outspend::Spent { txid, vin, block } => json!({
                "spent": true,
                "txid": txid.to_string(),
                "vin": vin,
                "status": match block {
                    Some((hash, height)) => json!({
                        "confirmed": true,
                        "block_height": height,
                        "block_hash": hash.to_string(),
                    }),
                    None => json!({ "confirmed": false }),
                },
            }),
        }
    }
}

#[derive(Debug)]
pub enum OutspendError {
    /// No such transaction, or no such output in it.
    UnknownOutput,
    /// Not answered yet — still searching, too busy, or its funding isn't
    /// mined: ask again shortly.
    Pending(String),
    /// Spent, but outside the window this index searches: the funding is more
    /// than [`MAX_SCAN_BLOCKS`] blocks old, or no spend turned up within it.
    /// Final: asking again won't change it.
    OutOfRange(String),
    Node(anyhow::Error),
}

impl From<anyhow::Error> for OutspendError {
    fn from(e: anyhow::Error) -> Self {
        OutspendError::Node(e)
    }
}

#[derive(Clone)]
enum Cached {
    /// A spender the node named (seen in its mempool): once it is mined, its
    /// own blockhash says where — no scan.
    Spender(Txid),
    /// A spend found in a block (rechecked against reorgs on every read).
    Found(Outspend),
    /// Blocks up to and including `through` (hash `hash`) hold no spend.
    Scanned { through: u32, hash: BlockHash },
    /// Searched the whole window without finding it.
    Exhausted { last: u32 },
}

/// Spend lookups over a [`SpendNode`], remembering what each search learned.
pub struct Spends {
    node: Box<dyn SpendNode>,
    cache: Mutex<HashMap<OutPoint, Cached>>,
    /// Outputs being searched right now (one search per output at a time).
    scanning: Mutex<HashSet<OutPoint>>,
}

/// An output's search slot; released when dropped.
struct ScanSlot<'a> {
    spends: &'a Spends,
    op: OutPoint,
}

impl Drop for ScanSlot<'_> {
    fn drop(&mut self) {
        self.spends.scanning.lock().unwrap().remove(&self.op);
    }
}

impl Spends {
    pub fn new(node: Box<dyn SpendNode>) -> Self {
        Spends {
            node,
            cache: Mutex::new(HashMap::new()),
            scanning: Mutex::new(HashSet::new()),
        }
    }

    /// Claim `op`'s search slot, or say why not (already searched, or too many
    /// searches running). Refused at once — never queued — so a burst of
    /// requests can't pile up threads waiting on the node.
    fn claim(&self, op: OutPoint) -> Result<ScanSlot<'_>, OutspendError> {
        let mut running = self.scanning.lock().unwrap();
        if running.contains(&op) {
            return Err(OutspendError::Pending(format!("{op} is being searched; ask again shortly")));
        }
        if running.len() >= MAX_CONCURRENT_SCANS {
            return Err(OutspendError::Pending("too many searches running; ask again shortly".into()));
        }
        running.insert(op);
        Ok(ScanSlot { spends: self, op })
    }

    pub fn node(&self) -> &dyn SpendNode {
        &*self.node
    }

    /// Who spent `txid:vout`, if anyone.
    pub fn outspend(&self, txid: &Txid, vout: u32) -> Result<Outspend, OutspendError> {
        let funding = self.node.tx(txid)?.ok_or(OutspendError::UnknownOutput)?;
        let Some(out) = funding["vout"].as_array().and_then(|v| v.get(vout as usize)) else {
            return Err(OutspendError::UnknownOutput);
        };
        // Provably unspendable: never spent (Esplora says the same).
        if out["scriptPubKey"]["type"] == "nulldata" {
            return Ok(Outspend::Unspent);
        }
        let op = OutPoint::new(*txid, vout);
        if self.node.unspent(&op)? {
            return Ok(Outspend::Unspent);
        }
        if let Some(spend) = self.node.known_spender(&op)? {
            let found = self.spent_by(&spend, &op)?;
            self.remember(op, Cached::Spender(spend));
            return Ok(found);
        }

        // Spent, and not in the mempool. What earlier lookups learned still
        // counts if its block is still on the main chain.
        let mut from = None;
        let cached = self.cache.lock().unwrap().get(&op).cloned();
        match cached {
            Some(Cached::Spender(spend)) => {
                // Mined since (its tx names its block), or replaced (gone): scan.
                if let Ok(found @ Outspend::Spent { block: Some(_), .. }) = self.spent_by(&spend, &op) {
                    self.remember(op, Cached::Found(found.clone()));
                    return Ok(found);
                }
            }
            Some(Cached::Found(found)) => {
                if let Outspend::Spent { block: Some((hash, height)), .. } = &found {
                    if self.node.height_of(hash)? == Some(*height) {
                        return Ok(found);
                    }
                }
            }
            Some(Cached::Scanned { through, hash }) if self.node.height_of(&hash)? == Some(through) => {
                from = Some(through + 1);
            }
            Some(Cached::Exhausted { last }) => {
                return Err(OutspendError::OutOfRange(format!(
                    "{op} is spent, but not within {MAX_SCAN_BLOCKS} blocks of its funding (searched to {last})"
                )));
            }
            Some(Cached::Scanned { .. }) | None => {}
        }

        let funded_at = match funding["blockhash"].as_str().map(str::parse::<BlockHash>) {
            Some(Ok(hash)) => self.node.height_of(&hash)?,
            _ => None,
        }
        .ok_or_else(|| OutspendError::Pending(format!("{op} is spent but its funding isn't mined")))?;
        let tip = self.node.tip()?;
        let last = tip.min(funded_at + MAX_SCAN_BLOCKS);
        let from = from.unwrap_or(funded_at);
        // Older than any swap and never searched: refuse rather than scan.
        if from == funded_at && tip > funded_at + MAX_SCAN_BLOCKS {
            return Err(OutspendError::OutOfRange(format!(
                "{op}'s funding is more than {MAX_SCAN_BLOCKS} blocks old; this index only searches newer outputs"
            )));
        }

        let _slot = self.claim(op)?;
        let stop = last.min(from.saturating_add(SCAN_BUDGET - 1));
        for height in from..=stop {
            let (hash, txs) = self.node.block(height)?;
            for t in &txs {
                if let Some(vin) = t.input.iter().position(|i| i.previous_output == op) {
                    let found = Outspend::Spent {
                        txid: t.compute_txid(),
                        vin: vin as u32,
                        block: Some((hash, height)),
                    };
                    self.remember(op, Cached::Found(found.clone()));
                    return Ok(found);
                }
            }
            self.remember(op, Cached::Scanned { through: height, hash });
        }
        if stop < last {
            return Err(OutspendError::Pending(format!(
                "{op}: searched blocks {from}..={stop} of {funded_at}..={last}; ask again to continue"
            )));
        }
        if last == funded_at + MAX_SCAN_BLOCKS {
            self.remember(op, Cached::Exhausted { last });
            return Err(OutspendError::OutOfRange(format!(
                "{op} is spent, but not within {MAX_SCAN_BLOCKS} blocks of its funding"
            )));
        }
        // Up to the tip and not found: the spend is mined in a block not seen yet.
        Err(OutspendError::Pending(format!("{op} is spent, but not in blocks {funded_at}..={last} yet")))
    }

    /// [`Outspend::Spent`] for a spender the node named directly.
    fn spent_by(&self, spend: &Txid, op: &OutPoint) -> Result<Outspend, OutspendError> {
        let t = self
            .node
            .tx(spend)?
            .ok_or_else(|| OutspendError::Pending(format!("spender {spend} not found")))?;
        let vin = t["vin"]
            .as_array()
            .and_then(|ins| {
                ins.iter().position(|i| {
                    i["txid"].as_str() == Some(&op.txid.to_string()) && i["vout"].as_u64() == Some(op.vout as u64)
                })
            })
            .ok_or_else(|| OutspendError::Pending(format!("{spend} doesn't spend {op}")))?;
        let block = match t["blockhash"].as_str().map(str::parse::<BlockHash>) {
            Some(Ok(hash)) => self.node.height_of(&hash)?.map(|h| (hash, h)),
            _ => None,
        };
        Ok(Outspend::Spent {
            txid: *spend,
            vin: vin as u32,
            block,
        })
    }

    fn remember(&self, op: OutPoint, c: Cached) {
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= CACHE_MAX && !cache.contains_key(&op) {
            // Scan progress first; spenders and found spends are kept unless
            // they alone fill it.
            cache.retain(|_, v| matches!(v, Cached::Spender(_) | Cached::Found(_)));
            if cache.len() >= CACHE_MAX {
                cache.clear();
            }
        }
        cache.insert(op, c);
    }
}

/// [`SpendNode`] over the node's JSON-RPC (needs `-txindex`).
pub struct RpcSpendNode {
    pub rpc: fortis_node::Rpc,
    pub fmt: crate::chain::HeaderFormat,
}

/// "No such mempool or blockchain transaction" / "Block not found".
fn not_found(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("error -5")
}

impl SpendNode for RpcSpendNode {
    fn tx(&self, txid: &Txid) -> Result<Option<Value>> {
        match self.rpc.call("getrawtransaction", json!([txid.to_string(), 1])) {
            Ok(v) => Ok(Some(v)),
            Err(e) if not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn raw_tx(&self, txid: &Txid) -> Result<Option<String>> {
        match self.rpc.call("getrawtransaction", json!([txid.to_string(), 0])) {
            Ok(v) => Ok(v.as_str().map(str::to_string)),
            Err(e) if not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn unspent(&self, op: &OutPoint) -> Result<bool> {
        let v = self.rpc.call("gettxout", json!([op.txid.to_string(), op.vout, true]))?;
        Ok(!v.is_null())
    }

    fn known_spender(&self, op: &OutPoint) -> Result<Option<Txid>> {
        // Mempool only, unless the node keeps `-txospenderindex` (Core 30+),
        // when it answers mined spends too. A node without the RPC: none known.
        let outputs = json!([{ "txid": op.txid.to_string(), "vout": op.vout }]);
        let v = match self.rpc.call("gettxspendingprevout", json!([outputs])) {
            Ok(v) => v,
            Err(e) if format!("{e:#}").contains("error -32601") => return Ok(None),
            // The spender index can refuse while it is still being built: ask
            // the mempool alone then (the block scan covers mined spends).
            Err(_) => self
                .rpc
                .call("gettxspendingprevout", json!([outputs, { "mempool_only": true }]))?,
        };
        Ok(v[0]["spendingtxid"].as_str().and_then(|s| s.parse().ok()))
    }

    fn height_of(&self, hash: &BlockHash) -> Result<Option<u32>> {
        match self.rpc.call("getblockheader", json!([hash.to_string(), true])) {
            // confirmations -1: a block off the main chain
            Ok(v) if v["confirmations"].as_i64().unwrap_or(-1) >= 0 => {
                Ok(v["height"].as_u64().map(|h| h as u32))
            }
            Ok(_) => Ok(None),
            Err(e) if not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn tip(&self) -> Result<u32> {
        let v = self.rpc.call("getblockcount", json!([]))?;
        v.as_u64()
            .map(|h| h as u32)
            .ok_or_else(|| anyhow::anyhow!("getblockcount: {v}"))
    }

    fn block(&self, height: u32) -> Result<(BlockHash, Vec<Transaction>)> {
        let hash: BlockHash = self
            .rpc
            .call("getblockhash", json!([height]))?
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("getblockhash {height}: not a string"))?
            .parse()?;
        let hex = self.rpc.call("getblock", json!([hash.to_string(), 0]))?;
        let bytes = hex::decode(hex.as_str().unwrap_or_default())?;
        let body = crate::chain::parse_block(&bytes, self.fmt.header_len(height))?;
        Ok((hash, body.txs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::absolute::LockTime;
    use bitcoin::hashes::Hash;
    use bitcoin::transaction::Version;
    use bitcoin::{Amount, ScriptBuf, Sequence, TxIn, TxOut, Witness};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn bh(n: u32) -> BlockHash {
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(&n.to_be_bytes());
        BlockHash::from_byte_array(b)
    }

    /// A tx spending each of `ins`, with one output (`tag` makes txids unique).
    fn tx(ins: &[OutPoint], tag: u64) -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: ins
                .iter()
                .map(|op| TxIn {
                    previous_output: *op,
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                })
                .collect(),
            output: vec![TxOut {
                value: Amount::from_sat(tag),
                script_pubkey: ScriptBuf::new(),
            }],
        }
    }

    /// A main chain of blocks from `base`, plus what the node says about the
    /// UTXO set and its mempool. Counts block fetches.
    #[derive(Default)]
    struct Chain {
        base: u32,
        blocks: Vec<(BlockHash, Vec<Transaction>)>,
        /// Txids the node knows, with the block holding them (None = mempool).
        known: HashMap<Txid, (Transaction, Option<BlockHash>)>,
        utxos: Vec<OutPoint>,
        mempool_spends: HashMap<OutPoint, Txid>,
        stale: Vec<BlockHash>,
    }

    struct Fake {
        chain: Mutex<Chain>,
        fetches: AtomicUsize,
        /// While true, `block()` waits (a scan held mid-way); `inside` counts waiters.
        hold: Mutex<bool>,
        released: std::sync::Condvar,
        inside: AtomicUsize,
    }

    impl Fake {
        fn fetches(&self) -> usize {
            self.fetches.load(Ordering::SeqCst)
        }
    }

    /// Lets a test keep its [`Fake`] while [`Spends`] owns a box of it.
    impl<T: SpendNode + ?Sized> SpendNode for Arc<T> {
        fn tx(&self, txid: &Txid) -> Result<Option<Value>> {
            (**self).tx(txid)
        }
        fn raw_tx(&self, txid: &Txid) -> Result<Option<String>> {
            (**self).raw_tx(txid)
        }
        fn unspent(&self, op: &OutPoint) -> Result<bool> {
            (**self).unspent(op)
        }
        fn known_spender(&self, op: &OutPoint) -> Result<Option<Txid>> {
            (**self).known_spender(op)
        }
        fn height_of(&self, hash: &BlockHash) -> Result<Option<u32>> {
            (**self).height_of(hash)
        }
        fn tip(&self) -> Result<u32> {
            (**self).tip()
        }
        fn block(&self, height: u32) -> Result<(BlockHash, Vec<Transaction>)> {
            (**self).block(height)
        }
    }

    fn spends(c: Chain) -> (Spends, Arc<Fake>) {
        let fake = Arc::new(Fake {
            chain: Mutex::new(c),
            fetches: AtomicUsize::new(0),
            hold: Mutex::new(false),
            released: std::sync::Condvar::new(),
            inside: AtomicUsize::new(0),
        });
        (Spends::new(Box::new(fake.clone())), fake)
    }

    impl SpendNode for Fake {
        fn tx(&self, txid: &Txid) -> Result<Option<Value>> {
            let c = self.chain.lock().unwrap();
            Ok(c.known.get(txid).map(|(t, b)| {
                json!({
                    "txid": txid.to_string(),
                    "vin": t.input.iter().map(|i| json!({
                        "txid": i.previous_output.txid.to_string(),
                        "vout": i.previous_output.vout,
                    })).collect::<Vec<_>>(),
                    "vout": t.output.iter().map(|o| json!({ "scriptPubKey": {
                        "type": if o.script_pubkey.is_op_return() { "nulldata" } else { "witness_v0_scripthash" },
                    }})).collect::<Vec<_>>(),
                    "blockhash": b.map(|h| h.to_string()),
                })
            }))
        }
        fn raw_tx(&self, txid: &Txid) -> Result<Option<String>> {
            let c = self.chain.lock().unwrap();
            Ok(c.known.get(txid).map(|(t, _)| bitcoin::consensus::encode::serialize_hex(t)))
        }
        fn unspent(&self, op: &OutPoint) -> Result<bool> {
            Ok(self.chain.lock().unwrap().utxos.contains(op))
        }
        fn known_spender(&self, op: &OutPoint) -> Result<Option<Txid>> {
            Ok(self.chain.lock().unwrap().mempool_spends.get(op).copied())
        }
        fn height_of(&self, hash: &BlockHash) -> Result<Option<u32>> {
            let c = self.chain.lock().unwrap();
            if c.stale.contains(hash) {
                return Ok(None);
            }
            Ok(c.blocks
                .iter()
                .position(|(h, _)| h == hash)
                .map(|i| c.base + i as u32))
        }
        fn tip(&self) -> Result<u32> {
            let c = self.chain.lock().unwrap();
            Ok(c.base + c.blocks.len() as u32 - 1)
        }
        fn block(&self, height: u32) -> Result<(BlockHash, Vec<Transaction>)> {
            {
                let mut held = self.hold.lock().unwrap();
                if *held {
                    self.inside.fetch_add(1, Ordering::SeqCst);
                    while *held {
                        held = self.released.wait(held).unwrap();
                    }
                }
            }
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let c = self.chain.lock().unwrap();
            Ok(c.blocks[(height - c.base) as usize].clone())
        }
    }

    /// Block 100 funds `op` (vout 0 of `funding`); blocks 101..=105 follow.
    fn setup() -> (Chain, Transaction, OutPoint) {
        let funding = tx(&[OutPoint::new(Txid::from_byte_array([9; 32]), 0)], 1);
        let op = OutPoint::new(funding.compute_txid(), 0);
        let mut c = Chain {
            base: 100,
            ..Default::default()
        };
        for h in 100..=105 {
            let txs = if h == 100 { vec![funding.clone()] } else { vec![tx(&[], h as u64)] };
            c.blocks.push((bh(h), txs));
        }
        c.known.insert(op.txid, (funding.clone(), Some(bh(100))));
        (c, funding, op)
    }

    /// Mine `spend` in block `h` (replacing that block's contents).
    fn mine(c: &mut Chain, h: u32, spend: &Transaction) {
        let i = (h - c.base) as usize;
        c.blocks[i].1.push(spend.clone());
        c.known.insert(spend.compute_txid(), (spend.clone(), Some(c.blocks[i].0)));
    }

    #[test]
    fn an_unspent_output_says_so() {
        let (mut c, _, op) = setup();
        c.utxos.push(op);
        let (s, fake) = spends(c);
        assert_eq!(s.outspend(&op.txid, 0).unwrap(), Outspend::Unspent);
        assert_eq!(fake.fetches(), 0, "no block scan for an unspent output");
    }

    #[test]
    fn an_unknown_tx_or_output_is_unknown() {
        let (c, _, op) = setup();
        let (s, _) = spends(c);
        assert!(matches!(
            s.outspend(&Txid::from_byte_array([7; 32]), 0),
            Err(OutspendError::UnknownOutput)
        ));
        assert!(matches!(s.outspend(&op.txid, 1), Err(OutspendError::UnknownOutput)));
    }

    #[test]
    fn a_mempool_spend_is_reported_unconfirmed_with_its_input_index() {
        let (mut c, _, op) = setup();
        let other = OutPoint::new(Txid::from_byte_array([5; 32]), 2);
        let spend = tx(&[other, op], 77); // our output is input 1
        c.known.insert(spend.compute_txid(), (spend.clone(), None));
        c.mempool_spends.insert(op, spend.compute_txid());
        let (s, fake) = spends(c);
        assert_eq!(
            s.outspend(&op.txid, 0).unwrap(),
            Outspend::Spent { txid: spend.compute_txid(), vin: 1, block: None }
        );
        assert_eq!(fake.fetches(), 0);
    }

    #[test]
    fn a_spend_mined_later_is_found_by_scanning_forward_from_the_funding_block() {
        let (mut c, _, op) = setup();
        let spend = tx(&[op], 42);
        mine(&mut c, 103, &spend);
        let (s, fake) = spends(c);
        let want = Outspend::Spent { txid: spend.compute_txid(), vin: 0, block: Some((bh(103), 103)) };
        assert_eq!(s.outspend(&op.txid, 0).unwrap(), want);
        assert_eq!(fake.fetches(), 4, "blocks 100..=103");
        // Remembered: the second lookup scans nothing.
        assert_eq!(s.outspend(&op.txid, 0).unwrap(), want);
        assert_eq!(fake.fetches(), 4);
    }

    #[test]
    fn a_spend_in_the_funding_block_itself_is_found() {
        let (mut c, _, op) = setup();
        let spend = tx(&[op], 43);
        mine(&mut c, 100, &spend);
        let (s, _) = spends(c);
        assert_eq!(
            s.outspend(&op.txid, 0).unwrap(),
            Outspend::Spent { txid: spend.compute_txid(), vin: 0, block: Some((bh(100), 100)) }
        );
    }

    #[test]
    fn the_node_s_own_spender_index_is_used_when_it_has_one() {
        let (mut c, _, op) = setup();
        let spend = tx(&[op], 44);
        mine(&mut c, 104, &spend);
        c.mempool_spends.insert(op, spend.compute_txid()); // "known_spender" answers it
        let (s, fake) = spends(c);
        assert_eq!(
            s.outspend(&op.txid, 0).unwrap(),
            Outspend::Spent { txid: spend.compute_txid(), vin: 0, block: Some((bh(104), 104)) }
        );
        assert_eq!(fake.fetches(), 0);
    }

    #[test]
    fn a_found_spend_whose_block_was_reorged_out_is_searched_again() {
        let (mut c, _, op) = setup();
        let spend = tx(&[op], 45);
        mine(&mut c, 102, &spend);
        let (s, fake) = spends(c);
        assert!(matches!(s.outspend(&op.txid, 0).unwrap(), Outspend::Spent { block: Some((_, 102)), .. }));
        // Block 102 is replaced by 102' without the spend; the spend lands in 105.
        {
            let mut c = fake.chain.lock().unwrap();
            c.stale.push(bh(102));
            c.blocks[2] = (bh(1102), vec![]);
            let i = 5;
            c.blocks[i].1.push(spend.clone());
            c.known.insert(spend.compute_txid(), (spend.clone(), Some(bh(105))));
        }
        assert_eq!(
            s.outspend(&op.txid, 0).unwrap(),
            Outspend::Spent { txid: spend.compute_txid(), vin: 0, block: Some((bh(105), 105)) }
        );
    }

    #[test]
    fn a_spend_not_found_yet_is_retried_from_where_the_scan_stopped() {
        let (c, _, op) = setup();
        let (s, fake) = spends(c); // spent (not a UTXO) but no spend mined yet
        assert!(matches!(s.outspend(&op.txid, 0), Err(OutspendError::Pending(_))));
        assert_eq!(fake.fetches(), 6, "blocks 100..=105");
        // A new block 106 carries it: only that block is fetched.
        let spend = tx(&[op], 46);
        {
            let mut c = fake.chain.lock().unwrap();
            c.blocks.push((bh(106), vec![spend.clone()]));
            c.known.insert(spend.compute_txid(), (spend.clone(), Some(bh(106))));
        }
        assert_eq!(
            s.outspend(&op.txid, 0).unwrap(),
            Outspend::Spent { txid: spend.compute_txid(), vin: 0, block: Some((bh(106), 106)) }
        );
        assert_eq!(fake.fetches(), 7);
    }

    /// Ask until the answer is final (each request scans at most SCAN_BUDGET blocks).
    fn settle(s: &Spends, op: &OutPoint) -> Result<Outspend, OutspendError> {
        for _ in 0..1_000 {
            match s.outspend(&op.txid, op.vout) {
                Err(OutspendError::Pending(_)) => continue,
                other => return other,
            }
        }
        panic!("never settled")
    }

    #[test]
    fn the_search_window_is_capped_and_then_final() {
        let (mut c, _, op) = setup();
        // The tip reaches exactly the window's end (any further and the output
        // counts as too old to search at all — see the test below).
        for h in 106..=(100 + MAX_SCAN_BLOCKS) {
            c.blocks.push((bh(h), vec![]));
        }
        let (s, fake) = spends(c);
        assert!(matches!(settle(&s, &op), Err(OutspendError::OutOfRange(_))));
        assert_eq!(fake.fetches(), MAX_SCAN_BLOCKS as usize + 1, "blocks 100..=100+MAX, once each");
        assert!(matches!(s.outspend(&op.txid, 0), Err(OutspendError::OutOfRange(_))), "and stays final");
        assert_eq!(fake.fetches(), MAX_SCAN_BLOCKS as usize + 1, "without scanning again");
    }

    #[test]
    fn each_request_scans_at_most_its_budget_and_the_next_resumes() {
        let (mut c, _, op) = setup();
        for h in 106..=220 {
            c.blocks.push((bh(h), vec![]));
        }
        let spend = tx(&[op], 47);
        mine(&mut c, 210, &spend);
        let (s, fake) = spends(c);
        assert!(matches!(s.outspend(&op.txid, 0), Err(OutspendError::Pending(_))));
        assert_eq!(fake.fetches(), SCAN_BUDGET as usize);
        assert!(matches!(s.outspend(&op.txid, 0), Err(OutspendError::Pending(_))));
        assert_eq!(fake.fetches(), 2 * SCAN_BUDGET as usize);
        assert_eq!(
            s.outspend(&op.txid, 0).unwrap(),
            Outspend::Spent { txid: spend.compute_txid(), vin: 0, block: Some((bh(210), 210)) }
        );
        assert_eq!(fake.fetches(), 111, "blocks 100..=210, each once");
    }

    #[test]
    fn an_output_older_than_any_swap_is_refused_without_scanning() {
        let (mut c, _, op) = setup();
        for h in 106..=(100 + MAX_SCAN_BLOCKS + 500) {
            c.blocks.push((bh(h), vec![]));
        }
        let (s, fake) = spends(c);
        assert!(matches!(s.outspend(&op.txid, 0), Err(OutspendError::OutOfRange(_))));
        assert_eq!(fake.fetches(), 0);
    }

    #[test]
    fn an_op_return_output_is_unspent_without_scanning() {
        let (mut c, _, _) = setup();
        let mut burn = tx(&[OutPoint::new(Txid::from_byte_array([8; 32]), 0)], 2);
        burn.output[0].script_pubkey = ScriptBuf::new_op_return([1u8, 2, 3]);
        mine(&mut c, 101, &burn);
        let (s, fake) = spends(c);
        assert_eq!(s.outspend(&burn.compute_txid(), 0).unwrap(), Outspend::Unspent);
        assert_eq!(fake.fetches(), 0);
    }

    #[test]
    fn a_mempool_spender_is_remembered_so_its_block_is_found_without_scanning() {
        let (mut c, _, op) = setup();
        let spend = tx(&[op], 48);
        c.known.insert(spend.compute_txid(), (spend.clone(), None));
        c.mempool_spends.insert(op, spend.compute_txid());
        let (s, fake) = spends(c);
        assert!(matches!(s.outspend(&op.txid, 0).unwrap(), Outspend::Spent { block: None, .. }));
        // Mined in 104 and gone from the mempool.
        {
            let mut c = fake.chain.lock().unwrap();
            c.mempool_spends.clear();
            c.blocks[4].1.push(spend.clone());
            c.known.insert(spend.compute_txid(), (spend.clone(), Some(bh(104))));
        }
        assert_eq!(
            s.outspend(&op.txid, 0).unwrap(),
            Outspend::Spent { txid: spend.compute_txid(), vin: 0, block: Some((bh(104), 104)) }
        );
        assert_eq!(fake.fetches(), 0);
    }

    #[test]
    fn a_replaced_mempool_spender_falls_back_to_the_scan() {
        let (mut c, _, op) = setup();
        let first = tx(&[op], 49);
        c.known.insert(first.compute_txid(), (first.clone(), None));
        c.mempool_spends.insert(op, first.compute_txid());
        let (s, fake) = spends(c);
        assert!(matches!(s.outspend(&op.txid, 0).unwrap(), Outspend::Spent { block: None, .. }));
        // Replaced (RBF): the first spender vanishes, another one is mined.
        let second = tx(&[op], 50);
        {
            let mut c = fake.chain.lock().unwrap();
            c.mempool_spends.clear();
            c.known.remove(&first.compute_txid());
            c.blocks[3].1.push(second.clone());
            c.known.insert(second.compute_txid(), (second.clone(), Some(bh(103))));
        }
        assert_eq!(
            s.outspend(&op.txid, 0).unwrap(),
            Outspend::Spent { txid: second.compute_txid(), vin: 0, block: Some((bh(103), 103)) }
        );
    }

    #[test]
    fn a_reorg_below_the_scan_progress_restarts_it_from_the_funding_block() {
        let (c, _, op) = setup();
        let (s, fake) = spends(c);
        assert!(matches!(s.outspend(&op.txid, 0), Err(OutspendError::Pending(_)))); // 100..=105 scanned, nothing
        // Blocks 103..=105 are replaced; the spend is in the new 103.
        let spend = tx(&[op], 51);
        {
            let mut c = fake.chain.lock().unwrap();
            c.stale.extend([bh(103), bh(104), bh(105)]);
            c.blocks[3] = (bh(1103), vec![spend.clone()]);
            c.blocks[4] = (bh(1104), vec![]);
            c.blocks[5] = (bh(1105), vec![]);
            c.known.insert(spend.compute_txid(), (spend.clone(), Some(bh(1103))));
        }
        assert_eq!(
            s.outspend(&op.txid, 0).unwrap(),
            Outspend::Spent { txid: spend.compute_txid(), vin: 0, block: Some((bh(1103), 103)) }
        );
    }

    #[test]
    fn one_scan_per_output_at_a_time_and_at_most_two_overall() {
        // Three outputs funded in block 100, all spent but no spend mined yet.
        let (mut c, funding, op) = setup();
        let ops: Vec<OutPoint> = (0..3u32).map(|v| OutPoint::new(funding.compute_txid(), v)).collect();
        let f = &mut c.blocks[0].1[0];
        f.output = (0..3).map(|_| TxOut { value: Amount::from_sat(1), script_pubkey: ScriptBuf::new() }).collect();
        let funding = f.clone();
        let ops: Vec<OutPoint> = ops.iter().map(|o| OutPoint::new(funding.compute_txid(), o.vout)).collect();
        c.known.clear();
        c.known.insert(funding.compute_txid(), (funding.clone(), Some(bh(100))));
        let _ = op;
        let (s, fake) = spends(c);
        let s = Arc::new(s);
        *fake.hold.lock().unwrap() = true;
        let scans: Vec<_> = ops[..2]
            .iter()
            .map(|o| {
                let (s, o) = (s.clone(), *o);
                std::thread::spawn(move || s.outspend(&o.txid, o.vout))
            })
            .collect();
        while fake.inside.load(Ordering::SeqCst) < 2 {
            std::thread::yield_now();
        }
        // The same output again, and a third one: both refused at once, not queued.
        assert!(matches!(s.outspend(&ops[0].txid, 0), Err(OutspendError::Pending(_))));
        assert!(matches!(s.outspend(&ops[2].txid, 2), Err(OutspendError::Pending(_))));
        *fake.hold.lock().unwrap() = false;
        fake.released.notify_all();
        for t in scans {
            assert!(matches!(t.join().unwrap(), Err(OutspendError::Pending(_))));
        }
        // Freed: a later search runs.
        let before = fake.fetches();
        let _ = s.outspend(&ops[2].txid, 2);
        assert!(fake.fetches() > before);
    }

    #[test]
    fn outspend_json_has_the_esplora_shape() {
        let t = Txid::from_byte_array([3; 32]);
        assert_eq!(Outspend::Unspent.json(), json!({ "spent": false }));
        assert_eq!(
            Outspend::Spent { txid: t, vin: 2, block: Some((bh(7), 7)) }.json(),
            json!({
                "spent": true, "txid": t.to_string(), "vin": 2,
                "status": { "confirmed": true, "block_height": 7, "block_hash": bh(7).to_string() },
            })
        );
        assert_eq!(
            Outspend::Spent { txid: t, vin: 0, block: None }.json()["status"],
            json!({ "confirmed": false })
        );
    }

}
