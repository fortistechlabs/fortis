//! UniFFI bindings for [`fortis_lightning`], gated behind `feature = "lightning"`.
//!
//! No `ldk-node` or `lightning` types cross this boundary: every record, enum,
//! and error is a plain UniFFI type that maps one-to-one onto
//! `fortis-lightning`'s public API.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bip39::Mnemonic;
use fortis_lightning::ldk_node::bitcoin::{Address, Network, OutPoint, Txid};
use fortis_lightning::lnurl::{self, LnurlPayParams, UreqGet};
use fortis_lightning::recover::{self, EsploraBroadcast, EsploraScan, StaticUtxo};
use fortis_lightning::{
    vss_state_exists, Balances, ChainSource, ChannelState, Direction, LnError, LnEvent, LspInfo,
    LspLimits, Node, NodeConfig, ParseError, Payment, PaymentTarget, ReceiveQuote, StartMode,
    Status, VssConfig,
};
use zeroize::Zeroizing;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum LnFfiError {
    #[error("backup_unavailable")]
    BackupUnavailable,
    #[error("active_on_another_device")]
    ActiveOnAnotherDevice,
    #[error("lsp_unreachable")]
    LspUnreachable,
    #[error("insufficient_lightning")]
    InsufficientLightning,
    #[error("no_route")]
    NoRoute,
    #[error("invoice_expired")]
    InvoiceExpired,
    #[error("wrong_network")]
    WrongNetwork,
    #[error("payment_state_unknown")]
    PaymentStateUnknown,
    #[error("amount_out_of_range")]
    AmountOutOfRange { min_msat: u64, max_msat: u64 },
    #[error("other: {reason}")]
    Other { reason: String },
}

impl From<LnError> for LnFfiError {
    fn from(err: LnError) -> Self {
        match err {
            LnError::BackupUnavailable => LnFfiError::BackupUnavailable,
            LnError::ActiveOnAnotherDevice => LnFfiError::ActiveOnAnotherDevice,
            LnError::LspUnreachable => LnFfiError::LspUnreachable,
            LnError::InsufficientLightning => LnFfiError::InsufficientLightning,
            LnError::NoRoute => LnFfiError::NoRoute,
            LnError::InvoiceExpired => LnFfiError::InvoiceExpired,
            LnError::WrongNetwork => LnFfiError::WrongNetwork,
            LnError::PaymentStateUnknown => LnFfiError::PaymentStateUnknown,
            LnError::AmountOutOfRange { min_msat, max_msat } => {
                LnFfiError::AmountOutOfRange { min_msat, max_msat }
            }
            LnError::Other(reason) => LnFfiError::Other { reason },
        }
    }
}

impl From<LnFfiError> for LnError {
    fn from(err: LnFfiError) -> Self {
        match err {
            LnFfiError::BackupUnavailable => LnError::BackupUnavailable,
            LnFfiError::ActiveOnAnotherDevice => LnError::ActiveOnAnotherDevice,
            LnFfiError::LspUnreachable => LnError::LspUnreachable,
            LnFfiError::InsufficientLightning => LnError::InsufficientLightning,
            LnFfiError::NoRoute => LnError::NoRoute,
            LnFfiError::InvoiceExpired => LnError::InvoiceExpired,
            LnFfiError::WrongNetwork => LnError::WrongNetwork,
            LnFfiError::PaymentStateUnknown => LnError::PaymentStateUnknown,
            LnFfiError::AmountOutOfRange { min_msat, max_msat } => {
                LnError::AmountOutOfRange { min_msat, max_msat }
            }
            LnFfiError::Other { reason } => LnError::Other(reason),
        }
    }
}

impl From<ParseError> for LnFfiError {
    fn from(err: ParseError) -> Self {
        match err {
            ParseError::WrongNetwork => LnFfiError::WrongNetwork,
            ParseError::Expired => LnFfiError::InvoiceExpired,
            other => LnFfiError::Other {
                reason: other.to_string(),
            },
        }
    }
}

