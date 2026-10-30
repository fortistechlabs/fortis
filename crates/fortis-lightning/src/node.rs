//! [`Node`]: the fortis Lightning node — ldk-node with a VSS store, keys
//! derived from the BTC wallet's BIP-39 seed, and one LSP peer.
//!
//! **Keys.** The node seed is BIP-85 HEX, 64 bytes, index 0
//! (`m/83696968'/128169'/64'/0'`) of the wallet's BIP-39 seed
//! ([`wallet_core::bip85_hex`]); those 64 bytes are ldk-node's entropy seed
//! as they are. The VSS store id is
//! `hex(HMAC-SHA256(key = node seed, "fortis/ln/vss/v1"))`: stable per seed,
//! unlinkable to the wallet.
//!
//! **Start.** [`Node::new`] only derives keys and validates the config; no
//! I/O. [`Node::start`] runs on a worker thread under one 14 s deadline and
//! returns the first failure as an [`LnError`]:
//!
//! 1. TCP reachability of the LSP → [`LnError::LspUnreachable`] (checked
//!    first, so "no network at all" reads as `LspUnreachable`);
//! 2. one VSS round trip (a `getObject`) → [`LnError::BackupUnavailable`];
//! 3. building ldk-node over the VSS store (reads all state) →
//!    `BackupUnavailable` on storage errors;
//! 4. ldk-node `start` (fee-rate fetch from the chain source);
//! 5. connecting to the LSP as a persisted peer → `LspUnreachable`.
//!
//! On timeout the error is that of the stage still running; the worker keeps
//! going in the background and stops whatever node it built.
//!
//! **One ldk-node per wallet.** VSS writes are unconditional (spec,
//! Findings (b)), so two ldk-nodes on one store silently corrupt channel
//! state. A worker first takes a process-wide claim on the store id and the
//! data dir (released only once its node is stopped and dropped), and holds
//! a generation token that `stop()` and a caller timeout bump. It checks the
//! token and "a node is already running" before building, before starting
//! and before connecting, and installs its node itself, under the lock, only
//! if the token is unchanged and none is installed. ldk-node's `stop` is never
//! called with the lock held.

use std::collections::{BTreeMap, HashMap};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use ldk_node::bitcoin::bip32::Xpriv;
use ldk_node::bitcoin::hashes::{hmac, sha256, Hash, HashEngine};
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Network;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::sign::{KeysManager, NodeSigner, Recipient};
use ldk_node::{BuildError, Builder, Event, NodeError};
use vss_client::client::VssClient;
use vss_client::error::VssError;
use vss_client::headers::FixedHeaders;
use vss_client::types::GetObjectRequest;
use vss_client::util::retry::{ExponentialBackoffRetryPolicy, RetryPolicy};
use zeroize::Zeroizing;

use crate::LnError;

/// [`Node::start`] never takes longer than this (the brief's bound is 15 s).
const START_DEADLINE: Duration = Duration::from_secs(14);
/// Per-probe cap for the LSP and VSS reachability checks.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const VSS_STORE_TAG: &[u8] = b"fortis/ln/vss/v1";

/// Where the LSP (`fortis-lsp`) is; the app fills it from `GET /ln/info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspInfo {
    /// Hex compressed public key.
    pub node_id: String,
    pub host: String,
    pub port: u16,
    /// LSPS2 token, if the LSP requires one.
    pub token: Option<String>,
}

/// The LSP's limits, from `GET /ln/info`. Stored by the node for the
/// receive/pay/channel checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LspLimits {
    pub min_payment_msat: u64,
    pub max_payment_msat: u64,
    pub channel_min_sat: u64,
    pub channel_max_sat: u64,
}

/// The VSS endpoint. `auth_header`, when non-empty, is sent as the
/// `Authorization` header on every VSS request.
#[derive(Clone, PartialEq, Eq)]
pub struct VssConfig {
    pub url: String,
    pub auth_header: String,
}

impl std::fmt::Debug for VssConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VssConfig")
            .field("url", &self.url)
            .field("auth_header", &"<redacted>")
            .finish()
    }
}

/// Where the node gets chain data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainSource {
    /// Production (Android): the edge's Esplora, or the user's own server.
    Esplora { url: String },
    /// Tests only (regtest harness: no Esplora on the dev host). A bitcoind-RPC
    /// node syncs from the current tip without rescanning history.
    BitcoindRpc {
        host: String,
        port: u16,
        user: String,
        password: String,
    },
}

