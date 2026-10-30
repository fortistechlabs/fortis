//! [`Node`]: the fortis Lightning node — ldk-node with a VSS store, keys
//! derived from the BTC wallet's BIP-39 seed, and one LSP peer.
//!
//! **Keys.** The node seed is BIP-85 HEX (32 bytes, index 0) of the wallet's
//! BIP-39 seed ([`wallet_core::bip85_hex`]). ldk-node wants 64 seed bytes, so
//! the 32 bytes are used as BIP-39 entropy and expanded with an empty
//! passphrase — exactly what ldk-node's own `set_entropy_bip39_mnemonic`
//! does with a 24-word mnemonic. The VSS store id is
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

use std::collections::HashMap;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
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

/// Everything [`Node::new`] needs. No `Debug`: it holds the seed.
#[derive(Clone)]
pub struct NodeConfig {
    /// The BTC wallet's 64-byte BIP-39 seed (`Mnemonic::to_seed`).
    pub bip39_seed: Vec<u8>,
    pub network: Network,
    /// The node's local data (logs, ldk-node's local files); state lives in VSS.
    pub data_dir: PathBuf,
    pub chain_source: ChainSource,
    pub lsp: LspInfo,
    pub vss: VssConfig,
    pub limits: LspLimits,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Balances {
    /// Spendable-on-close Lightning balance (channels).
    pub lightning_sat: u64,
    /// ldk-node's on-chain wallet: funds on their way into or out of a channel.
    pub onchain_transit_sat: u64,
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
    inner: Mutex<Option<Arc<ldk_node::Node>>>,
    /// Held by a start worker for its whole life, so a worker left running
    /// past a timeout and a new `start` never build two nodes at once.
    starting: Arc<Mutex<()>>,
}

impl Node {
    /// Derives the node keys and validates `cfg`. No I/O.
    pub fn new(cfg: NodeConfig) -> Result<Node, LnError> {
        let bip39_seed: Zeroizing<[u8; 64]> = Zeroizing::new(
            cfg.bip39_seed
                .as_slice()
                .try_into()
                .map_err(|_| LnError::Other("bip39_seed must be 64 bytes".into()))?,
        );
        let node_seed = wallet_core::bip85_hex(&bip39_seed, cfg.network, 32, 0);
        let mnemonic = ldk_node::bip39::Mnemonic::from_entropy(&node_seed)
            .map_err(|e| LnError::Other(format!("node seed: {e}")))?;
        let ldk_seed = Zeroizing::new(mnemonic.to_seed(""));

        let mut e = hmac::HmacEngine::<sha256::Hash>::new(&node_seed);
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
            inner: Mutex::new(None),
            starting: Arc::new(Mutex::new(())),
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
    /// Never blocks longer than 15 s. A no-op when already running.
    pub fn start(&self) -> Result<(), LnError> {
        // Not held while waiting: balances/next_event stay responsive.
        if self.running().is_some() {
            return Ok(());
        }
        let deadline = Instant::now() + START_DEADLINE;
        let stage = Arc::new(AtomicU8::new(Stage::Lsp as u8));
        let (tx, rx) = mpsc::channel::<Result<ldk_node::Node, LnError>>();
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
            deadline,
            stage: Arc::clone(&stage),
        };
        let starting = Arc::clone(&self.starting);
        std::thread::Builder::new()
            .name("fortis-ln-start".into())
            .spawn(move || {
                // A worker from a timed-out start may still hold this.
                let _busy = loop {
                    if let Ok(g) = starting.try_lock() {
                        break g;
                    }
                    if Instant::now() >= job.deadline {
                        let _ = tx.send(Err(LnError::Other("previous start still running".into())));
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                };
                let res = job.run();
                if let Err(mpsc::SendError(Ok(node))) = tx.send(res) {
                    // The caller gave up (timeout): don't leave a node running.
                    let _ = node.stop();
                }
            })
            .map_err(|e| LnError::Other(format!("spawn: {e}")))?;

        let wait = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(Ok(node)) => {
                let mut inner = self.inner.lock().unwrap();
                if inner.is_some() {
                    // A concurrent start won (its worker ran first): keep that one.
                    let _ = node.stop();
                } else {
                    *inner = Some(Arc::new(node));
                }
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(Stage::from(stage.load(Ordering::SeqCst)).timeout_error())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(LnError::Other("start worker panicked".into()))
            }
        }
    }

