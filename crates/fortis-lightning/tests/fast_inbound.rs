//! A small inbound channel is accepted after fewer confirmations than LDK's 6 (`fast_inbound_max_sat`
//! / `fast_inbound_depth` in the LSP's config); a larger one still waits for 6. The depth the LSP
//! asks for is in its `accept_channel`, so the opener sees it as `confirmations_required` the moment
//! the channel is negotiated: no blocks needed to check it.
//!
//! ```sh
//! cargo build -p fortis-lsp
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//!   cargo test -p fortis-lightning --test fast_inbound -- --ignored --nocapture --test-threads=1
//! ```

mod common;

use std::str::FromStr;

use common::{builder, poll, test_seed, Lsp, Regtest, TempDir};
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;

#[test]
#[ignore = "needs fortis-lsp built and FORTIS_BTC_BITCOIND"]
fn small_inbound_channels_need_fewer_confirmations_and_large_ones_do_not() {
    let rt = Regtest::start();
    let lsp = Lsp::start(&rt, &[("fast_inbound_max_sat", "100000"), ("fast_inbound_depth", "1")]);
    let lsp_id = PublicKey::from_str(&lsp.node_id).unwrap();
    let lsp_addr = SocketAddress::from_str(&format!("{}:{}", lsp.host, lsp.port)).unwrap();

    let dir = TempDir::new("fast-inbound");
    let (b, _addr) = builder(&rt, test_seed(0x91), &dir.str("opener"));
    let opener = b.build_with_fs_store().unwrap();
    opener.start().unwrap();
    let funding = opener.onchain_payment().new_address().unwrap();
    rt.fund(&funding.to_string(), 500_000);
    rt.mine(6);
    poll("the opener sees its funds", 90, || {
        opener.sync_wallets().ok();
        (opener.list_balances().spendable_onchain_balance_sats > 0).then_some(())
    });

    let required = |sat: u64| {
        opener.connect(lsp_id, lsp_addr.clone(), true).unwrap();
        opener.open_channel(lsp_id, lsp_addr.clone(), sat, None, None).unwrap();
        poll("the channel is negotiated", 60, || {
            opener
                .list_channels()
                .iter()
                .find(|c| c.channel_value_sats == sat)
                .and_then(|c| c.confirmations_required)
        })
    };
    assert_eq!(required(50_000), 1, "a small channel waits for 1 confirmation");
    assert_eq!(required(150_000), 6, "a channel above the limit keeps LDK's 6");
    opener.stop().unwrap();
}