/// Everything [`Node::new`] needs. No `Debug` or `Clone`: it holds the seed,
/// which [`Node::new`] takes out and wipes.
pub struct NodeConfig {
    /// The BTC wallet's 64-byte BIP-39 seed (`Mnemonic::to_seed`).
    /// [`Node::new`] moves it into a zeroizing buffer.
    pub bip39_seed: Vec<u8>,
    pub network: Network,
    /// The node's local data (logs, ldk-node's local files); state lives in VSS.
    pub data_dir: PathBuf,
    pub chain_source: ChainSource,
    pub lsp: LspInfo,
    pub vss: VssConfig,
    pub limits: LspLimits,
}

/// The node's funds. `lightning_sat + onchain_transit_sat +
/// pending_close_sat` counts no sat twice, but leaves out two amounts the
/// node does hold: unconfirmed incoming transit funds, and (while a channel
/// exists) the anchor reserve ldk-node keeps in the transit wallet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Balances {
    /// What the open channels hold for us (spendable on close: LDK's
    /// claimable amount, i.e. less the commitment fee and anchors we pay as
    /// funder).
    pub lightning_sat: u64,
    /// Spendable now in ldk-node's on-chain wallet, the transit wallet:
    /// funds on their way into or out of a channel. Excludes unconfirmed
    /// incoming funds and, while a channel exists, the anchor reserve
    /// ldk-node holds back (see `channel.rs`).
    pub onchain_transit_sat: u64,
    /// Funds of closed or closing channels not yet spendable in the transit
    /// wallet: a closing transaction not yet confirmed, our output under its
    /// force-close timelock, HTLCs being resolved on chain, and ldk-node's
    /// sweeps of those into the transit wallet until they confirm.
    pub pending_close_sat: u64,
    /// What can be received over ready channels.
    pub inbound_sat: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LnEvent {
    PaymentReceived { id: String, amount_msat: u64 },
    PaymentSent { id: String, fee_msat: u64 },
    PaymentFailed { id: String, reason: String },
    ChannelReady,
    ChannelClosed { reason: String },
}

/// The fortis Lightning node. Cheap to create; [`start`](Self::start) does
/// the I/O. Safe to share across threads.
pub struct Node {
    network: Network,
    data_dir: PathBuf,
    chain_source: ChainSource,
    vss: VssConfig,
    limits: LspLimits,
    lsp_id: PublicKey,
    lsp_addr: SocketAddress,
    lsp_host: String,
    lsp_port: u16,
    lsp_token: Option<String>,
    ldk_seed: Zeroizing<[u8; 64]>,
    store_id: String,
    node_id: String,
    /// Keys of the process-wide claim: store id and absolute data dir.
    claim_keys: Vec<String>,
    shared: Arc<Shared>,
}

/// What a [`Node`] shares with its start workers (which may outlive it).
struct Shared {
    inner: Mutex<Option<Arc<Running>>>,
    /// Bumped by `stop()` and by a timed-out `start`: a worker whose token is
    /// stale builds/installs nothing.
    generation: AtomicU64,
}

/// A started ldk-node and the claim on its wallet. Fields drop in order: the
/// node first, then the claim.
pub(crate) struct Running {
    pub(crate) node: Live,
    _claim: Claim,
}

/// A built ldk-node (counted for the tests' "never two at once" check).
pub(crate) struct Live(ldk_node::Node);

impl Live {
    fn new(node: ldk_node::Node) -> Live {
        let n = test_hooks::LIVE.fetch_add(1, Ordering::SeqCst) + 1;
        test_hooks::MAX_LIVE.fetch_max(n, Ordering::SeqCst);
        Live(node)
    }
}

