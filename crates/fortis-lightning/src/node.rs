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
//! I/O. [`Node::start_with`] runs on a worker thread under one 14 s deadline
//! and returns the first failure as an [`LnError`]:
//!
//! 1. TCP reachability of the LSP → [`LnError::LspUnreachable`] (checked
//!    first, so "no network at all" reads as `LspUnreachable`);
//! 2. the device lock (`device.rs`; two VSS round trips, which double as
//!    the reachability check): VSS not answering →
//!    [`LnError::BackupUnavailable`], held by another device →
//!    [`LnError::ActiveOnAnotherDevice`] — in both cases before anything is
//!    built;
//! 3. building ldk-node over the VSS store (reads all state) →
//!    `BackupUnavailable` on storage errors;
//! 4. ldk-node `start` (fee-rate fetch from the chain source);
//! 5. connecting to the LSP as a persisted peer → `LspUnreachable`.
//!
//! On timeout the error is that of the stage still running; the worker keeps
//! going in the background and stops whatever node it built (and releases
//! the device lock it took).
//!
//! **Exit mode.** [`StartMode::Exit`] is the recovery start for when the
//! LSP is gone (spec: users can force-close unilaterally at any time). It
//! skips stage 1, and stage 5 is best effort and runs after the node is
//! installed: a TCP probe of the LSP (≤ 2 s, and the time left), and
//! `connect` only when that answers; neither failing is an error (the node
//! is then just not connected), and a stalling LSP cannot make the start
//! fail (the caller's deadline finds the node installed: `Ok`). Stages 2–4
//! and their errors are as in Normal mode: VSS and the chain source are
//! still required. ldk-node keeps trying to reconnect to its persisted peers
//! (the LSP while a channel exists) in the background every 60 s; once it is
//! connected, the node behaves as in Normal mode. While it is not,
//! `receive`, `pay`, `open_channel` and `close_channel` return
//! `LspUnreachable` at once ([`Node::exit_gate`]).
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
//!
//! **One active device per seed.** The process-wide claim cannot see other
//! phones; the device lock in VSS does (`device.rs`, spec Findings (b)).
//! The running node holds it and re-checks it (a conditional rewrite, one
//! VSS round trip ≤ 3 s) before every payment, channel and transit action
//! ([`Node::pre_send_checks`](crate::Node)) and every
//! [`LOCK_CHECK_INTERVAL`] (60 s) on a background thread. When another
//! device has taken it over, the check stops this node (not under any of
//! the node's locks), queues [`LnEvent::DeviceLockLost`] (once) and the
//! action returns `ActiveOnAnotherDevice`; VSS not answering is
//! `BackupUnavailable` and changes nothing. [`Node::stop`] releases the
//! lock (≤ 3 s, best effort), so "closed the app on phone A, opened it on
//! phone B" needs no takeover; [`Node::takeover`] is for the phone that
//! was lost or crashed.

use std::collections::{BTreeMap, HashMap};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ldk_node::bitcoin::bip32::Xpriv;
use ldk_node::bitcoin::hashes::{hmac, sha256, Hash, HashEngine};
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Network;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::sign::{KeysManager, NodeSigner, Recipient};
use ldk_node::{BuildError, Builder, Event, NodeError};
use zeroize::Zeroizing;

use crate::device::{self, DeviceId, Held, VssLockClient};
use crate::LnError;

/// [`Node::start`] never takes longer than this (the brief's bound is 15 s).
const START_DEADLINE: Duration = Duration::from_secs(14);
/// Rapid Gossip Sync snapshots for mainnet (LDK's public server; the LSP uses it too).
const MAINNET_RGS_URL: &str = "https://rapidsync.lightningdevkit.org/snapshot";
/// Per-probe cap for the LSP and VSS reachability checks.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Exit mode's best-effort LSP probe: a host that is gone (packets dropped)
/// costs at most this.
const EXIT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const VSS_STORE_TAG: &[u8] = b"fortis/ln/vss/v1";
/// The running node's background device-lock check: every this long
/// (tests shorten it, [`test_hooks::set_lock_check_interval`]).
pub(crate) const LOCK_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Cap on one device-lock check (background, or before an action).
pub(crate) const LOCK_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
/// Cap on releasing the lock in `stop()`.
const LOCK_RELEASE_TIMEOUT: Duration = Duration::from_secs(3);
/// `stop()` waits at most this long for a cancelled start worker and the
/// background check to finish ([`Node::is_quiescent`]).
const QUIESCE_TIMEOUT: Duration = Duration::from_secs(10);

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
#[derive(Clone, PartialEq, Eq)]
pub enum ChainSource {
    /// Production (Android): the edge's Esplora, or the user's own server.
    /// `auth_header` is sent as `Authorization` on every request: the hosted
    /// edge runs with `--require-token` and refuses chain reads without it.
    /// Pass it for the hosted edge only, never for a server of the user's own.
    Esplora {
        url: String,
        auth_header: Option<String>,
    },
    /// Tests only (regtest harness: no Esplora on the dev host). A bitcoind-RPC
    /// node syncs from the current tip without rescanning history.
    BitcoindRpc {
        host: String,
        port: u16,
        user: String,
        password: String,
    },
}

impl std::fmt::Debug for ChainSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainSource::Esplora { url, auth_header } => f
                .debug_struct("Esplora")
                .field("url", url)
                .field("auth_header", &auth_header.as_ref().map(|_| "<redacted>"))
                .finish(),
            ChainSource::BitcoindRpc { host, port, user, .. } => f
                .debug_struct("BitcoindRpc")
                .field("host", host)
                .field("port", port)
                .field("user", user)
                .field("password", &"<redacted>")
                .finish(),
        }
    }
}

/// Which Bitcoin-family chain the Lightning node runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LnChain {
    Btc,
    /// XBT (the BLAKE2b fork): Esplora only, peer-to-peer gossip, no anchors,
    /// splicing with the LSP.
    Xbt,
}

/// XBT forked from BTC at this height; older channel announcements are BTC's.
const XBT_GOSSIP_FLOOR_HEIGHT: u32 = 961640;
/// BLAKE2b's activation height on an XBT regtest (Knots
/// `testactivationheight=blake2b@20`, as `tools/xbt-regtest` runs it): the
/// regtest chain's gossip floor. A mainnet floor there would drop every
/// channel announcement, so the node could not route past its LSP.
const XBT_REGTEST_GOSSIP_FLOOR_HEIGHT: u32 = 20;