pub(crate) fn parse_network(network: &str) -> Result<Network, LnFfiError> {
    match network.trim().to_ascii_lowercase().as_str() {
        "bitcoin" | "mainnet" => Ok(Network::Bitcoin),
        "regtest" => Ok(Network::Regtest),
        "testnet" | "testnet3" => Ok(Network::Testnet),
        "testnet4" => Ok(Network::Testnet4),
        "signet" => Ok(Network::Signet),
        other => Err(LnFfiError::Other {
            reason: format!("unknown network {other}"),
        }),
    }
}

// ---------------------------------------------------------------------------
// Config & state types
// ---------------------------------------------------------------------------

#[derive(uniffi::Record, Debug, Clone, PartialEq, Eq)]
pub struct LnLspInfo {
    pub node_id: String,
    pub host: String,
    pub port: u16,
    pub token: Option<String>,
}

impl From<LnLspInfo> for LspInfo {
    fn from(v: LnLspInfo) -> Self {
        LspInfo {
            node_id: v.node_id,
            host: v.host,
            port: v.port,
            token: v.token,
        }
    }
}

#[derive(uniffi::Record, Debug, Clone, Copy, PartialEq, Eq)]
pub struct LnLspLimits {
    pub min_payment_msat: u64,
    pub max_payment_msat: u64,
    pub channel_min_sat: u64,
    pub channel_max_sat: u64,
}

impl From<LnLspLimits> for LspLimits {
    fn from(v: LnLspLimits) -> Self {
        LspLimits {
            min_payment_msat: v.min_payment_msat,
            max_payment_msat: v.max_payment_msat,
            channel_min_sat: v.channel_min_sat,
            channel_max_sat: v.channel_max_sat,
        }
    }
}

impl From<LspLimits> for LnLspLimits {
    fn from(v: LspLimits) -> Self {
        LnLspLimits {
            min_payment_msat: v.min_payment_msat,
            max_payment_msat: v.max_payment_msat,
            channel_min_sat: v.channel_min_sat,
            channel_max_sat: v.channel_max_sat,
        }
    }
}

#[derive(uniffi::Record, Clone, PartialEq, Eq)]
pub struct LnVssConfig {
    pub url: String,
    pub auth_header: String,
}

impl std::fmt::Debug for LnVssConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LnVssConfig")
            .field("url", &self.url)
            .field("auth_header", &"<redacted>")
            .finish()
    }
}

impl From<LnVssConfig> for VssConfig {
    fn from(v: LnVssConfig) -> Self {
        VssConfig {
            url: v.url,
            auth_header: v.auth_header,
        }
    }
}

#[derive(uniffi::Record)]
pub struct LnNodeConfig {
    pub bip39_seed: Vec<u8>,
    pub network: String,
    pub data_dir: String,
    pub esplora_url: String,
    /// `Authorization` value for `esplora_url`, for the hosted edge only (it
    /// requires a token); empty for a server of the user's own.
    pub esplora_auth_header: String,
    pub lsp: LnLspInfo,
    pub vss: LnVssConfig,
    pub limits: LnLspLimits,
}

#[derive(uniffi::Record, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LnBalances {
    pub lightning_sat: u64,
    pub onchain_transit_sat: u64,
    pub pending_close_sat: u64,
    pub reserved_sat: u64,
    pub pending_transit_sat: u64,
    pub inbound_sat: u64,
}

impl From<Balances> for LnBalances {
    fn from(b: Balances) -> Self {
        LnBalances {
            lightning_sat: b.lightning_sat,
            onchain_transit_sat: b.onchain_transit_sat,
            pending_close_sat: b.pending_close_sat,
            reserved_sat: b.reserved_sat,
            pending_transit_sat: b.pending_transit_sat,
            inbound_sat: b.inbound_sat,
        }
    }
}