impl std::ops::Deref for Live {
    type Target = ldk_node::Node;
    fn deref(&self) -> &ldk_node::Node {
        &self.0
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.0.stop();
        test_hooks::LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClaimState {
    Starting,
    Running,
}

/// Process-wide: which wallets (store ids, data dirs) have a start worker or
/// a running node in this process.
static CLAIMS: Mutex<BTreeMap<String, ClaimState>> = Mutex::new(BTreeMap::new());

struct Claim {
    keys: Vec<String>,
}

impl Claim {
    /// Takes every key, or none: `Err` names the strongest current holder.
    fn try_acquire(keys: &[String]) -> Result<Claim, ClaimState> {
        let mut claims = CLAIMS.lock().unwrap();
        let held: Vec<ClaimState> = keys.iter().filter_map(|k| claims.get(k).copied()).collect();
        if held.contains(&ClaimState::Running) {
            return Err(ClaimState::Running);
        }
        if !held.is_empty() {
            return Err(ClaimState::Starting);
        }
        for k in keys {
            claims.insert(k.clone(), ClaimState::Starting);
        }
        Ok(Claim {
            keys: keys.to_vec(),
        })
    }

    fn set_running(&self) {
        let mut claims = CLAIMS.lock().unwrap();
        for k in &self.keys {
            claims.insert(k.clone(), ClaimState::Running);
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut claims = CLAIMS.lock().unwrap();
        for k in &self.keys {
            claims.remove(k);
        }
    }
}

impl Node {
    /// Derives the node keys and validates `cfg`. No I/O.
    /// The seed is moved out of `cfg` into a zeroizing buffer.
    pub fn new(mut cfg: NodeConfig) -> Result<Node, LnError> {
        let taken = Zeroizing::new(std::mem::take(&mut cfg.bip39_seed));
        if taken.len() != 64 {
            return Err(LnError::Other("bip39_seed must be 64 bytes".into()));
        }
        let mut bip39_seed = Zeroizing::new([0u8; 64]);
        bip39_seed.copy_from_slice(&taken);
        drop(taken);
        let node_seed = wallet_core::bip85_hex(&bip39_seed, cfg.network, 64, 0);
        drop(bip39_seed);
        let mut ldk_seed = Zeroizing::new([0u8; 64]);
        ldk_seed.copy_from_slice(&node_seed);
        drop(node_seed);

        let mut e = hmac::HmacEngine::<sha256::Hash>::new(&ldk_seed[..]);
        e.input(VSS_STORE_TAG);
        let store_id = hex::encode(hmac::Hmac::<sha256::Hash>::from_engine(e).to_byte_array());

        let node_id = ldk_node_id(&ldk_seed, cfg.network)?.to_string();

        let lsp_id: PublicKey = cfg
            .lsp
            .node_id
            .parse()
            .map_err(|e| LnError::Other(format!("lsp node id: {e}")))?;
        let lsp_addr: SocketAddress = format!("{}:{}", cfg.lsp.host, cfg.lsp.port)
            .parse()
            .map_err(|e| LnError::Other(format!("lsp address: {e:?}")))?;
        let abs_dir = std::path::absolute(&cfg.data_dir)
            .map_err(|e| LnError::Other(format!("data dir: {e}")))?;
        let claim_keys = vec![
            format!("store:{store_id}"),
            format!("dir:{}", abs_dir.display()),
        ];

        Ok(Node {
            network: cfg.network,
            data_dir: cfg.data_dir,
            chain_source: cfg.chain_source,
            vss: cfg.vss,
            limits: cfg.limits,
            lsp_id,
            lsp_addr,
            lsp_host: cfg.lsp.host,
            lsp_port: cfg.lsp.port,
            lsp_token: cfg.lsp.token,
            ldk_seed,
            store_id,
            node_id,
            claim_keys,
            shared: Arc::new(Shared {
                inner: Mutex::new(None),
                generation: AtomicU64::new(0),
            }),
        })
    }

    /// Hex compressed public key; known before `start`.
    pub fn node_id(&self) -> String {
        self.node_id.clone()
    }

    pub fn limits(&self) -> LspLimits {
        self.limits
    }

    /// Connects to the LSP, VSS and the chain source and starts the node.
    /// Never blocks longer than 15 s. `Ok` when already running (also when a
    /// concurrent `start` on this `Node` got there first);
    /// `Other("already_running")` when another `Node` in this process runs
    /// this wallet; `Other("start_in_progress")` when another start for it
    /// is still working; `Other("cancelled")` when `stop()` came first.
    pub fn start(&self) -> Result<(), LnError> {
        if self.running().is_some() {
            return Ok(());
        }
        let generation = self.shared.generation.load(Ordering::SeqCst);
        let deadline = Instant::now() + START_DEADLINE;
        let stage = Arc::new(AtomicU8::new(Stage::Claim as u8));
        let (tx, rx) = mpsc::channel::<Result<(), LnError>>();
        let job = StartJob {
            network: self.network,
            data_dir: self.data_dir.to_string_lossy().into_owned(),
            chain_source: self.chain_source.clone(),
            vss: self.vss.clone(),
            lsp_id: self.lsp_id,
            lsp_addr: self.lsp_addr.clone(),
            lsp_host: self.lsp_host.clone(),
            lsp_port: self.lsp_port,
            lsp_token: self.lsp_token.clone(),
            ldk_seed: self.ldk_seed.clone(),
            store_id: self.store_id.clone(),
            node_id: self.node_id.clone(),
            claim_keys: self.claim_keys.clone(),
            deadline,
            stage: Arc::clone(&stage),
            shared: Arc::clone(&self.shared),
            generation,
        };
        std::thread::Builder::new()
            .name("fortis-ln-start".into())
            .spawn(move || {
                let _ = tx.send(job.run());
            })
            .map_err(|e| LnError::Other(format!("spawn: {e}")))?;

        let wait = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(res) => res,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Same lock as the worker's install: either it installed
                // already, or it never will.
                let inner = self.shared.inner.lock().unwrap();
                if inner.is_some() {
                    return Ok(());
                }
                self.shared.generation.fetch_add(1, Ordering::SeqCst);
                Err(Stage::from(stage.load(Ordering::SeqCst)).timeout_error())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(LnError::Other("start worker panicked".into()))
            }
        }
    }

    /// Stops the node and cancels any pending `start` (a no-op when neither).
    ///
    /// Returns once ldk-node has stopped. In-flight HTLC state is durable
    /// before that: ldk-node persists each channel-monitor update to VSS
    /// synchronously as the HTLC is added, and on stop its background
    /// processor writes the channel manager (and the payment store is
    /// written on every change), so a payment pending at `stop` is still
    /// pending after a restart from the same seed. Normally well under 10 s
    /// (the pay tests assert it); ldk-node's own bounds are 5 s for its
    /// background tasks plus up to 30 s for the final persist when the store
    /// stalls.
    pub fn stop(&self) {
        let running = {
            let mut inner = self.shared.inner.lock().unwrap();
            self.shared.generation.fetch_add(1, Ordering::SeqCst);
            inner.take()
        };
        // Outside the lock; the claim goes when the last `Arc` drops.
        if let Some(r) = running {
            let _ = r.node.stop();
        }
    }

    /// Zero when not running.
    pub fn balances(&self) -> Balances {
        let Some(node) = self.running() else {
            return Balances::default();
        };
        let node = &node.node;
        let b = node.list_balances();
        let channels = node.list_channels();
        let inbound_msat: u64 = channels
            .iter()
            .filter(|c| c.is_channel_ready)
            .map(|c| c.inbound_capacity_msat)
            .sum();
        let open: Vec<_> = channels.iter().map(|c| c.channel_id).collect();
        let (lightning_sat, pending_close_sat) = crate::channel::lightning_balances(&b, &open);
        Balances {
            lightning_sat,
            onchain_transit_sat: b.spendable_onchain_balance_sats,
            pending_close_sat,
            inbound_sat: inbound_msat / 1000,
        }
    }

    /// The next event, waiting up to `timeout`. Events are acknowledged to
    /// ldk-node as they are returned; ones fortis doesn't surface are skipped.
    /// `None` at once when the node is not running.
    pub fn next_event(&self, timeout: Duration) -> Option<LnEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            let running = self.running()?;
            let node = &running.node;
            if let Some(event) = node.next_event() {
                // On failure ldk-node re-delivers the event; don't surface it twice.
                if node.event_handled().is_err() {
                    continue;
                }
                if let Some(e) = translate(event) {
                    return Some(e);
                }
                continue;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(100)));
        }
    }

    pub(crate) fn running(&self) -> Option<Arc<Running>> {
        self.shared.inner.lock().unwrap().clone()
    }

    pub(crate) fn network(&self) -> Network {
        self.network
    }

    /// The LSP's node id and address.
    pub(crate) fn lsp_peer(&self) -> (PublicKey, SocketAddress) {
        (self.lsp_id, self.lsp_addr.clone())
    }

    pub(crate) fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// One VSS round trip (see [`probe_vss`]), at most `timeout`.
    pub(crate) fn probe_backup(&self, timeout: Duration) -> Result<(), LnError> {
        probe_vss(&self.vss, &self.store_id, timeout)
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop();
    }
}