/// The height below which channel announcements are not XBT's.
fn xbt_gossip_floor_height(network: Network) -> u32 {
    match network {
        Network::Regtest => XBT_REGTEST_GOSSIP_FLOOR_HEIGHT,
        _ => XBT_GOSSIP_FLOOR_HEIGHT,
    }
}

/// What a [`LnChain`] changes in the ldk-node builder.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LdkSettings {
    pub xbt: Option<ldk_node::config::XbtConfig>,
    pub rgs_url: Option<String>,
    pub anchors_trust_lsp: bool,
}

pub(crate) fn ldk_settings(chain: LnChain, network: Network, lsp_id: PublicKey) -> LdkSettings {
    match chain {
        LnChain::Xbt => LdkSettings {
            xbt: Some(ldk_node::config::XbtConfig {
                gossip_floor_height: xbt_gossip_floor_height(network),
                splice_peers: vec![lsp_id],
                splice_all_peers: false,
            }),
            rgs_url: None,
            anchors_trust_lsp: false,
        },
        LnChain::Btc => LdkSettings {
            xbt: None,
            rgs_url: (network == Network::Bitcoin).then(|| MAINNET_RGS_URL.to_string()),
            anchors_trust_lsp: true,
        },
    }
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
    /// Which chain the node runs on. `Btc` is today's behaviour.
    pub chain: LnChain,
}

/// The node's funds, in five parts that never count a sat twice:
/// `lightning_sat + onchain_transit_sat + pending_close_sat + reserved_sat +
/// pending_transit_sat` is everything the node holds for the user, up to the
/// amounts below. (The app's "Lightning" total is that sum; computed in
/// `channel.rs`, `breakdown`.)
///
/// **Not visible** (by design, all small or transient):
/// - in `lightning_sat`, the funder's commitment fee and the two 330-sat
///   anchors (LDK's claimable amount excludes them; a cooperative close
///   returns part of the fee);
/// - outbound HTLCs still unresolved on a closed channel (LDK counts them
///   as 0 until they time out back to us);
/// - while a cooperative close is unconfirmed, `pending_close_sat` shows the
///   monitor's amount, which exceeds the closing output by up to the close
///   fee;
/// - a transaction paying the transit wallet (deposit, sweep) between its
///   broadcast and the wallet's next sync sees it (Esplora: the wallet sync
///   interval).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Balances {
    /// What the open channels hold for us: LDK's claimable amount on close,
    /// i.e. less the commitment fee and anchors we pay as funder.
    pub lightning_sat: u64,
    /// Spendable now in the transit wallet (ldk-node's on-chain wallet):
    /// confirmed funds and our own unconfirmed change, less `reserved_sat`.
    pub onchain_transit_sat: u64,
    /// Funds of closed or closing channels not yet in the transit wallet: a
    /// closing transaction not yet confirmed, our output under its
    /// force-close timelock, HTLCs being resolved on chain, and ldk-node's
    /// sweeps of those until the wallet counts them.
    pub pending_close_sat: u64,
    /// The anchor reserve ldk-node holds back in the transit wallet while a
    /// channel exists (25 000 sat per channel, capped by what the wallet
    /// holds). Spendable again once no channel is left.
    pub reserved_sat: u64,
    /// Unconfirmed incoming transit funds (e.g. a "Move to Lightning"
    /// deposit before its first confirmation), excluding a cooperative close
    /// already counted in `pending_close_sat`.
    pub pending_transit_sat: u64,
    /// What can be received over ready channels.
    pub inbound_sat: u64,
}

/// How [`Node::start_with`] starts the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartMode {
    /// The LSP must be reachable (`LspUnreachable` otherwise).
    Normal,
    /// Recovery: starts without the LSP so the user can force-close and
    /// sweep (module docs). VSS and the chain source are still required.
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LnEvent {
    PaymentReceived { id: String, amount_msat: u64 },
    PaymentSent { id: String, fee_msat: u64 },
    PaymentFailed { id: String, reason: String },
    ChannelReady,
    ChannelClosed { reason: String },
    /// Another device took over this wallet's Lightning (its device lock):
    /// this node has stopped itself. Reported once; `start` returns
    /// `ActiveOnAnotherDevice` until [`Node::takeover`].
    DeviceLockLost,
}

/// The fortis Lightning node. Cheap to create; [`start`](Self::start) does
/// the I/O. Safe to share across threads.
pub struct Node {
    network: Network,
    data_dir: PathBuf,
    chain_source: ChainSource,
    chain: LnChain,
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
    /// Serialises the channel and transit calls (`channel.rs`).
    channel_ops: Mutex<()>,
}

/// What a [`Node`] shares with its start workers (which may outlive it) and
/// its background lock check (which holds it only as a `Weak`).
struct Shared {
    inner: Mutex<Option<Arc<Running>>>,
    /// Bumped by `stop()`, by a timed-out `start` and by a lost device
    /// lock: a worker whose token is stale builds/installs nothing.
    generation: AtomicU64,
    /// The device lock this node holds: set by a start worker once acquired,
    /// cleared on release, on loss, or when the worker gives up. Held across
    /// each lock round trip, so checks and the release never interleave.
    lock: Mutex<Option<HeldLock>>,
    /// A `DeviceLockLost` not yet returned by `next_event`.
    lock_lost: AtomicBool,
    /// Dropping it wakes and ends the background check.
    checker: Mutex<Option<mpsc::Sender<()>>>,
    /// Start workers, the background check and lost-lock stoppers alive.
    busy: Arc<Busy>,
    vss: VssConfig,
    store_id: String,
    vss_client: Mutex<Option<Arc<VssLockClient>>>,
}

impl Shared {
    fn vss_client(&self) -> Result<Arc<VssLockClient>, LnError> {
        let mut slot = self.vss_client.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = slot.as_ref() {
            return Ok(Arc::clone(c));
        }
        let c = Arc::new(VssLockClient::new(&self.vss, &self.store_id)?);
        *slot = Some(Arc::clone(&c));
        Ok(c)
    }
}

#[derive(Clone, Copy)]
struct HeldLock {
    me: DeviceId,
    held: Held,
}