#[derive(uniffi::Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LnChannelState {
    None,
    Opening,
    Ready,
    Closing,
}

impl From<ChannelState> for LnChannelState {
    fn from(s: ChannelState) -> Self {
        match s {
            ChannelState::None => LnChannelState::None,
            ChannelState::Opening => LnChannelState::Opening,
            ChannelState::Ready => LnChannelState::Ready,
            ChannelState::Closing => LnChannelState::Closing,
        }
    }
}

#[derive(uniffi::Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LnStartMode {
    Normal,
    Exit,
}

impl From<LnStartMode> for StartMode {
    fn from(m: LnStartMode) -> Self {
        match m {
            LnStartMode::Normal => StartMode::Normal,
            LnStartMode::Exit => StartMode::Exit,
        }
    }
}

impl From<StartMode> for LnStartMode {
    fn from(m: StartMode) -> Self {
        match m {
            StartMode::Normal => LnStartMode::Normal,
            StartMode::Exit => LnStartMode::Exit,
        }
    }
}

#[derive(uniffi::Record, Debug, Clone, PartialEq, Eq)]
pub struct LnReceiveQuote {
    pub invoice: String,
    pub amount_msat: u64,
    pub fee_msat: u64,
    pub needs_channel: bool,
}

impl From<ReceiveQuote> for LnReceiveQuote {
    fn from(q: ReceiveQuote) -> Self {
        LnReceiveQuote {
            invoice: q.invoice,
            amount_msat: q.amount_msat,
            fee_msat: q.fee_msat,
            needs_channel: q.needs_channel,
        }
    }
}

#[derive(uniffi::Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LnDirection {
    In,
    Out,
}

impl From<Direction> for LnDirection {
    fn from(d: Direction) -> Self {
        match d {
            Direction::In => LnDirection::In,
            Direction::Out => LnDirection::Out,
        }
    }
}

#[derive(uniffi::Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LnPaymentStatus {
    Pending,
    Succeeded,
    Failed,
}

impl From<Status> for LnPaymentStatus {
    fn from(s: Status) -> Self {
        match s {
            Status::Pending => LnPaymentStatus::Pending,
            Status::Succeeded => LnPaymentStatus::Succeeded,
            Status::Failed => LnPaymentStatus::Failed,
        }
    }
}

#[derive(uniffi::Record, Debug, Clone, PartialEq, Eq)]
pub struct LnPayment {
    pub id: String,
    pub direction: LnDirection,
    pub amount_msat: u64,
    pub fee_msat: u64,
    pub status: LnPaymentStatus,
    pub description: String,
    pub timestamp: u64,
}

impl From<Payment> for LnPayment {
    fn from(p: Payment) -> Self {
        LnPayment {
            id: p.id,
            direction: p.direction.into(),
            amount_msat: p.amount_msat,
            fee_msat: p.fee_msat,
            status: p.status.into(),
            description: p.description,
            timestamp: p.timestamp,
        }
    }
}

#[derive(uniffi::Enum, Debug, Clone, PartialEq, Eq)]
pub enum LnEventRecord {
    PaymentReceived { id: String, amount_msat: u64 },
    PaymentSent { id: String, fee_msat: u64 },
    PaymentFailed { id: String, reason: String },
    ChannelReady,
    ChannelClosed { reason: String },
    DeviceLockLost,
}

impl From<LnEvent> for LnEventRecord {
    fn from(e: LnEvent) -> Self {
        match e {
            LnEvent::PaymentReceived { id, amount_msat } => {
                LnEventRecord::PaymentReceived { id, amount_msat }
            }
            LnEvent::PaymentSent { id, fee_msat } => LnEventRecord::PaymentSent { id, fee_msat },
            LnEvent::PaymentFailed { id, reason } => LnEventRecord::PaymentFailed { id, reason },
            LnEvent::ChannelReady => LnEventRecord::ChannelReady,
            LnEvent::ChannelClosed { reason } => LnEventRecord::ChannelClosed { reason },
            LnEvent::DeviceLockLost => LnEventRecord::DeviceLockLost,
        }
    }
}

