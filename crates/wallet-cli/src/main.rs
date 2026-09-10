//! fortis CLI wallet.
//!
//! Wraps `wallet-core` and a Bitcoin Knots / BLAKE2b full node: the node is the
//! chain backend (UTXOs, confirmations, fee estimation, broadcast); `wallet-core`
//! owns keys, derivation, coin selection and signing.
//!
//! Watch-only reporting (`address` / `balance` / `utxos`) needs only the account
//! xpub. `send` needs the seed — either sealed at `<home>/seed.enc` (`import-seed`)
//! or piped in with `--phrase-stdin`.

mod config;
mod seed;

use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use fortis_node::{self as node, Rpc};
use serde_json::{json, Value};
use zeroize::Zeroizing;

use config::{default_bitcoin_datadir, default_home, NodeConfig, WalletConfig, CONFIG_VERSION};
use wallet_core::bitcoin::address::NetworkUnchecked;
use wallet_core::bitcoin::bip32::Xpub;
use wallet_core::bitcoin::{consensus, Address, Amount, Denomination, TxOut};
use wallet_core::{Chain, ChainParams, MasterKey, WalletView};

#[derive(Parser)]
#[command(
    name = "fortis",
    version,
    about = "fortis wallet — wallet-core driven against a Bitcoin Knots / BLAKE2b node"
)]
struct Cli {
    /// Wallet home dir (default: $FORTIS_HOME, else %APPDATA%\fortis or ~/.fortis).
    #[arg(long, global = true)]
    home: Option<std::path::PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new wallet, or restore one from a recovery phrase.
    Init(InitArgs),
    /// Seal the seed at <home>/seed.enc so `send` can sign without --phrase-stdin.
    ImportSeed(ImportSeedArgs),
    /// Show node status: chain, sync, BLAKE2b activation.
    Node,
    /// Create/refresh the node-side watch-only wallet and import fortis descriptors.
    Connect(ConnectArgs),
    /// Print the next unused address (or peek at a specific index).
    Address(AddressArgs),
    /// Show wallet balance as seen by the node.
    Balance,
    /// List spendable UTXOs the node sees for this wallet.
    Utxos,
    /// Build, sign and broadcast a payment.
    Send(SendArgs),
    /// Broadcast a raw transaction hex (e.g. from `send --dry-run`).
    Broadcast {
        /// Raw transaction, hex-encoded.
        hex: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ChainArg {
    /// The Bitcoin Knots BLAKE2b hard fork.
    Xbt,
    /// Canonical Bitcoin.
    Btc,
}

impl ChainArg {
    fn as_str(self) -> &'static str {
        match self {
            ChainArg::Xbt => "xbt",
            ChainArg::Btc => "btc",
        }
    }
    fn to_chain(self) -> Chain {
        match self {
            ChainArg::Xbt => Chain::Xbt,
            ChainArg::Btc => Chain::Btc,
        }
    }
}

#[derive(Args)]
struct InitArgs {
    /// Restore from an existing BIP-39 phrase (read from stdin) instead of generating one.
    #[arg(long)]
    restore: bool,
    /// Also apply a BIP-39 passphrase (read from stdin after the phrase).
    #[arg(long)]
    passphrase: bool,
    /// Word count for a generated phrase: 12 or 24.
    #[arg(long, default_value_t = 24)]
    words: u8,
    /// Chain this wallet tracks.
    #[arg(long, value_enum, default_value_t = ChainArg::Xbt)]
    chain: ChainArg,
    /// Network: mainnet, regtest, or regtest-legacy.
    #[arg(long, default_value = "mainnet")]
    network: String,
    /// Node data directory (holds `.cookie`). Default: the platform Bitcoin dir.
    #[arg(long)]
    datadir: Option<std::path::PathBuf>,
    /// Node RPC URL.
    #[arg(long, default_value = "http://127.0.0.1:8332")]
    rpc_url: String,
    /// Also seal the seed to <home>/seed.enc (prompts for a password).
    #[arg(long)]
    seal: bool,
    /// Fold extra entropy into the new seed: a file of bytes, or `-` for stdin
    /// (e.g. a page of dice rolls or keyboard mashing). Supplements the OS
    /// CSPRNG, never replaces it.
    #[arg(long, value_name = "FILE")]
    extra_entropy: Option<String>,
    /// Overwrite an existing wallet.json.
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct ImportSeedArgs {
    /// Read mnemonic (line 1) + optional passphrase (line 2) from stdin.
    #[arg(long)]
    phrase_stdin: bool,
}

#[derive(Args)]
struct SendArgs {
    /// Destination address.
    to: String,
    /// Amount, in whole coins (e.g. 0.01) unless --sats. Omit with --sweep.
    amount: Option<String>,
    /// Interpret AMOUNT as satoshis.
    #[arg(long)]
    sats: bool,
    /// Send the entire confirmed balance (fee deducted), with no change output.
    #[arg(long)]
    sweep: bool,
    /// Fee rate, sat/vB. Default: estimatesmartfee, floored at the node's minrelay.
    #[arg(long)]
    feerate: Option<u64>,
    /// Confirmation target for fee estimation.
    #[arg(long, default_value_t = 6)]
    conf_target: u16,
    /// Only spend UTXOs with at least this many confirmations.
    #[arg(long, default_value_t = 1)]
    min_conf: u32,
    /// Read the seed from stdin instead of the sealed seed.enc.
    #[arg(long)]
    phrase_stdin: bool,
    /// BTC only: append a 100-byte random OP_RETURN so the tx is consensus-invalid
    /// on the BLAKE2b fork and cannot be replayed there. Non-standard on default
    /// Bitcoin relay — needs a tolerant node/service to broadcast.
    #[arg(long)]
    replay_protect: bool,
    /// Build and check the transaction but do not broadcast; print the signed hex.
    #[arg(long)]
    dry_run: bool,
    /// Skip the interactive "type yes" confirmation.
    #[arg(long)]
    yes: bool,
}

#[derive(Args)]
struct ConnectArgs {
    /// Import from genesis to discover pre-existing funds (triggers a full rescan).
    #[arg(long)]
    rescan: bool,
    /// Address range (gap limit) to import per branch.
    #[arg(long, default_value_t = 1000)]
    range: u32,
}

#[derive(Args)]
struct AddressArgs {
    /// Show a change address instead of a receive address.
    #[arg(long)]
    change: bool,
    /// Show the address at this exact index without advancing the counter.
    #[arg(long)]
    peek: Option<u32>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let home = cli.home.clone().unwrap_or_else(default_home);