/// A count of this node's helper threads, with a wake-up for `stop()`.
#[derive(Default)]
struct Busy {
    n: Mutex<usize>,
    idle: Condvar,
}

/// One counted helper thread (see [`Busy`]); uncounted on drop.
struct BusyToken(Arc<Busy>);

impl BusyToken {
    fn new(b: &Arc<Busy>) -> BusyToken {
        *b.n.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        BusyToken(Arc::clone(b))
    }
}

impl Drop for BusyToken {
    fn drop(&mut self) {
        let mut n = self.0.n.lock().unwrap_or_else(|e| e.into_inner());
        *n -= 1;
        self.0.idle.notify_all();
    }
}

impl Busy {
    fn idle(&self) -> bool {
        *self.n.lock().unwrap_or_else(|e| e.into_inner()) == 0
    }

    /// Waits until no helper thread is left, at most `timeout`.
    fn wait_idle(&self, timeout: Duration) {
        let n = self.n.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self.idle.wait_timeout_while(n, timeout, |n| *n > 0);
    }
}

/// A started ldk-node and the claim on its wallet. Fields drop in order: the
/// node first, then the claim.
pub(crate) struct Running {
    pub(crate) node: Live,
    pub(crate) mode: StartMode,
    /// The generation token it was installed under.
    generation: u64,
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
    Stopping,
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
        let mut claims = CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
        let held: Vec<ClaimState> = keys.iter().filter_map(|k| claims.get(k).copied()).collect();
        if held.contains(&ClaimState::Running) {
            return Err(ClaimState::Running);
        }
        if held.contains(&ClaimState::Stopping) {
            return Err(ClaimState::Stopping);
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
        let mut claims = CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
        for k in &self.keys {
            claims.insert(k.clone(), ClaimState::Running);
        }
    }