/// The Lightning part of a [`LnTarget::Unified`] BIP21 URI (always a BOLT11
/// invoice or a BOLT12 offer).
#[derive(uniffi::Enum, Debug, Clone, PartialEq, Eq)]
pub enum LnUnifiedLightning {
    Bolt11 {
        invoice: String,
        amount_msat: Option<u64>,
        description: String,
        expiry_secs: u64,
    },
    Bolt12Offer {
        offer: String,
        amount_msat: Option<u64>,
        description: String,
    },
}

impl LnUnifiedLightning {
    pub fn into_payment_target(self) -> PaymentTarget {
        match self {
            LnUnifiedLightning::Bolt11 {
                invoice,
                amount_msat,
                description,
                expiry_secs,
            } => PaymentTarget::Bolt11 {
                invoice,
                amount_msat,
                description,
                expiry_secs,
            },
            LnUnifiedLightning::Bolt12Offer {
                offer,
                amount_msat,
                description,
            } => PaymentTarget::Bolt12Offer {
                offer,
                amount_msat,
                description,
            },
        }
    }
}

#[derive(uniffi::Enum, Debug, Clone, PartialEq, Eq)]
pub enum LnTarget {
    Bolt11 {
        invoice: String,
        amount_msat: Option<u64>,
        description: String,
        expiry_secs: u64,
    },
    Bolt12Offer {
        offer: String,
        amount_msat: Option<u64>,
        description: String,
    },
    LnurlPay {
        url: String,
    },
    LightningAddress {
        user: String,
        domain: String,
    },
    OnChain {
        address: String,
        amount_sat: Option<u64>,
    },
    Unified {
        address: String,
        amount_sat: Option<u64>,
        lightning: LnUnifiedLightning,
    },
}

impl TryFrom<PaymentTarget> for LnTarget {
    type Error = LnFfiError;

    fn try_from(t: PaymentTarget) -> Result<Self, Self::Error> {
        Ok(match t {
            PaymentTarget::Bolt11 {
                invoice,
                amount_msat,
                description,
                expiry_secs,
            } => LnTarget::Bolt11 {
                invoice,
                amount_msat,
                description,
                expiry_secs,
            },
            PaymentTarget::Bolt12Offer {
                offer,
                amount_msat,
                description,
            } => LnTarget::Bolt12Offer {
                offer,
                amount_msat,
                description,
            },
            PaymentTarget::LnurlPay { url } => LnTarget::LnurlPay { url },
            PaymentTarget::LightningAddress { user, domain } => {
                LnTarget::LightningAddress { user, domain }
            }
            PaymentTarget::OnChain {
                address,
                amount_sat,
            } => LnTarget::OnChain {
                address,
                amount_sat,
            },
            PaymentTarget::Unified {
                address,
                amount_sat,
                lightning,
            } => {
                let ln = match *lightning {
                    PaymentTarget::Bolt11 {
                        invoice,
                        amount_msat,
                        description,
                        expiry_secs,
                    } => LnUnifiedLightning::Bolt11 {
                        invoice,
                        amount_msat,
                        description,
                        expiry_secs,
                    },
                    PaymentTarget::Bolt12Offer {
                        offer,
                        amount_msat,
                        description,
                    } => LnUnifiedLightning::Bolt12Offer {
                        offer,
                        amount_msat,
                        description,
                    },
                    _ => {
                        return Err(LnFfiError::Other {
                            reason: "invalid: unified lightning target is not bolt11 or bolt12"
                                .into(),
                        })
                    }
                };
                LnTarget::Unified {
                    address,
                    amount_sat,
                    lightning: ln,
                }
            }
        })
    }
}