/// ldk-node's node id for `ldk_seed`: its `KeysManager` is seeded with the
/// BIP-32 master private key of the 64 seed bytes.
fn ldk_node_id(ldk_seed: &[u8; 64], network: Network) -> Result<PublicKey, LnError> {
    let mut master = Xpriv::new_master(network, ldk_seed)
        .map_err(|e| LnError::Other(format!("master key: {e}")))?;
    let km_seed = Zeroizing::new(master.private_key.secret_bytes());
    master.private_key.non_secure_erase();
    // KeysManager keeps its derived secrets unwiped until dropped (LDK type).
    KeysManager::new(&km_seed, 0, 0, true)
        .get_node_id(Recipient::Node)
        .map_err(|_| LnError::Other("node id".into()))
}

fn hex32(b: &[u8; 32]) -> String {
    hex::encode(b)
}

fn translate(event: Event) -> Option<LnEvent> {
    Some(match event {
        Event::PaymentReceived {
            payment_id,
            payment_hash,
            amount_msat,
            ..
        } => LnEvent::PaymentReceived {
            id: payment_id
                .map(|p| hex32(&p.0))
                .unwrap_or_else(|| hex32(&payment_hash.0)),
            amount_msat,
        },
        Event::PaymentSuccessful {
            payment_id,
            payment_hash,
            fee_paid_msat,
            ..
        } => LnEvent::PaymentSent {
            id: payment_id
                .map(|p| hex32(&p.0))
                .unwrap_or_else(|| hex32(&payment_hash.0)),
            fee_msat: fee_paid_msat.unwrap_or(0),
        },
        Event::PaymentFailed {
            payment_id,
            payment_hash,
            reason,
        } => LnEvent::PaymentFailed {
            id: payment_id
                .map(|p| hex32(&p.0))
                .or_else(|| payment_hash.map(|h| hex32(&h.0)))
                .unwrap_or_default(),
            reason: reason
                .map(|r| format!("{r:?}"))
                .unwrap_or_else(|| "unknown".into()),
        },
        Event::ChannelReady { .. } => LnEvent::ChannelReady,
        Event::ChannelClosed { reason, .. } => LnEvent::ChannelClosed {
            reason: reason
                .map(|r| r.to_string())
                .unwrap_or_else(|| "unknown".into()),
        },
        _ => return None,
    })
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum Stage {
    Claim = 0,
    Lsp = 1,
    Vss = 2,
    Build = 3,
    Start = 4,
    Connect = 5,
}

impl From<u8> for Stage {
    fn from(v: u8) -> Stage {
        match v {
            0 => Stage::Claim,
            1 => Stage::Lsp,
            2 => Stage::Vss,
            3 => Stage::Build,
            4 => Stage::Start,
            _ => Stage::Connect,
        }
    }
}

impl Stage {
    fn timeout_error(self) -> LnError {
        match self {
            Stage::Claim => LnError::Other("start_in_progress".into()),
            Stage::Lsp | Stage::Connect => LnError::LspUnreachable,
            Stage::Vss | Stage::Build => LnError::BackupUnavailable,
            Stage::Start => LnError::Other("chain_source_timeout".into()),
        }
    }
}

struct StartJob {
    network: Network,
    data_dir: String,
    chain_source: ChainSource,
    vss: VssConfig,
    lsp_id: PublicKey,
    lsp_addr: SocketAddress,
    lsp_host: String,
    lsp_port: u16,
    lsp_token: Option<String>,
    ldk_seed: Zeroizing<[u8; 64]>,
    store_id: String,
    node_id: String,
    claim_keys: Vec<String>,
    deadline: Instant,
    stage: Arc<AtomicU8>,
    shared: Arc<Shared>,
    generation: u64,
}

/// Why a worker gives up early: `Ok(())` = a node is already running for
/// this `Node`, `Err` = cancelled.
type Halt = Result<(), LnError>;

fn cancelled() -> LnError {
    LnError::Other("cancelled".into())
}

impl StartJob {
    fn set(&self, s: Stage) {
        self.stage.store(s as u8, Ordering::SeqCst);
    }

    fn probe_timeout(&self) -> Duration {
        self.deadline
            .saturating_duration_since(Instant::now())
            .min(PROBE_TIMEOUT)
            .max(Duration::from_millis(1))
    }

    fn vss_headers(&self) -> HashMap<String, String> {
        vss_headers(&self.vss)
    }

    /// `Some(outcome)` when the worker must stop: token stale or a node
    /// already installed.
    fn checkpoint(&self) -> Option<Halt> {
        if self.shared.generation.load(Ordering::SeqCst) != self.generation {
            return Some(Err(cancelled()));
        }
        if self.shared.inner.lock().unwrap().is_some() {
            return Some(Ok(()));
        }
        None
    }

    fn claim(&self) -> Result<Claim, Halt> {
        loop {
            if let Some(halt) = self.checkpoint() {
                return Err(halt);
            }
            match Claim::try_acquire(&self.claim_keys) {
                Ok(c) => return Ok(c),
                Err(ClaimState::Running) => {
                    return Err(Err(LnError::Other("already_running".into())))
                }
                Err(ClaimState::Starting) if Instant::now() >= self.deadline => {
                    return Err(Err(LnError::Other("start_in_progress".into())))
                }
                Err(ClaimState::Starting) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    fn run(self) -> Result<(), LnError> {
        self.set(Stage::Claim);
        let claim = match self.claim() {
            Ok(c) => c,
            Err(halt) => return halt,
        };
        self.set(Stage::Lsp);
        self.probe_lsp()?;
        self.set(Stage::Vss);
        self.probe_vss()?;

        if let Some(halt) = self.checkpoint() {
            return halt;
        }
        self.set(Stage::Build);
        let mut b = Builder::new();
        b.set_network(self.network);
        match &self.chain_source {
            ChainSource::Esplora { url } => {
                b.set_chain_source_esplora(url.clone(), None);
            }
            ChainSource::BitcoindRpc {
                host,
                port,
                user,
                password,
            } => {
                b.set_chain_source_bitcoind_rpc(
                    host.clone(),
                    *port,
                    user.clone(),
                    password.clone(),
                );
            }
        }
        b.set_entropy_seed_bytes(*self.ldk_seed);
        b.set_storage_dir_path(self.data_dir.clone());
        b.set_liquidity_source_lsps2(self.lsp_id, self.lsp_addr.clone(), self.lsp_token.clone());
        let node = b
            .build_with_vss_store_and_fixed_headers(
                self.vss.url.clone(),
                self.store_id.clone(),
                self.vss_headers(),
            )
            .map_err(map_build_error)?;
        test_hooks::BUILDS.fetch_add(1, Ordering::SeqCst);
        // Dropping `Live` stops the node, outside any lock.
        let node = Live::new(node);
        if node.node_id().to_string() != self.node_id {
            return Err(LnError::Other("node id mismatch".into()));
        }

        if let Some(halt) = self.checkpoint() {
            return halt;
        }
        self.set(Stage::Start);
        node.start().map_err(map_node_error)?;

        if let Some(halt) = self.checkpoint() {
            return halt;
        }
        self.set(Stage::Connect);
        if let Err(e) = node.connect(self.lsp_id, self.lsp_addr.clone(), true) {
            return Err(match e {
                NodeError::PersistenceFailed => LnError::BackupUnavailable,
                _ => LnError::LspUnreachable,
            });
        }

        let mut inner = self.shared.inner.lock().unwrap();
        let outcome = if self.shared.generation.load(Ordering::SeqCst) != self.generation {
            Err(cancelled())
        } else if inner.is_some() {
            Ok(())
        } else {
            claim.set_running();
            *inner = Some(Arc::new(Running {
                node,
                _claim: claim,
            }));
            return Ok(());
        };
        drop(inner);
        drop(node); // stops it, then the claim is released
        drop(claim);
        outcome
    }

    fn probe_lsp(&self) -> Result<(), LnError> {
        let addrs: Vec<_> = (self.lsp_host.as_str(), self.lsp_port)
            .to_socket_addrs()
            .map_err(|_| LnError::LspUnreachable)?
            .collect();
        for addr in addrs {
            if TcpStream::connect_timeout(&addr, self.probe_timeout()).is_ok() {
                return Ok(());
            }
        }
        Err(LnError::LspUnreachable)
    }

    fn probe_vss(&self) -> Result<(), LnError> {
        probe_vss(&self.vss, &self.store_id, self.probe_timeout())
    }
}

/// One `getObject` of a key that need not exist, with no retry and at most
/// `timeout`: any answer from VSS (including "no such key") means it is
/// reachable and accepts our auth; anything else is `BackupUnavailable`.
fn probe_vss(vss: &VssConfig, store_id: &str, timeout: Duration) -> Result<(), LnError> {
    let rt = ldk_node::tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| LnError::Other(format!("runtime: {e}")))?;
    let http = vss_client::reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| LnError::Other(format!("http client: {e}")))?;
    let policy = ExponentialBackoffRetryPolicy::<VssError>::new(Duration::from_millis(10))
        .with_max_attempts(1);
    let client = VssClient::from_client_and_headers(
        vss.url.clone(),
        http,
        policy,
        Arc::new(FixedHeaders::new(vss_headers(vss))),
    );
    let req = GetObjectRequest {
        store_id: store_id.to_string(),
        key: "fortis-probe".into(),
    };
    match rt.block_on(client.get_object(&req)) {
        Ok(_) | Err(VssError::NoSuchKeyError(_)) => Ok(()),
        Err(_) => Err(LnError::BackupUnavailable),
    }
}

fn vss_headers(vss: &VssConfig) -> HashMap<String, String> {
    let mut h = HashMap::new();
    if !vss.auth_header.is_empty() {
        h.insert("Authorization".to_string(), vss.auth_header.clone());
    }
    h
}

fn map_build_error(e: BuildError) -> LnError {
    match e {
        BuildError::KVStoreSetupFailed | BuildError::ReadFailed | BuildError::WriteFailed => {
            LnError::BackupUnavailable
        }
        BuildError::NetworkMismatch => LnError::WrongNetwork,
        other => LnError::Other(format!("build: {other}")),
    }
}

fn map_node_error(e: NodeError) -> LnError {
    match e {
        NodeError::ConnectionFailed => LnError::LspUnreachable,
        NodeError::PersistenceFailed => LnError::BackupUnavailable,
        other => LnError::Other(format!("start: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(seed: Vec<u8>) -> NodeConfig {
        NodeConfig {
            bip39_seed: seed,
            network: Network::Regtest,
            data_dir: PathBuf::from("unused"),
            chain_source: ChainSource::Esplora {
                url: "http://192.0.2.1".into(),
            },
            lsp: LspInfo {
                node_id: "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
                    .into(),
                host: "192.0.2.1".into(),
                port: 9735,
                token: None,
            },
            vss: VssConfig {
                url: "http://192.0.2.1/vss".into(),
                auth_header: String::new(),
            },
            limits: LspLimits {
                min_payment_msat: 1,
                max_payment_msat: 2,
                channel_min_sat: 3,
                channel_max_sat: 4,
            },
        }
    }

    #[test]
    fn node_seed_is_bip85_hex_64_index_0_and_store_id_its_hmac() {
        let seed = [9u8; 64];
        let node = Node::new(cfg(seed.to_vec())).unwrap();
        let expect = wallet_core::bip85_hex(&seed, Network::Regtest, 64, 0);
        assert_eq!(&node.ldk_seed[..], &expect[..]);
        let mut e = hmac::HmacEngine::<sha256::Hash>::new(&expect);
        e.input(b"fortis/ln/vss/v1");
        let id = hmac::Hmac::<sha256::Hash>::from_engine(e).to_byte_array();
        assert_eq!(node.store_id, hex::encode(id));
        assert_eq!(
            node.node_id,
            ldk_node_id(&node.ldk_seed, Network::Regtest)
                .unwrap()
                .to_string()
        );
        assert_ne!(
            node.node_id(),
            Node::new(cfg(vec![8u8; 64])).unwrap().node_id()
        );
    }

    #[test]
    fn rejects_a_seed_that_is_not_64_bytes() {
        assert!(matches!(
            Node::new(cfg(vec![9u8; 32])),
            Err(LnError::Other(_))
        ));
    }
}

/// Counters for the integration tests (not API).
#[doc(hidden)]
pub mod test_hooks {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(crate) static BUILDS: AtomicU64 = AtomicU64::new(0);
    pub(crate) static LIVE: AtomicU64 = AtomicU64::new(0);
    pub(crate) static MAX_LIVE: AtomicU64 = AtomicU64::new(0);

    /// ldk-node builds in this process so far.
    pub fn builds() -> u64 {
        BUILDS.load(Ordering::SeqCst)
    }

    /// Most fortis ldk-nodes alive at once since the last [`reset_max_live`].
    pub fn max_live() -> u64 {
        MAX_LIVE.load(Ordering::SeqCst)
    }

    /// Resets [`max_live`] to the current count.
    pub fn reset_max_live() {
        MAX_LIVE.store(LIVE.load(Ordering::SeqCst), Ordering::SeqCst);
    }
}