    fn set_stopping(&self) {
        let mut claims = CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
        for k in &self.keys {
            claims.insert(k.clone(), ClaimState::Stopping);
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut claims = CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
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
        let (ldk_seed, store_id) = derive_keys(&taken, cfg.network)?;
        drop(taken);

        let node_id = ldk_node_id(&ldk_seed, cfg.network)?.to_string();

        let lsp_id: PublicKey = cfg
            .lsp
            .node_id
            .parse()
            .map_err(|e| LnError::Other(format!("lsp node id: {e}")))?;
        if cfg.chain == LnChain::Xbt && matches!(cfg.chain_source, ChainSource::BitcoindRpc { .. })
        {
            return Err(LnError::Config(
                "xbt requires an esplora chain source".into(),
            ));
        }
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
            chain: cfg.chain,
            vss: cfg.vss.clone(),
            limits: cfg.limits,
            lsp_id,
            lsp_addr,
            lsp_host: cfg.lsp.host,
            lsp_port: cfg.lsp.port,
            lsp_token: cfg.lsp.token,
            ldk_seed,
            store_id: store_id.clone(),
            node_id,
            claim_keys,
            shared: Arc::new(Shared {
                inner: Mutex::new(None),
                generation: AtomicU64::new(0),
                lock: Mutex::new(None),
                lock_lost: AtomicBool::new(false),
                checker: Mutex::new(None),
                busy: Arc::new(Busy::default()),
                vss: cfg.vss.clone(),
                store_id: store_id.clone(),
                vss_client: Mutex::new(None),
            }),
            channel_ops: Mutex::new(()),
        })
    }

    /// Hex compressed public key; known before `start`.
    pub fn node_id(&self) -> String {
        self.node_id.clone()
    }

    pub fn limits(&self) -> LspLimits {
        self.limits
    }

    /// What ldk-node holds back in the transit wallet per anchor channel: 0,
    /// as the LSP is a trusted peer (`trusted_peers_no_reserve`; see the
    /// channel module docs).
    pub fn anchor_reserve_sat() -> u64 {
        0
    }

    /// [`start_with`](Self::start_with)`(StartMode::Normal)`.
    pub fn start(&self) -> Result<(), LnError> {
        self.start_with(StartMode::Normal)
    }

    /// Connects to VSS, the chain source and (Normal mode: required; Exit
    /// mode: best effort) the LSP, and starts the node. Never blocks longer
    /// than 15 s, in either mode. `Ok` when already running, in whatever
    /// mode it runs (also when a concurrent start on this `Node` got there
    /// first): `stop()` first to change the mode, see
    /// [`start_mode`](Self::start_mode). `Other("already_running")` when
    /// another `Node` in this process runs this wallet;
    /// `Other("start_in_progress")` when another start for it is still
    /// working; `Other("cancelled")` when `stop()` came first.
    ///
    /// **Device lock** (module docs): taken before ldk-node is built; free
    /// or held by this device (its own crash restart) → taken; held by
    /// another device → `ActiveOnAnotherDevice` and nothing is built (offer
    /// [`takeover`](Self::takeover)); an unparsable lock record →
    /// `Other("device_lock_corrupt")` (also resolved by `takeover`); a
    /// device id file in the data dir that is not valid →
    /// `Other("device_id_corrupt")`.
    pub fn start_with(&self, mode: StartMode) -> Result<(), LnError> {
        if self.running().is_some() {
            return Ok(());
        }
        self.start_impl(mode, LockMode::Acquire)
    }

    /// "Use Lightning here instead": takes the device lock from whichever
    /// device holds it (read the lock, write ours at the version read — of
    /// two concurrent takeovers exactly one write lands, the other gets
    /// `ActiveOnAnotherDevice`), then starts as
    /// [`start_with`](Self::start_with)`(mode)`, under the same 15 s bound
    /// and errors. Only when this node is not running:
    /// `Other("already_running")` otherwise.
    ///
    /// **The old device's window.** VSS cannot stop the old device's
    /// ldk-node: its writes are unconditional (spec, Findings (b)). It
    /// notices the takeover at its next check — before its next payment,
    /// channel or transit action, or by its background check within 60 s —
    /// and then stops itself (`DeviceLockLost`). Until then it may still
    /// write channel state over this device's, e.g. when it forwards or
    /// claims an incoming payment. The app must tell the user to close
    /// Lightning on the old phone first and take over only when it is lost
    /// or broken.
    pub fn takeover(&self, mode: StartMode) -> Result<(), LnError> {
        if self.running().is_some() {
            return Err(LnError::Other("already_running".into()));
        }
        self.start_impl(mode, LockMode::Takeover)
    }

    fn start_impl(&self, mode: StartMode, lock_mode: LockMode) -> Result<(), LnError> {
        let generation = self.shared.generation.load(Ordering::SeqCst);
        let deadline = Instant::now() + START_DEADLINE;
        let stage = Arc::new(AtomicU8::new(Stage::Claim as u8));
        let (tx, rx) = mpsc::channel::<Result<(), LnError>>();
        let job = StartJob {
            network: self.network,
            data_dir: self.data_dir.to_string_lossy().into_owned(),
            chain_source: self.chain_source.clone(),
            chain: self.chain,
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
            mode,
            lock_mode,
            deadline,
            stage: Arc::clone(&stage),
            shared: Arc::clone(&self.shared),
            generation,
        };
        let busy = BusyToken::new(&self.shared.busy);
        std::thread::Builder::new()
            .name("fortis-ln-start".into())
            .spawn(move || {
                let _busy = busy;
                let _ = tx.send(job.run());
            })
            .map_err(|e| LnError::Other(format!("spawn: {e}")))?;

        let wait = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(res) => res,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Same lock as the worker's install: either it installed
                // already, or it never will.
                let inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
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
    ///
    /// Then the device lock is released (≤ 3 s; when VSS does not answer it
    /// stays this device's: this device restarts as usual, another one
    /// needs [`takeover`](Self::takeover)), and `stop` waits (≤ 10 s) for a
    /// cancelled start and the background check to finish, see
    /// [`is_quiescent`](Self::is_quiescent).
    pub fn stop(&self) {
        let running = {
            let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
            self.shared.generation.fetch_add(1, Ordering::SeqCst);
            drop(self.shared.checker.lock().unwrap_or_else(|e| e.into_inner()).take());
            let r = inner.take();
            if let Some(ref r) = r {
                r._claim.set_stopping();
            }
            r
        };
        // Outside the lock; the claim goes when the last `Arc` drops, after
        // the release (so no other start in this process takes the lock
        // in between).
        if let Some(r) = running {
            let _ = r.node.stop();
            release_lock(&self.shared);
            drop(r);
        }
        self.shared.busy.wait_idle(QUIESCE_TIMEOUT);
    }

    /// No node running and nothing of this `Node` still working in the
    /// background (start worker, lock check, a stop after a lost lock), so
    /// its data dir may be deleted or another start/takeover is not racing
    /// a stale one. `stop()` waits for this (bounded); poll it after a
    /// `stop()` that returned early or after `DeviceLockLost`.
    pub fn is_quiescent(&self) -> bool {
        self.shared.inner.lock().unwrap_or_else(|e| e.into_inner()).is_none() && self.shared.busy.idle()
    }

    /// The mode the running node was started in; `None` when not running.
    pub fn start_mode(&self) -> Option<StartMode> {
        self.running().map(|r| r.mode)
    }

    /// Zero when not running.
    pub fn balances(&self) -> Balances {
        let Some(node) = self.running() else {
            return Balances::default();
        };
        crate::channel::balances_of(&node.node, &self.data_dir, self.chain_kind())
    }

    /// The next event, waiting up to `timeout`. Events are acknowledged to
    /// ldk-node as they are returned; ones fortis doesn't surface are skipped.
    /// `None` at once when the node is not running.
    pub fn next_event(&self, timeout: Duration) -> Option<LnEvent> {
        let deadline = Instant::now() + timeout;
        // The event whose close was already recorded in this call: a failed
        // `event_handled` makes ldk-node hand the same event out again.
        let mut recorded_close = None;
        loop {
            if self.shared.lock_lost.swap(false, Ordering::SeqCst) {
                return Some(LnEvent::DeviceLockLost);
            }
            let running = self.running()?;
            let node = &running.node;
            if let Some(event) = node.next_event() {
                if let Event::ChannelClosed {
                    channel_id, reason, ..
                } = &event
                {
                    if recorded_close != Some(*channel_id) {
                        crate::channel::record_closed_event(
                            &self.data_dir,
                            channel_id,
                            reason.as_ref(),
                        );
                        recorded_close = Some(*channel_id);
                    }
                }
                // On failure ldk-node re-delivers the event; don't surface it
                // twice. Retry only within `timeout` and not in a hot loop
                // (VSS down keeps `event_handled` failing): the caller polls
                // again, and everything else it does must not wait on this.
                if node.event_handled().is_err() {
                    let now = Instant::now();
                    if now >= deadline {
                        return None;
                    }
                    std::thread::sleep((deadline - now).min(Duration::from_millis(100)));
                    continue;
                }
                if let Some(e) = translate(event) {
                    // LDK hands out `PaymentClaimed` again at every start until
                    // the channel has fully settled the claim, so a payment
                    // received yesterday would be announced again today.
                    if let LnEvent::PaymentReceived { id, .. } = &e {
                        if !first_report_of_received(&self.data_dir, id) {
                            continue;
                        }
                    }
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
        self.shared.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn network(&self) -> Network {
        self.network
    }

    /// The lock serialising `open_channel`, `close_channel`,
    /// `force_close_channel` and `sweep_transit`.
    pub(crate) fn channel_ops(&self) -> std::sync::MutexGuard<'_, ()> {
        self.channel_ops.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The LSP's node id and address.
    pub(crate) fn lsp_peer(&self) -> (PublicKey, SocketAddress) {
        (self.lsp_id, self.lsp_addr.clone())
    }

    /// The chain this node runs on (BTC or XBT).
    pub(crate) fn ln_chain(&self) -> LnChain {
        self.chain
    }

    /// The chain source's kind, for the void rule (`channel.rs`).
    pub(crate) fn chain_kind(&self) -> crate::channel::ChainKind {
        match self.chain_source {
            ChainSource::BitcoindRpc { .. } => crate::channel::ChainKind::Bitcoind,
            ChainSource::Esplora { .. } => crate::channel::ChainKind::Esplora,
        }
    }

    pub(crate) fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Exit mode only: `LspUnreachable` unless the LSP peer is connected
    /// right now (ldk-node `list_peers`, no network wait). Called before any
    /// LSP-dependent work by `receive`, `pay`, `open_channel` and
    /// `close_channel`; in Normal mode always `Ok` (their own handling is
    /// unchanged).
    pub(crate) fn exit_gate(&self, running: &Running) -> Result<(), LnError> {
        if running.mode == StartMode::Exit && !peer_connected(&running.node, &self.lsp_id) {
            return Err(LnError::LspUnreachable);
        }
        Ok(())
    }

    /// The device-lock check before an action (`pre_send_checks`): one
    /// conditional rewrite of the lock, at most `timeout`, which doubles as
    /// the VSS reachability check. `BackupUnavailable` when VSS does not
    /// answer (the node keeps running); on a lost lock the node is stopped
    /// (on another thread: callers may hold `channel_ops`),
    /// `DeviceLockLost` is queued and the error is `ActiveOnAnotherDevice`
    /// (or `Other("device_lock_corrupt")`). `Other("not_started")` when not
    /// running.
    pub(crate) fn check_device_lock(&self, timeout: Duration) -> Result<(), LnError> {
        let running = self.running().ok_or_else(|| LnError::Other("not_started".into()))?;
        let generation = running.generation;
        drop(running);
        self.check_lock_at(generation, timeout)
    }

    /// Unit tests: as if a start of generation 0 had taken the lock.
    #[cfg(test)]
    pub(crate) fn hold_lock_for_test(&self) {
        *self.shared.lock.lock().unwrap() = Some(HeldLock {
            me: DeviceId::TEST,
            held: Held { version: 1, since: 0 },
        });
    }

    pub(crate) fn check_lock_at(&self, generation: u64, timeout: Duration) -> Result<(), LnError> {
        match refresh_lock(&self.shared, generation, timeout) {
            Err(e) if device::is_lost(&e) => {
                if let Some(r) = lose_lock(&self.shared, generation) {
                    let busy = BusyToken::new(&self.shared.busy);
                    let spawned = std::thread::Builder::new()
                        .name("fortis-ln-lock-lost".into())
                        .spawn(move || {
                            let _busy = busy;
                            let _ = r.node.stop();
                        });
                    // No thread: the node stops when the last `Arc` drops
                    // (`Live`), normally the caller's, after its locks.
                    drop(spawned);
                }
                Err(e)
            }
            other => other,
        }
    }
}

/// One check of the lock held for `generation` (see
/// [`Node::check_device_lock`]); on a lost lock the slot is cleared, the
/// caller stops the node ([`lose_lock`]).
fn refresh_lock(shared: &Shared, generation: u64, timeout: Duration) -> Result<(), LnError> {
    let mut slot = shared.lock.lock().unwrap_or_else(|e| e.into_inner());
    let not_started = || LnError::Other("not_started".into());
    if shared.generation.load(Ordering::SeqCst) != generation {
        return Err(not_started());
    }
    let Some(mut h) = *slot else {
        return Err(not_started());
    };
    let client = shared.vss_client()?;
    let store = client.with_timeout(timeout);
    match device::refresh(&store, h.me, &mut h.held) {
        Ok(_) => {
            *slot = Some(h);
            Ok(())
        }
        Err(e) => {
            if device::is_lost(&e) {
                *slot = None;
            } else {
                *slot = Some(h);
            }
            Err(e)
        }
    }
}

/// The lock of run `generation` is lost: uninstalls that run (cancelling
/// its background check) and queues `DeviceLockLost`, once. Returns the
/// run for the caller to stop, outside every lock.
fn lose_lock(shared: &Shared, generation: u64) -> Option<Arc<Running>> {
    let mut inner = shared.inner.lock().unwrap_or_else(|e| e.into_inner());
    if shared.generation.load(Ordering::SeqCst) != generation || inner.is_none() {
        return None;
    }
    shared.generation.fetch_add(1, Ordering::SeqCst);
    drop(shared.checker.lock().unwrap_or_else(|e| e.into_inner()).take());
    shared.lock_lost.store(true, Ordering::SeqCst);
    let r = inner.take();
    if let Some(ref r) = r {
        r._claim.set_stopping();
    }
    r
}

/// Releases the lock this node holds, if any (best effort, ≤ 3 s).
fn release_lock(shared: &Shared) {
    release_lock_if(shared, None);
}

/// [`release_lock`], only if what is held is `only` (when given): a start
/// worker releases its own acquire, nothing newer.
fn release_lock_if(shared: &Shared, only: Option<Held>) {
    let mut slot = shared.lock.lock().unwrap_or_else(|e| e.into_inner());
    let Some(h) = *slot else { return };
    if only.is_some_and(|o| o != h.held) {
        return;
    }
    *slot = None;
    if let Ok(client) = shared.vss_client() {
        let store = client.with_timeout(LOCK_RELEASE_TIMEOUT);
        let _ = device::release(&store, h.held);
    }
}

/// The background check of run `generation`: every [`LOCK_CHECK_INTERVAL`]
/// a [`refresh_lock`]; on a lost lock it stops the node itself. Holds the
/// node only as a `Weak` (upgraded for one check at a time); ends when
/// `cancel`'s sender is dropped (`stop()`, a lost lock) or the generation
/// moves on. VSS not answering changes nothing (the next action's check
/// refuses it).
fn spawn_lock_checker(shared: &Arc<Shared>, generation: u64) {
    let (tx, cancel) = mpsc::channel::<()>();
    let weak: Weak<Shared> = Arc::downgrade(shared);
    let busy = BusyToken::new(&shared.busy);
    {
        let inner = shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        if shared.generation.load(Ordering::SeqCst) != generation || inner.is_none() {
            return;
        }
        *shared.checker.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }
    let spawned = std::thread::Builder::new()
        .name("fortis-ln-lock-check".into())
        .spawn(move || {
            let _busy = busy;
            loop {
                match cancel.recv_timeout(test_hooks::lock_check_interval()) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    _ => return,
                }
                let Some(shared) = weak.upgrade() else { return };
                match refresh_lock(&shared, generation, LOCK_CHECK_TIMEOUT) {
                    Ok(()) | Err(LnError::BackupUnavailable) => {}
                    Err(e) if device::is_lost(&e) => {
                        let lost = lose_lock(&shared, generation);
                        drop(shared);
                        if let Some(r) = lost {
                            let _ = r.node.stop();
                        }
                        return;
                    }
                    // Not running any more (stopped, or another run).
                    Err(_) => return,
                }
            }
        });
    if spawned.is_err() {
        // No background check: the checks before each action remain.
        drop(shared.checker.lock().unwrap_or_else(|e| e.into_inner()).take());
    }
}

/// The node seed (ldk-node's entropy) and VSS store id for a BIP-39 seed
/// (module docs).
fn derive_keys(bip39: &[u8], network: Network) -> Result<(Zeroizing<[u8; 64]>, String), LnError> {
    if bip39.len() != 64 {
        return Err(LnError::Other("bip39_seed must be 64 bytes".into()));
    }
    let mut bip39_seed = Zeroizing::new([0u8; 64]);
    bip39_seed.copy_from_slice(bip39);
    let node_seed = wallet_core::bip85_hex(&bip39_seed, network, 64, 0);
    drop(bip39_seed);
    let mut ldk_seed = Zeroizing::new([0u8; 64]);
    ldk_seed.copy_from_slice(&node_seed);
    drop(node_seed);
    let mut e = hmac::HmacEngine::<sha256::Hash>::new(&ldk_seed[..]);
    e.input(VSS_STORE_TAG);
    let store_id = hex::encode(hmac::Hmac::<sha256::Hash>::from_engine(e).to_byte_array());
    Ok((ldk_seed, store_id))
}

/// Whether the VSS store of the wallet with this BIP-39 seed holds any
/// node state (anything but the device lock), for a restore flow to choose
/// between "restore from backup" (`true`: start a `Node`, ldk-node loads
/// the channels) and "scan for force-closed funds" (`false` on a wallet
/// that had Lightning: its VSS data is lost, use the seed-only sweeper,
/// [`crate::recover`]). One VSS round trip, ≤ 5 s; `BackupUnavailable`
/// when VSS does not answer — never a guess.
pub fn vss_state_exists(vss: &VssConfig, bip39_seed: &[u8], network: Network) -> Result<bool, LnError> {
    let (ldk_seed, store_id) = derive_keys(bip39_seed, network)?;
    drop(ldk_seed);
    let client = VssLockClient::new(vss, &store_id)?;
    let store = client.with_timeout(PROBE_TIMEOUT);
    device::state_exists(&store)
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Whether ldk-node has a live connection to `id`.
pub(crate) fn peer_connected(node: &ldk_node::Node, id: &PublicKey) -> bool {
    node.list_peers()
        .iter()
        .any(|p| p.node_id == *id && p.is_connected)
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

/// The file listing received payments already reported (one id per line).
const RECEIVED_REPORTED_FILE: &str = "received_reported";

/// True the first time `id` is seen here, and records it; false for a payment
/// already reported. Files that cannot be read or written count as unseen
/// (a repeated notice is better than a lost one).
fn first_report_of_received(data_dir: &std::path::Path, id: &str) -> bool {
    use std::io::Write;
    let path = data_dir.join(RECEIVED_REPORTED_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        if text.lines().any(|l| l == id) {
            return false;
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{id}");
    }
    true
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
    /// Exit mode's best-effort LSP probe and connect.
    ExitConnect = 6,
}

impl From<u8> for Stage {
    fn from(v: u8) -> Stage {
        match v {
            0 => Stage::Claim,
            1 => Stage::Lsp,
            2 => Stage::Vss,
            3 => Stage::Build,
            4 => Stage::Start,
            5 => Stage::Connect,
            _ => Stage::ExitConnect,
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
            // Not reached: the node is installed before this stage, so a
            // caller timing out then returns `Ok`.
            Stage::ExitConnect => LnError::Other("start_timeout".into()),
        }
    }
}

struct StartJob {
    network: Network,
    data_dir: String,
    chain_source: ChainSource,
    chain: LnChain,
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
    mode: StartMode,
    lock_mode: LockMode,
    deadline: Instant,
    stage: Arc<AtomicU8>,
    shared: Arc<Shared>,
    generation: u64,
}

/// How a start worker takes the device lock.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LockMode {
    /// Free or ours → take it; another device's → `ActiveOnAnotherDevice`.
    Acquire,
    /// Whoever holds it ([`Node::takeover`]).
    Takeover,
}

/// The device lock a start worker took: released (best effort) on drop
/// unless the worker installed its node, which then owns it.
struct LockGuard {
    shared: Arc<Shared>,
    held: Held,
    armed: bool,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if self.armed {
            release_lock_if(&self.shared, Some(self.held));
        }
    }
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
        self.log_timing(&format!("stage {}", s as u8));
    }

    /// Appends "<ms since this start began> <what>" to `start_timing.log` in
    /// the data dir (best effort), to see where a slow start spends its time.
    fn log_timing(&self, what: &str) {
        use std::io::Write;
        let began = self.deadline - START_DEADLINE;
        let ms = Instant::now().saturating_duration_since(began).as_millis();
        let path = std::path::Path::new(&self.data_dir).join("start_timing.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let _ = writeln!(f, "{secs} +{ms}ms {what}");
        }
    }

    fn probe_timeout(&self) -> Duration {
        self.deadline
            .saturating_duration_since(Instant::now())
            .min(PROBE_TIMEOUT)
            .max(Duration::from_millis(1))
    }

    fn vss_headers(&self) -> HashMap<String, String> {
        device::vss_headers(&self.vss)
    }

    /// Stage 2: the device lock (module docs), before anything is built.
    fn take_lock(&self) -> Result<LockGuard, LnError> {
        let me = device::load_or_create_device_id(std::path::Path::new(&self.data_dir))?;
        let client = self.shared.vss_client()?;
        let store = client.with_timeout(self.probe_timeout());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut slot = self.shared.lock.lock().unwrap_or_else(|e| e.into_inner());
        let held = match self.lock_mode {
            LockMode::Acquire => device::acquire(&store, me, now)?,
            LockMode::Takeover => device::takeover(&store, me, now, test_hooks::takeover_barrier)?,
        };
        *slot = Some(HeldLock { me, held });
        Ok(LockGuard {
            shared: Arc::clone(&self.shared),
            held,
            armed: true,
        })
    }

    /// `Some(outcome)` when the worker must stop: token stale or a node
    /// already installed.
    fn checkpoint(&self) -> Option<Halt> {
        if self.shared.generation.load(Ordering::SeqCst) != self.generation {
            return Some(Err(cancelled()));
        }
        if self.shared.inner.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
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
                    if self.lock_mode == LockMode::Acquire
                        && self.shared.inner.lock().unwrap_or_else(|e| e.into_inner()).is_some()
                    {
                        return Err(Ok(()));
                    }
                    return Err(Err(LnError::Other("already_running".into())));
                }
                Err(ClaimState::Starting | ClaimState::Stopping)
                    if Instant::now() >= self.deadline =>
                {
                    return Err(Err(LnError::Other("start_in_progress".into())));
                }
                Err(ClaimState::Starting | ClaimState::Stopping) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    fn run(self) -> Result<(), LnError> {
        self.set(Stage::Claim);
        let claim = match self.claim() {
            Ok(c) => c,
            Err(halt) => return halt,
        };
        if self.mode == StartMode::Normal {
            self.set(Stage::Lsp);
            self.probe_lsp()?;
        }
        if let Some(halt) = self.checkpoint() {
            return halt;
        }
        self.set(Stage::Vss);
        // Dropped before `claim` (declared after it): a worker that gives up
        // releases the lock while it still holds the claim.
        let mut lock = self.take_lock()?;

        if let Some(halt) = self.checkpoint() {
            return halt;
        }
        self.set(Stage::Build);
        // The LSP gets closes confirmed for us: no 25,000 sat anchor reserve.
        let settings = ldk_settings(self.chain, self.network, self.lsp_id);
        let mut config = ldk_node::config::Config::default();
        if settings.anchors_trust_lsp {
            if let Some(a) = config.anchor_channels_config.as_mut() {
                a.trusted_peers_no_reserve.push(self.lsp_id);
            }
        }
        // The builder drops anchors itself when `xbt` is set.
        config.xbt = settings.xbt;
        let mut b = Builder::from_config(config);
        b.set_network(self.network);
        match &self.chain_source {
            ChainSource::Esplora { url, auth_header } => match auth_header {
                Some(h) if !h.is_empty() => {
                    let headers = std::collections::HashMap::from([(
                        "Authorization".to_string(),
                        h.clone(),
                    )]);
                    b.set_chain_source_esplora_with_headers(url.clone(), headers, None);
                }
                _ => {
                    b.set_chain_source_esplora(url.clone(), None);
                }
            },
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
        // The network graph from Rapid Gossip Sync (one compact HTTPS snapshot,
        // in the background), not P2P gossip: that streamed the LSP's whole
        // graph down the same connection on every connect, holding up the
        // payments queued behind it by seconds. Mainnet only (no RGS server for
        // test networks; a node there routes over its own channels).
        // XBT has no RGS server: it keeps the builder's default P2P gossip.
        if let Some(url) = settings.rgs_url {
            b.set_gossip_source_rgs(url);
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
        if self.mode == StartMode::Normal {
            self.set(Stage::Connect);
            if let Err(e) = node.connect(self.lsp_id, self.lsp_addr.clone(), true) {
                return Err(match e {
                    NodeError::PersistenceFailed => LnError::BackupUnavailable,
                    _ => LnError::LspUnreachable,
                });
            }
        }

        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = if self.shared.generation.load(Ordering::SeqCst) != self.generation {
            Err(cancelled())
        } else if inner.is_some() {
            Ok(())
        } else {
            claim.set_running();
            let running = Arc::new(Running {
                node,
                mode: self.mode,
                generation: self.generation,
                _claim: claim,
            });
            let weak = Arc::downgrade(&running);
            *inner = Some(running);
            // The installed node owns the lock now (`stop()` releases it).
            lock.armed = false;
            drop(inner);
            spawn_lock_checker(&self.shared, self.generation);
            self.log_timing("running");
            if self.mode == StartMode::Exit {
                self.exit_connect(&weak);
            }
            return Ok(());
        };
        drop(inner);
        drop(node); // stops it, then the lock and the claim are released
        drop(lock);
        drop(claim);
        outcome
    }

    /// Exit mode, after the node is installed (a caller timing out now gets
    /// `Ok`): a best-effort connection to the LSP, never an error. Only when
    /// the LSP answers a TCP probe within the time left, so a down LSP costs
    /// no `connect` wait (up to 10 s in lightning-net-tokio); a handshake
    /// that stalls just runs past the caller's deadline here (ldk-node drops
    /// it after a timer tick, or `stop()` disconnects it). The node is held
    /// only for the `connect` call itself.
    fn exit_connect(&self, running: &std::sync::Weak<Running>) {
        self.set(Stage::ExitConnect);
        if self.probe_lsp_within(EXIT_PROBE_TIMEOUT).is_err() {
            return;
        }
        if self.shared.generation.load(Ordering::SeqCst) != self.generation {
            return;
        }
        if let Some(r) = running.upgrade() {
            let _ = r.node.connect(self.lsp_id, self.lsp_addr.clone(), true);
        }
    }

    fn probe_lsp(&self) -> Result<(), LnError> {
        self.probe_lsp_within(PROBE_TIMEOUT)
    }

    /// TCP connect to the LSP, each attempt at most `cap` and the time left.
    fn probe_lsp_within(&self, cap: Duration) -> Result<(), LnError> {
        let addrs: Vec<_> = (self.lsp_host.as_str(), self.lsp_port)
            .to_socket_addrs()
            .map_err(|_| LnError::LspUnreachable)?
            .collect();
        for addr in addrs {
            if TcpStream::connect_timeout(&addr, self.probe_timeout().min(cap)).is_ok() {
                return Ok(());
            }
        }
        Err(LnError::LspUnreachable)
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

#[cfg(test)]
mod tests {
    #[test]
    fn a_received_payment_is_reported_once() {
        let dir = std::env::temp_dir().join(format!("fortis-rx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(super::first_report_of_received(&dir, "aa"));
        assert!(!super::first_report_of_received(&dir, "aa"));
        assert!(super::first_report_of_received(&dir, "bb"));
        assert!(!super::first_report_of_received(&dir, "bb"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    #[test]
    fn chain_source_debug_never_shows_a_token_or_password() {
        let e = ChainSource::Esplora {
            url: "https://edge/btc".into(),
            auth_header: Some("Bearer secret-token".into()),
        };
        let b = ChainSource::BitcoindRpc {
            host: "h".into(),
            port: 1,
            user: "u".into(),
            password: "secret-pw".into(),
        };
        let shown = format!("{e:?} {b:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("https://edge/btc"));
    }

    #[test]
    fn anchor_reserve_matches_ldk_nodes_default() {
        assert_eq!(Node::anchor_reserve_sat(), 0);
    }

    fn cfg(seed: Vec<u8>) -> NodeConfig {
        NodeConfig {
            bip39_seed: seed,
            network: Network::Regtest,
            data_dir: PathBuf::from("unused"),
            chain_source: ChainSource::Esplora {
                url: "http://192.0.2.1".into(),
                auth_header: None,
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
            chain: LnChain::Btc,
        }
    }

    fn lsp_key() -> PublicKey {
        "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
            .parse()
            .unwrap()
    }

    #[test]
    fn btc_settings_unchanged() {
        let id = lsp_key();
        let main = ldk_settings(LnChain::Btc, Network::Bitcoin, id);
        assert_eq!(main.xbt, None);
        assert_eq!(main.rgs_url.as_deref(), Some(MAINNET_RGS_URL));
        assert!(main.anchors_trust_lsp);
        let reg = ldk_settings(LnChain::Btc, Network::Regtest, id);
        assert_eq!(reg.rgs_url, None);
        assert!(reg.anchors_trust_lsp);
    }

    #[test]
    fn xbt_settings() {
        let id = lsp_key();
        // The gossip floor is the network's BLAKE2b activation height:
        // 961,640 on mainnet, 20 on regtest (the XBT regtest gate's Knots).
        for (net, floor) in [(Network::Bitcoin, 961640), (Network::Regtest, 20)] {
            let s = ldk_settings(LnChain::Xbt, net, id);
            assert_eq!(
                s.xbt,
                Some(ldk_node::config::XbtConfig {
                    gossip_floor_height: floor,
                    splice_peers: vec![id],
                    splice_all_peers: false,
                })
            );
            assert_eq!(s.rgs_url, None);
            assert!(!s.anchors_trust_lsp);
        }
    }

    #[test]
    fn xbt_with_bitcoind_rpc_is_a_config_error() {
        let mut c = cfg(vec![9u8; 64]);
        c.chain = LnChain::Xbt;
        c.chain_source = ChainSource::BitcoindRpc {
            host: "h".into(),
            port: 1,
            user: "u".into(),
            password: "p".into(),
        };
        assert!(matches!(Node::new(c), Err(LnError::Config(_))));
        let mut ok = cfg(vec![9u8; 64]);
        ok.chain = LnChain::Xbt;
        assert!(Node::new(ok).is_ok());
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
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::Duration;

    pub(crate) static BUILDS: AtomicU64 = AtomicU64::new(0);
    pub(crate) static LIVE: AtomicU64 = AtomicU64::new(0);
    pub(crate) static MAX_LIVE: AtomicU64 = AtomicU64::new(0);
    static LOCK_CHECK_MS: AtomicU64 =
        AtomicU64::new(super::LOCK_CHECK_INTERVAL.as_millis() as u64);
    static TAKEOVER_BARRIER: Mutex<Option<Arc<Barrier>>> = Mutex::new(None);

    /// The device lock's key in the node's VSS store.
    pub const DEVICE_LOCK_KEY: &str = crate::device::LOCK_KEY;
    /// The production background-check interval.
    pub const DEFAULT_LOCK_CHECK_INTERVAL: Duration = super::LOCK_CHECK_INTERVAL;

    /// Sets the background lock-check interval for checkers started from
    /// now on (each reads it before every wait).
    pub fn set_lock_check_interval(d: Duration) {
        LOCK_CHECK_MS.store(d.as_millis().max(1) as u64, Ordering::SeqCst);
    }

    pub(crate) fn lock_check_interval() -> Duration {
        Duration::from_millis(LOCK_CHECK_MS.load(Ordering::SeqCst))
    }

    /// Every `takeover` waits on `b` between reading the lock and writing
    /// it (so concurrent takeovers all read the same version).
    pub fn set_takeover_barrier(b: Option<Arc<Barrier>>) {
        *TAKEOVER_BARRIER.lock().unwrap() = b;
    }

    pub(crate) fn takeover_barrier() {
        let b = TAKEOVER_BARRIER.lock().unwrap().clone();
        if let Some(b) = b {
            b.wait();
        }
    }

    /// A crash: the node stops without releasing its device lock (the
    /// process is gone; the lock stays in VSS with this device's id).
    pub fn crash(node: &crate::Node) {
        *node.shared.lock.lock().unwrap() = None;
        node.stop();
    }

    /// Gives `node` its own process-wide claim scope for its store, so a
    /// test can run a second "device" for the same seed in one process
    /// (the claim, Task 4, would refuse it before it reaches VSS; across
    /// real devices it never applies).
    pub fn own_claims(node: &mut crate::Node, tag: &str) {
        node.claim_keys[0] = format!("store:{}@{tag}", node.store_id);
    }

    /// The node's VSS store id.
    pub fn store_id(node: &crate::Node) -> String {
        node.store_id.clone()
    }

    /// ldk-node builds in this process so far.
    pub fn builds() -> u64 {
        BUILDS.load(Ordering::SeqCst)
    }

    /// Most fortis ldk-nodes alive at once since the last [`reset_max_live`].
    pub fn max_live() -> u64 {
        MAX_LIVE.load(Ordering::SeqCst)
    }

    /// ldk-node's `total_onchain_balance_sats` (unconfirmed included), so a
    /// test can wait until the wallet has seen a mempool transaction.
    pub fn wallet_total_sat(node: &crate::Node) -> u64 {
        node.running()
            .map_or(0, |r| r.node.list_balances().total_onchain_balance_sats)
    }

    /// What `pay` checks an amount against (its `InsufficientLightning`
    /// step); 0 when not running.
    pub fn sendable_msat(node: &crate::Node) -> u64 {
        node.running()
            .map_or(0, |r| crate::pay::sendable_msat(&r.node))
    }

    /// Resets [`max_live`] to the current count.
    pub fn reset_max_live() {
        MAX_LIVE.store(LIVE.load(Ordering::SeqCst), Ordering::SeqCst);
    }
}