    let result = match &cli.cmd {
        Cmd::Init(a) => cmd_init(&home, a),
        Cmd::ImportSeed(a) => cmd_import_seed(&home, a),
        Cmd::Node => with_ctx(&home, cmd_node),
        Cmd::Connect(a) => with_ctx(&home, |cfg, rpc| cmd_connect(cfg, rpc, a)),
        Cmd::Address(a) => match load_ctx(&home) {
            Ok((cfg, rpc)) => cmd_address(&home, cfg, &rpc, a),
            Err(e) => Err(e),
        },
        Cmd::Balance => with_ctx(&home, cmd_balance),
        Cmd::Utxos => with_ctx(&home, cmd_utxos),
        Cmd::Send(a) => match load_ctx(&home) {
            Ok((cfg, rpc)) => cmd_send(&home, cfg, &rpc, a),
            Err(e) => Err(e),
        },
        Cmd::Broadcast { hex } => with_ctx(&home, |_cfg, rpc| cmd_broadcast(rpc, hex)),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn load_ctx(home: &Path) -> Result<(WalletConfig, Rpc)> {
    let cfg = WalletConfig::load(home)?;
    let auth = cfg.node.auth_userpass(&cfg.network)?;
    let rpc = Rpc::new(&cfg.node.rpc_url, &auth);
    Ok((cfg, rpc))
}

fn with_ctx(home: &Path, f: impl FnOnce(&WalletConfig, &Rpc) -> Result<()>) -> Result<()> {
    let (cfg, rpc) = load_ctx(home)?;
    f(&cfg, &rpc)
}

fn resolve_params(cfg: &WalletConfig) -> Result<ChainParams> {
    let chain = match cfg.chain.as_str() {
        "xbt" => Chain::Xbt,
        "btc" => Chain::Btc,
        other => bail!("wallet.json has unknown chain {other:?}"),
    };
    ChainParams::resolve(chain, &cfg.network)
        .ok_or_else(|| anyhow!("unknown chain/network: {}/{}", cfg.chain, cfg.network))
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

fn cmd_init(home: &Path, a: &InitArgs) -> Result<()> {
    if WalletConfig::exists(home) && !a.force {
        bail!(
            "a wallet already exists at {} (pass --force to overwrite)",
            WalletConfig::path(home).display()
        );
    }
    let params = ChainParams::resolve(a.chain.to_chain(), &a.network).ok_or_else(|| {
        anyhow!("unknown network {:?} for chain {}", a.network, a.chain.as_str())
    })?;

    let phrase: Zeroizing<String> = if a.restore {
        eprint!("Enter your recovery phrase, then press Enter:\n> ");
        io::stderr().flush().ok();
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() < 12 {
            bail!("expected at least 12 words, got {}", words.len());
        }
        Zeroizing::new(words.join(" "))
    } else {
        if !matches!(a.words, 12 | 24) {
            bail!("--words must be 12 or 24");
        }
        let mut csprng = Zeroizing::new([0u8; 32]);
        getrandom::getrandom(&mut csprng[..]).map_err(|e| anyhow!("CSPRNG failed: {e}"))?;
        let extra: Zeroizing<Vec<u8>> = match a.extra_entropy.as_deref() {
            None => Zeroizing::new(Vec::new()),
            Some("-") => {
                let mut buf = Vec::new();
                io::stdin().read_to_end(&mut buf)?;
                eprintln!("mixed in {} bytes of extra entropy from stdin", buf.len());
                Zeroizing::new(buf)
            }
            Some(path) => {
                let buf = std::fs::read(path).with_context(|| format!("reading {path}"))?;
                eprintln!("mixed in {} bytes of extra entropy from {path}", buf.len());
                Zeroizing::new(buf)
            }
        };
        let sources: &[&[u8]] = if extra.is_empty() { &[] } else { &[extra.as_slice()] };
        let (mnemonic, _key) = MasterKey::generate_mixed(csprng.as_slice(), sources, a.words)?;
        Zeroizing::new(mnemonic.to_string())
    };

    let passphrase: Zeroizing<String> = if a.passphrase {
        eprint!("BIP-39 passphrase (leave empty for none): ");
        io::stderr().flush().ok();
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        Zeroizing::new(line.trim_end_matches(['\r', '\n']).to_string())
    } else {
        Zeroizing::new(String::new())
    };

    let key = MasterKey::from_phrase(phrase.as_str(), passphrase.as_str())?;
    let xpub = key.account_xpub(&params, 0)?;
    let fingerprint = key.master_fingerprint().to_string();

    let datadir = a
        .datadir
        .clone()
        .unwrap_or_else(default_bitcoin_datadir)
        .to_string_lossy()
        .into_owned();

    // regtest's RPC is on a different default port; adjust unless overridden.
    let rpc_url = if a.network.starts_with("regtest") && a.rpc_url == "http://127.0.0.1:8332" {
        "http://127.0.0.1:18443".to_string()
    } else {
        a.rpc_url.clone()
    };

    let cfg = WalletConfig {
        version: CONFIG_VERSION,
        chain: a.chain.as_str().to_string(),
        network: a.network.clone(),
        account_xpub: xpub.to_string(),
        master_fingerprint: fingerprint,
        next_receive: 0,
        next_change: 0,
        node: NodeConfig {
            datadir,
            rpc_url,
            watch_wallet: format!("fortis-{}", a.chain.as_str()),
            cookie_file: None,
            rpc_user: None,
            rpc_password: None,
        },
    };
    cfg.save(home)?;
    // A sealed seed from a previous wallet at this home would now be stale.
    seed::delete(home)?;

    if !a.restore {
        println!("\n  ┌────────────────────────────────────────────────────────────┐");
        println!("  │  RECOVERY PHRASE — write this on paper, offline.            │");
        println!("  │  It is shown once and is NOT stored anywhere.              │");
        println!("  └────────────────────────────────────────────────────────────┘\n");
        for (i, w) in phrase.split_whitespace().enumerate() {
            print!("  {:>2}. {:<14}", i + 1, w);
            if (i + 1) % 3 == 0 {
                println!();
            }
        }
        println!();
    }
    println!();
    println!("wallet home      {}", home.display());
    println!("chain / network  {} / {}", cfg.chain, cfg.network);
    println!("account xpub     {}", cfg.account_xpub);
    println!("fingerprint      {}", cfg.master_fingerprint);
    println!("node datadir     {}", cfg.node.datadir);
    println!("node rpc         {}", cfg.node.rpc_url);
    println!("watch wallet     {}", cfg.node.watch_wallet);

    if a.seal {
        println!();
        let password = seed::prompt_new_password()?;
        seed::seal(home, phrase.as_str(), passphrase.as_str(), &password)?;
        println!("sealed seed      {}", seed::sealed_path(home).display());
    }

    println!();
    println!("Next:  fortis node       # check the node is reachable");
    println!("       fortis connect    # import the watch-only descriptors");
    if !a.seal {
        println!("       fortis import-seed  # (optional) seal the seed for `send`");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// import-seed
// ---------------------------------------------------------------------------

fn cmd_import_seed(home: &Path, a: &ImportSeedArgs) -> Result<()> {
    let cfg = WalletConfig::load(home)?;
    let params = resolve_params(&cfg)?;

    let (mnemonic, passphrase): (Zeroizing<String>, Zeroizing<String>) = if a.phrase_stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        let mut lines = input.lines();
        let m = lines
            .next()
            .ok_or_else(|| anyhow!("expected a recovery phrase on stdin"))?;
        (
            Zeroizing::new(m.split_whitespace().collect::<Vec<_>>().join(" ")),
            Zeroizing::new(lines.next().unwrap_or("").trim().to_string()),
        )
    } else {
        (seed::prompt_mnemonic()?, seed::prompt_passphrase()?)
    };

    let key = MasterKey::from_phrase(mnemonic.as_str(), passphrase.as_str())?;
    let derived = key.account_xpub(&params, 0)?.to_string();
    if derived != cfg.account_xpub {
        bail!(
            "that phrase derives\n  {derived}\nbut wallet.json expects\n  {}\n\
             (wrong seed, or a passphrase is needed / wrong)",
            cfg.account_xpub
        );
    }

    let password = seed::prompt_new_password()?;
    seed::seal(home, mnemonic.as_str(), passphrase.as_str(), &password)?;
    println!("sealed seed written to {}", seed::sealed_path(home).display());
    println!("`fortis send` will now prompt for this password.");
    Ok(())
}

// ---------------------------------------------------------------------------
// node
// ---------------------------------------------------------------------------

fn cmd_node(cfg: &WalletConfig, rpc: &Rpc) -> Result<()> {
    let s = node::chain_status(rpc)?;
    println!("node           {}", s.subversion);
    println!("chain          {}", s.chain);
    println!("blocks         {}  (headers {})", s.blocks, s.headers);
    println!(
        "sync           {:.4}%{}",
        s.progress * 100.0,
        if s.ibd { "   [initial block download]" } else { "" }
    );
    println!("pruned         {}", s.pruned);
    match s.blake2b_active {
        Some(true) => println!(
            "blake2b        ACTIVE{}",
            s.blake2b_height
                .map(|h| format!(" (since height {h})"))
                .unwrap_or_default()
        ),
        Some(false) => println!("blake2b        not yet active"),
        None => println!("blake2b        not reported by this node"),
    }

    let params = resolve_params(cfg)?;
    if params.require_unified_sighash && s.blake2b_active == Some(false) {
        println!(
            "\n⚠  this wallet requires SIGHASH_UNIFIED but the node has not activated \
             BLAKE2b — spends would be rejected."
        );
    }
    if cfg.network == "mainnet" && s.chain != "main" {
        println!(
            "\n⚠  wallet.json says mainnet but the node chain is {:?}.",
            s.chain
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// connect
// ---------------------------------------------------------------------------

fn cmd_connect(cfg: &WalletConfig, rpc: &Rpc, a: &ConnectArgs) -> Result<()> {
    let params = resolve_params(cfg)?;
    let s = node::chain_status(rpc)?;
    if params.require_unified_sighash && s.blake2b_active == Some(false) {
        println!("⚠  node has not activated BLAKE2b; funds will show but spends are unsupported.");
    }
    if a.rescan && s.pruned {
        bail!("--rescan needs an unpruned node, but this node is pruned");
    }

    let wallet = &cfg.node.watch_wallet;
    let how = node::ensure_watch_wallet(rpc, wallet)?;
    println!("watch wallet   {wallet}  ({how})");

    let (recv_ok, change_ok) = node::import_account(
        rpc,
        wallet,
        &cfg.master_fingerprint,
        params.bip44_coin_type,
        0,
        &cfg.account_xpub,
        a.range,
        a.rescan,
    )?;
    println!("import receive  {}", if recv_ok { "ok" } else { "FAILED" });
    println!("import change   {}", if change_ok { "ok" } else { "FAILED" });

    if recv_ok && change_ok {
        println!("\ndescriptors active, range 0..={}", a.range);
        if a.rescan {
            println!("a rescan from genesis is running — check `fortis balance` for progress");
        }
        println!("next:  fortis address");
    } else {
        bail!("descriptor import failed");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// address
// ---------------------------------------------------------------------------

fn cmd_address(home: &Path, mut cfg: WalletConfig, rpc: &Rpc, a: &AddressArgs) -> Result<()> {
    let params = resolve_params(&cfg)?;
    let xpub: Xpub = cfg
        .account_xpub
        .parse()
        .map_err(|e| anyhow!("wallet.json has an invalid account xpub: {e}"))?;
    let view = WalletView::new(params, xpub);
    let branch = u32::from(a.change);

    let (index, advancing) = match a.peek {
        Some(n) => (n, false),
        None => (
            if a.change { cfg.next_change } else { cfg.next_receive },
            true,
        ),
    };

    let addr = view.address_at(branch, index)?.to_string();

    let info = rpc.wallet_call(&cfg.node.watch_wallet, "getaddressinfo", json!([addr]));
    let (ismine, path) = match &info {
        Ok(v) => (
            v.get("ismine").and_then(Value::as_bool).unwrap_or(false),
            v.get("hdkeypath").and_then(Value::as_str).unwrap_or("").to_string(),
        ),
        Err(_) => (false, String::new()),
    };

    println!("{addr}");
    println!(
        "  branch/index   {branch}/{index}  ({})",
        if branch == 0 { "receive" } else { "change" }
    );
    if !path.is_empty() {
        println!("  node hdkeypath  {path}");
    }
    println!("  node ismine     {ismine}");
    if !ismine {
        println!(
            "  ⚠  the node does not recognise this address yet — run `fortis connect` \
             (raise --range if the index is high)."
        );
    }

    if advancing {
        if a.change {
            cfg.next_change += 1;
        } else {
            cfg.next_receive += 1;
        }
        cfg.save(home)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// balance / utxos
// ---------------------------------------------------------------------------

fn unit(cfg: &WalletConfig) -> &'static str {
    if cfg.chain == "xbt" {
        "XBT"
    } else {
        "BTC"
    }
}

fn cmd_balance(cfg: &WalletConfig, rpc: &Rpc) -> Result<()> {
    let wallet = &cfg.node.watch_wallet;
    node::ensure_watch_wallet(rpc, wallet)?;

    if let Some((progress, secs)) = node::scanning(rpc, wallet) {
        println!(
            "⚠  rescan in progress: {:.1}% ({secs}s) — figures below are incomplete\n",
            progress * 100.0
        );
    }

    let balances = rpc.wallet_call(wallet, "getbalances", json!([]))?;
    let mine = balances.get("mine").cloned().unwrap_or(Value::Null);
    let get = |k: &str| mine.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let u = unit(cfg);
    println!("confirmed   {:>18.8} {u}", get("trusted"));
    println!("pending     {:>18.8} {u}", get("untrusted_pending"));
    println!("immature    {:>18.8} {u}", get("immature"));
    Ok(())
}

fn cmd_utxos(cfg: &WalletConfig, rpc: &Rpc) -> Result<()> {
    let wallet = &cfg.node.watch_wallet;
    node::ensure_watch_wallet(rpc, wallet)?;

    let utxos = rpc.wallet_call(wallet, "listunspent", json!([0, 9_999_999]))?;
    let rows = utxos.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("no UTXOs");
        return Ok(());
    }

    let u = unit(cfg);
    println!("{:<70} {:>16} {:>8}  address", "outpoint", u, "conf");
    let mut total = 0.0;
    for r in &rows {
        let txid = r.get("txid").and_then(Value::as_str).unwrap_or("");
        let vout = r.get("vout").and_then(Value::as_u64).unwrap_or(0);
        let amount = r.get("amount").and_then(Value::as_f64).unwrap_or(0.0);
        let conf = r.get("confirmations").and_then(Value::as_u64).unwrap_or(0);
        let address = r.get("address").and_then(Value::as_str).unwrap_or("");
        total += amount;
        println!(
            "{:<70} {:>16.8} {:>8}  {address}",
            format!("{txid}:{vout}"),
            amount,
            conf
        );
    }
    println!(
        "{:<70} {:>16.8}",
        format!("{} output(s)", rows.len()),
        total
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// send / broadcast
// ---------------------------------------------------------------------------

fn parse_amount(s: &str, sats: bool) -> Result<Amount> {
    if sats {
        Ok(Amount::from_sat(s.trim().parse::<u64>().context("amount (sats)")?))
    } else {
        Amount::from_str_in(s.trim(), Denomination::Bitcoin).map_err(|e| anyhow!("amount: {e}"))
    }
}

fn coins(sat: u64) -> f64 {
    sat as f64 / 1e8
}

fn cmd_send(home: &Path, mut cfg: WalletConfig, rpc: &Rpc, a: &SendArgs) -> Result<()> {
    let params = resolve_params(&cfg)?;
    let net = params.network;
    let u = unit(&cfg);

    let dest = a
        .to
        .parse::<Address<NetworkUnchecked>>()
        .map_err(|e| anyhow!("bad destination address: {e}"))?
        .require_network(net)
        .map_err(|_| anyhow!("address {} is not valid on {net:?}", a.to))?;
    let dest_spk = dest.script_pubkey();

    let amount = match (a.sweep, a.amount.as_deref()) {
        (true, _) => None,
        (false, Some(s)) => Some(parse_amount(s, a.sats)?),
        (false, None) => bail!("give an AMOUNT, or pass --sweep"),
    };

    let feerate = match a.feerate {
        Some(f) => f.max(1),
        None => node::estimate_feerate(rpc, a.conf_target)?,
    };

    let wallet = &cfg.node.watch_wallet;
    node::ensure_watch_wallet(rpc, wallet)?;
    if let Some((p, _)) = node::scanning(rpc, wallet) {
        bail!(
            "a rescan is running ({:.0}%) — let it finish before sending",
            p * 100.0
        );
    }
    let utxos = node::collect_utxos(rpc, wallet, a.min_conf)?;
    if utxos.is_empty() {
        bail!("no spendable UTXOs at min-conf {}", a.min_conf);
    }

    let xpub: Xpub = cfg
        .account_xpub
        .parse()
        .map_err(|e| anyhow!("wallet.json has an invalid account xpub: {e}"))?;
    let mut view = WalletView::new(params.clone(), xpub);
    view.set_next_indices(cfg.next_receive, cfg.next_change);

    let mut extra_outs: Vec<TxOut> = Vec::new();
    if a.replay_protect {
        if cfg.chain != "btc" {
            bail!("--replay-protect only makes sense on the btc chain");
        }
        let mut data = [0u8; 100];
        getrandom::getrandom(&mut data).map_err(|e| anyhow!("CSPRNG: {e}"))?;
        extra_outs.push(wallet_core::op_return_output(&data)?);
        eprintln!("replay-protect: +100-byte OP_RETURN (non-standard on default Bitcoin relay)");
    }

    let plan = match amount {
        None if extra_outs.is_empty() => {
            view.plan_sweep(&utxos, dest_spk.clone(), feerate, a.min_conf, None)?
        }
        None => bail!("--sweep and --replay-protect can't be combined"),
        Some(v) => {
            let mut outs = vec![TxOut { value: v, script_pubkey: dest_spk.clone() }];
            outs.append(&mut extra_outs);
            view.plan_payment(&utxos, outs, feerate, a.min_conf, None, false)?
        }
    };
    let (_, next_change_after) = view.next_indices();

    // ---- sign ----
    let source = if a.phrase_stdin {
        seed::SeedSource::Stdin
    } else {
        seed::SeedSource::Auto
    };
    let key = seed::load_master_key(home, source)?;
    if key.account_xpub(&params, 0)?.to_string() != cfg.account_xpub {
        bail!("the supplied seed does not match this wallet (account xpub mismatch)");
    }

    let mut tx = plan.tx.clone();
    let prevouts: Vec<TxOut> = plan
        .selected
        .iter()
        .map(|c| TxOut { value: c.value, script_pubkey: c.script_pubkey.clone() })
        .collect();
    let paths: Vec<(bool, u32)> = plan
        .selected
        .iter()
        .map(|c| (c.is_change, c.derivation_index))
        .collect();
    key.sign_p2wpkh_tx(&params, 0, &mut tx, &prevouts, &paths)?;

    let raw = consensus::encode::serialize_hex(&tx);
    let txid = tx.compute_txid();
    let vsize = tx.vsize();

    // ---- consensus + policy pre-check (no broadcast, no fee spent) ----
    let check = rpc.call("testmempoolaccept", json!([[raw]]))?;
    let first = check.get(0).cloned().unwrap_or(Value::Null);
    if !first.get("allowed").and_then(Value::as_bool).unwrap_or(false) {
        let reason = first
            .get("reject-reason")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        bail!("the node would reject this transaction: {reason}");
    }

    // ---- summary ----
    let sent_sat = match amount {
        Some(v) => v.to_sat(),
        None => tx.output[0].value.to_sat(),
    };
    let in_sat: u64 = plan.selected.iter().map(|c| c.value.to_sat()).sum();
    println!(
        "from        {} input(s), {:.8} {u}",
        plan.selected.len(),
        coins(in_sat)
    );
    println!("to          {dest}");
    println!("            {:.8} {u}", coins(sent_sat));
    if let Some(chg) = plan.change {
        println!("change      {:.8} {u}", coins(chg.to_sat()));
    }
    println!(
        "fee         {} sat   ({:.2} sat/vB over {vsize} vB)",
        plan.fee.to_sat(),
        plan.fee.to_sat() as f64 / vsize as f64
    );
    println!("txid        {txid}");

    if a.dry_run {
        println!("\n--dry-run — not broadcast. Signed transaction:\n{raw}");
        return Ok(());
    }

    if !a.yes {
        eprint!("\nbroadcast? type 'yes' to confirm: ");
        io::stderr().flush().ok();
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        if line.trim() != "yes" {
            bail!("aborted — nothing was broadcast");
        }
    }

    let sent = rpc.call("sendrawtransaction", json!([raw]))?;
    let out = sent.as_str().unwrap_or("").to_string();
    println!(
        "\nbroadcast   {}",
        if out.is_empty() { txid.to_string() } else { out }
    );

    if plan.change.is_some() && next_change_after != cfg.next_change {
        cfg.next_change = next_change_after;
        cfg.save(home)?;
    }
    Ok(())
}

fn cmd_broadcast(rpc: &Rpc, hex: &str) -> Result<()> {
    let sent = rpc.call("sendrawtransaction", json!([hex.trim()]))?;
    println!("{}", sent.as_str().unwrap_or_default());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHRASE: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn sample_cfg() -> WalletConfig {
        let key = MasterKey::from_phrase(PHRASE, "").unwrap();
        let params = ChainParams::resolve(Chain::Xbt, "mainnet").unwrap();
        WalletConfig {
            version: CONFIG_VERSION,
            chain: "xbt".into(),
            network: "mainnet".into(),
            account_xpub: key.account_xpub(&params, 0).unwrap().to_string(),
            master_fingerprint: key.master_fingerprint().to_string(),
            next_receive: 0,
            next_change: 0,
            node: NodeConfig {
                datadir: "/x".into(),
                rpc_url: "http://127.0.0.1:8332".into(),
                watch_wallet: "fortis-xbt".into(),
                cookie_file: None,
                rpc_user: None,
                rpc_password: None,
            },
        }
    }

    #[test]
    fn descriptor_has_origin_and_wildcard() {
        let cfg = sample_cfg();
        let params = resolve_params(&cfg).unwrap();
        let d = |branch| {
            node::wpkh_branch_descriptor(
                &cfg.master_fingerprint,
                params.bip44_coin_type,
                0,
                &cfg.account_xpub,
                branch,
            )
        };
        assert_eq!(
            d(0),
            format!("wpkh([{}/84h/0h/0h]{}/0/*)", cfg.master_fingerprint, cfg.account_xpub)
        );
        assert!(d(0).starts_with("wpkh([73c5da0a/84h/0h/0h]xpub"));
        assert!(d(1).ends_with("/1/*)"));
    }

    #[test]
    fn xbt_wallet_requires_unified_sighash() {
        let params = resolve_params(&sample_cfg()).unwrap();
        assert!(params.require_unified_sighash);
    }

    #[test]
    fn import_request_shape() {
        let now = node::import_request("wpkh(x)#c", false, 500, false);
        assert_eq!(now["timestamp"], serde_json::json!("now"));
        assert_eq!(now["range"], serde_json::json!([0, 500]));
        assert_eq!(now["active"], serde_json::json!(true));
        assert!(now.get("label").is_none());

        let rescan = node::import_request("wpkh(x)#c", true, 10, true);
        assert_eq!(rescan["timestamp"], serde_json::json!(0));
        assert_eq!(rescan["internal"], serde_json::json!(true));
    }
}