impl From<LnTarget> for PaymentTarget {
    fn from(t: LnTarget) -> Self {
        match t {
            LnTarget::Bolt11 {
                invoice,
                amount_msat,
                description,
                expiry_secs,
            } => PaymentTarget::Bolt11 {
                invoice,
                amount_msat,
                description,
                expiry_secs,
            },
            LnTarget::Bolt12Offer {
                offer,
                amount_msat,
                description,
            } => PaymentTarget::Bolt12Offer {
                offer,
                amount_msat,
                description,
            },
            LnTarget::LnurlPay { url } => PaymentTarget::LnurlPay { url },
            LnTarget::LightningAddress { user, domain } => {
                PaymentTarget::LightningAddress { user, domain }
            }
            LnTarget::OnChain {
                address,
                amount_sat,
            } => PaymentTarget::OnChain {
                address,
                amount_sat,
            },
            LnTarget::Unified {
                address,
                amount_sat,
                lightning,
            } => PaymentTarget::Unified {
                address,
                amount_sat,
                lightning: Box::new(lightning.into_payment_target()),
            },
        }
    }
}

#[derive(uniffi::Record, Debug, Clone, PartialEq, Eq)]
pub struct LnurlParams {
    pub callback: String,
    pub min_msat: u64,
    pub max_msat: u64,
    pub description: String,
    pub metadata: String,
}

impl From<LnurlPayParams> for LnurlParams {
    fn from(p: LnurlPayParams) -> Self {
        LnurlParams {
            callback: p.callback,
            min_msat: p.min_msat,
            max_msat: p.max_msat,
            description: p.description,
            metadata: p.metadata,
        }
    }
}

#[derive(uniffi::Record, Debug, Clone, PartialEq, Eq)]
pub struct LnStaticUtxo {
    pub txid: String,
    pub vout: u32,
    pub value_sat: u64,
    pub script_index: u32,
    pub confirmations: u32,
}

impl From<StaticUtxo> for LnStaticUtxo {
    fn from(u: StaticUtxo) -> Self {
        LnStaticUtxo {
            txid: u.outpoint.txid.to_string(),
            vout: u.outpoint.vout,
            value_sat: u.value_sat,
            script_index: u.script_index,
            confirmations: u.confirmations,
        }
    }
}

impl TryFrom<LnStaticUtxo> for StaticUtxo {
    type Error = LnFfiError;