    /// Stops the node (a no-op when not running).
    pub fn stop(&self) {
        let node = self.inner.lock().unwrap().take();
        if let Some(node) = node {
            let _ = node.stop();
        }
    }

    /// Zero when not running.
    pub fn balances(&self) -> Balances {
        let Some(node) = self.running() else {
            return Balances::default();
        };
        let b = node.list_balances();
        let inbound_msat: u64 = node
            .list_channels()
            .iter()
            .filter(|c| c.is_channel_ready)
            .map(|c| c.inbound_capacity_msat)
            .sum();
        Balances {
            lightning_sat: b.total_lightning_balance_sats,
            onchain_transit_sat: b.total_onchain_balance_sats,
            inbound_sat: inbound_msat / 1000,
        }
    }

    /// The next event, waiting up to `timeout`. Events are acknowledged to
    /// ldk-node as they are returned; ones fortis doesn't surface are skipped.
    /// `None` at once when the node is not running.
    pub fn next_event(&self, timeout: Duration) -> Option<LnEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            let node = self.running()?;
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

    fn running(&self) -> Option<Arc<ldk_node::Node>> {
        self.inner.lock().unwrap().clone()
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
    let master = Xpriv::new_master(network, ldk_seed)
        .map_err(|e| LnError::Other(format!("master key: {e}")))?;
    let km_seed = Zeroizing::new(master.private_key.secret_bytes());
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
    Lsp = 0,
    Vss = 1,
    Build = 2,
    Start = 3,
    Connect = 4,
}

impl From<u8> for Stage {
    fn from(v: u8) -> Stage {
        match v {
            0 => Stage::Lsp,
            1 => Stage::Vss,
            2 => Stage::Build,
            3 => Stage::Start,
            _ => Stage::Connect,
        }
    }
}

impl Stage {
    fn timeout_error(self) -> LnError {
        match self {
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
    deadline: Instant,
    stage: Arc<AtomicU8>,
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
        let mut h = HashMap::new();
        if !self.vss.auth_header.is_empty() {
            h.insert("Authorization".to_string(), self.vss.auth_header.clone());
        }
        h
    }

    fn run(self) -> Result<ldk_node::Node, LnError> {
        self.set(Stage::Lsp);
        self.probe_lsp()?;
        self.set(Stage::Vss);
        self.probe_vss()?;

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
        if node.node_id().to_string() != self.node_id {
            return Err(LnError::Other("node id mismatch".into()));
        }

        self.set(Stage::Start);
        node.start().map_err(map_node_error)?;

        self.set(Stage::Connect);
        if let Err(e) = node.connect(self.lsp_id, self.lsp_addr.clone(), true) {
            let _ = node.stop();
            return Err(match e {
                NodeError::PersistenceFailed => LnError::BackupUnavailable,
                _ => LnError::LspUnreachable,
            });
        }
        Ok(node)
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

    /// One `getObject` of a key that need not exist: any answer from VSS
    /// (including "no such key") means it is reachable and accepts our auth.
    fn probe_vss(&self) -> Result<(), LnError> {
        let rt = ldk_node::tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| LnError::Other(format!("runtime: {e}")))?;
        let http = vss_client::reqwest::Client::builder()
            .timeout(self.probe_timeout())
            .build()
            .map_err(|e| LnError::Other(format!("http client: {e}")))?;
        let policy = ExponentialBackoffRetryPolicy::<VssError>::new(Duration::from_millis(10))
            .with_max_attempts(1);
        let client = VssClient::from_client_and_headers(
            self.vss.url.clone(),
            http,
            policy,
            Arc::new(FixedHeaders::new(self.vss_headers())),
        );
        let req = GetObjectRequest {
            store_id: self.store_id.clone(),
            key: "fortis-probe".into(),
        };
        match rt.block_on(client.get_object(&req)) {
            Ok(_) | Err(VssError::NoSuchKeyError(_)) => Ok(()),
            Err(_) => Err(LnError::BackupUnavailable),
        }
    }
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