    fn try_from(u: LnStaticUtxo) -> Result<Self, Self::Error> {
        let txid = Txid::from_str(&u.txid).map_err(|_| LnFfiError::Other {
            reason: format!("bad txid {}", u.txid),
        })?;
        Ok(StaticUtxo {
            outpoint: OutPoint::new(txid, u.vout),
            value_sat: u.value_sat,
            script_index: u.script_index,
            confirmations: u.confirmations,
        })
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

#[uniffi::export]
pub fn ln_parse(text: String, network: String) -> Result<LnTarget, LnFfiError> {
    let net = parse_network(&network)?;
    let target = fortis_lightning::parse(&text, net)?;
    LnTarget::try_from(target)
}

#[uniffi::export]
pub fn ln_parse_at(text: String, network: String, now_secs: u64) -> Result<LnTarget, LnFfiError> {
    let net = parse_network(&network)?;
    let target = fortis_lightning::parse_at(&text, net, now_secs)?;
    LnTarget::try_from(target)
}

#[uniffi::export]
pub fn ln_resolve_lnurl(target: LnTarget) -> Result<LnurlParams, LnFfiError> {
    let core_target = PaymentTarget::from(target);
    let params = lnurl::resolve(&core_target, &UreqGet::new())?;
    Ok(params.into())
}

/// Derives the 64-byte BIP-39 seed from `mnemonic` and `passphrase`, so the
/// Android shell can pass it to `LightningSession.onUnlocked` / `LnNodeConfig`.
#[uniffi::export]
pub fn ln_bip39_seed(mnemonic: String, passphrase: String) -> Result<Vec<u8>, LnFfiError> {
    let m = Mnemonic::parse(mnemonic.trim()).map_err(|_| LnFfiError::Other {
        reason: "invalid_mnemonic".into(),
    })?;
    let seed = Zeroizing::new(m.to_seed(&passphrase));
    Ok(seed.to_vec())
}

/// Upper-bound routing fee estimate (`1 % + 50 sat`) for `target` / `amount_msat`:
/// the rule `LnNode::pay` enforces, from `fortis_lightning` (not a copy of it).
#[uniffi::export]
pub fn ln_estimate_fee_msat(target: LnTarget, amount_msat: u64) -> u64 {
    fortis_lightning::estimate_routing_fee_msat(&PaymentTarget::from(target), amount_msat)
}

#[uniffi::export]
pub fn ln_vss_state_exists(
    vss: LnVssConfig,
    bip39_seed: Vec<u8>,
    network: String,
) -> Result<bool, LnFfiError> {
    let net = parse_network(&network)?;
    let seed = Zeroizing::new(bip39_seed);
    Ok(vss_state_exists(&vss.into(), &seed, net)?)
}

fn derive_node_seed(bip39_seed: Vec<u8>, network: Network) -> Result<Zeroizing<[u8; 64]>, LnFfiError> {
    let seed = Zeroizing::new(bip39_seed);
    let arr: &[u8; 64] = seed.as_slice().try_into().map_err(|_| LnFfiError::Other {
        reason: "bip39_seed must be 64 bytes".into(),
    })?;
    let raw = wallet_core::bip85_hex(arr, network, 64, 0);
    let mut out = Zeroizing::new([0u8; 64]);
    out.copy_from_slice(&raw);
    Ok(out)
}

#[uniffi::export]
pub fn ln_recover_scan(
    bip39_seed: Vec<u8>,
    network: String,
    esplora_url: String,
    esplora_auth_header: String,
) -> Result<Vec<LnStaticUtxo>, LnFfiError> {
    let net = parse_network(&network)?;
    let node_seed = derive_node_seed(bip39_seed, net)?;
    let source = EsploraScan::with_auth(&esplora_url, Some(esplora_auth_header));
    let utxos = recover::scan_static_outputs(&node_seed, net, &source, &mut |_, _| {})?;
    Ok(utxos.into_iter().map(Into::into).collect())
}

#[uniffi::export]
pub fn ln_recover_sweep(
    bip39_seed: Vec<u8>,
    network: String,
    utxos: Vec<LnStaticUtxo>,
    to_address: String,
    feerate_sat_per_vb: u64,
    esplora_url: String,
    esplora_auth_header: String,
) -> Result<Option<String>, LnFfiError> {
    let net = parse_network(&network)?;
    let node_seed = derive_node_seed(bip39_seed, net)?;
    let core_utxos: Vec<StaticUtxo> = utxos
        .into_iter()
        .map(StaticUtxo::try_from)
        .collect::<Result<_, _>>()?;
    let addr = Address::from_str(to_address.trim())
        .map_err(|_| LnFfiError::Other {
            reason: "bad_address".into(),
        })?
        .require_network(net)
        .map_err(|_| LnFfiError::WrongNetwork)?;
    let broadcaster = EsploraBroadcast::with_auth(&esplora_url, Some(esplora_auth_header));
    let txid = recover::sweep_static_outputs(
        &node_seed,
        net,
        &core_utxos,
        &addr,
        feerate_sat_per_vb,
        &broadcaster,
    )?;
    Ok(txid.map(|t| t.to_string()))
}

// ---------------------------------------------------------------------------
// LnNode object
// ---------------------------------------------------------------------------

#[derive(uniffi::Object)]
pub struct LnNode {
    inner: Node,
}

#[uniffi::export]
impl LnNode {
    #[uniffi::constructor]
    pub fn new(cfg: LnNodeConfig) -> Result<Arc<Self>, LnFfiError> {
        let network = parse_network(&cfg.network)?;
        let node = Node::new(NodeConfig {
            bip39_seed: cfg.bip39_seed,
            network,
            data_dir: PathBuf::from(cfg.data_dir),
            chain_source: ChainSource::Esplora {
                url: cfg.esplora_url,
                auth_header: Some(cfg.esplora_auth_header).filter(|h| !h.is_empty()),
            },
            lsp: cfg.lsp.into(),
            vss: cfg.vss.into(),
            limits: cfg.limits.into(),
        })?;
        Ok(Arc::new(LnNode { inner: node }))
    }

    pub fn node_id(&self) -> String {
        self.inner.node_id()
    }

    pub fn limits(&self) -> LnLspLimits {
        self.inner.limits().into()
    }

    /// Sats a move to Lightning must deposit on top of the channel size: the
    /// anchor reserve `open_channel` needs left in the transit wallet.
    pub fn anchor_reserve_sat(&self) -> u64 {
        Node::anchor_reserve_sat()
    }

    pub fn start(&self) -> Result<(), LnFfiError> {
        Ok(self.inner.start()?)
    }

    pub fn start_with(&self, mode: LnStartMode) -> Result<(), LnFfiError> {
        Ok(self.inner.start_with(mode.into())?)
    }

    pub fn start_mode(&self) -> Option<LnStartMode> {
        self.inner.start_mode().map(Into::into)
    }

    pub fn takeover(&self) -> Result<(), LnFfiError> {
        Ok(self.inner.takeover(StartMode::Normal)?)
    }

    pub fn takeover_with(&self, mode: LnStartMode) -> Result<(), LnFfiError> {
        Ok(self.inner.takeover(mode.into())?)
    }

    pub fn stop(&self) {
        self.inner.stop();
    }

    pub fn is_quiescent(&self) -> bool {
        self.inner.is_quiescent()
    }

    pub fn balances(&self) -> LnBalances {
        self.inner.balances().into()
    }

    pub fn receive(
        &self,
        amount_msat: u64,
        description: String,
    ) -> Result<LnReceiveQuote, LnFfiError> {
        Ok(self.inner.receive(amount_msat, &description)?.into())
    }

    pub fn pay(&self, target: LnTarget, amount_msat: Option<u64>) -> Result<String, LnFfiError> {
        let core_target = PaymentTarget::from(target);
        Ok(self.inner.pay(&core_target, amount_msat)?)
    }

    pub fn estimate_fee_msat(&self, target: LnTarget, amount_msat: u64) -> u64 {
        let core_target = PaymentTarget::from(target);
        self.inner.estimate_fee_msat(&core_target, amount_msat)
    }

    /// Msat `pay` can send now (LDK's outbound HTLC limit over usable channels).
    pub fn sendable_msat(&self) -> u64 {
        self.inner.sendable_msat()
    }

    pub fn payments(&self) -> Vec<LnPayment> {
        self.inner.payments().into_iter().map(Into::into).collect()
    }

    pub fn next_event(&self, timeout_ms: u64) -> Option<LnEventRecord> {
        self.inner
            .next_event(Duration::from_millis(timeout_ms))
            .map(Into::into)
    }

    pub fn transit_address(&self) -> Result<String, LnFfiError> {
        Ok(self.inner.transit_address()?)
    }

    pub fn open_channel(&self, amount_sat: u64) -> Result<(), LnFfiError> {
        Ok(self.inner.open_channel(amount_sat)?)
    }

    pub fn close_channel(&self) -> Result<(), LnFfiError> {
        Ok(self.inner.close_channel()?)
    }

    pub fn force_close_channel(&self) -> Result<(), LnFfiError> {
        Ok(self.inner.force_close_channel()?)
    }

    pub fn sweep_transit(&self, to_address: String) -> Result<Option<String>, LnFfiError> {
        Ok(self.inner.sweep_transit(&to_address)?)
    }

    pub fn channel_state(&self) -> LnChannelState {
        self.inner.channel_state().into()
    }
}
