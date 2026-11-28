//! The web wallet derives its Lightning node identity and signs Fortis account
//! requests in wallet-core (no ldk-node). It must produce exactly the node id
//! `ldk-node` has for the same seed and exactly the signature `fortis-lsp`
//! verifies (`message_signing::verify` over `fortis-account-v1:` + body).

use fortis_lightning::{derive_node_seed, LnChain};
use ldk_node::bip39::Mnemonic;
use ldk_node::bitcoin::bip32::Xpriv;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Network;
use ldk_node::lightning::sign::{KeysManager, NodeSigner, Recipient};
use ldk_node::lightning::util::message_signing;
use wallet_core::keys::MasterKey;
use wallet_core::ln_account::LnChainId;

const PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const BODY: &str = r#"{"node_id":"x","op":"balance","params":{},"nonce":"0123456789abcdef0123456789abcdef","expires_at":1800000120}"#;

/// ldk-node's node id for this phrase and chain (the `ldk_node_id` recipe in `node.rs`).
fn ldk_id(chain: LnChain) -> (PublicKey, ldk_node::bitcoin::secp256k1::SecretKey) {
    let seed = Mnemonic::parse(PHRASE).unwrap().to_seed("");
    let ldk_seed = derive_node_seed(&seed, Network::Bitcoin, chain);
    let master = Xpriv::new_master(Network::Bitcoin, &ldk_seed[..]).unwrap();
    let km = KeysManager::new(&master.private_key.secret_bytes(), 0, 0, true);
    (km.get_node_id(Recipient::Node).unwrap(), km.get_node_secret_key())
}

fn chains() -> [(LnChain, LnChainId); 2] {
    [(LnChain::Btc, LnChainId::Btc), (LnChain::Xbt, LnChainId::Xbt)]
}

#[test]
fn web_node_id_is_the_ldk_node_id() {
    let key = MasterKey::from_phrase(PHRASE, "").unwrap();
    for (ldk, web) in chains() {
        assert_eq!(key.ln_node_id(web), ldk_id(ldk).0.to_string());
    }
    assert_ne!(key.ln_node_id(LnChainId::Btc), key.ln_node_id(LnChainId::Xbt));
}

#[test]
fn web_signature_is_what_the_lsp_verifies_and_what_ldk_signs() {
    let key = MasterKey::from_phrase(PHRASE, "").unwrap();
    for (ldk, web) in chains() {
        let (pk, sk) = ldk_id(ldk);
        let sig = key.ln_sign_account(web, BODY);
        let msg = format!("fortis-account-v1:{BODY}");
        assert!(message_signing::verify(msg.as_bytes(), &sig, &pk), "LSP check");
        assert_eq!(sig, message_signing::sign(msg.as_bytes(), &sk), "same bytes as LDK");
    }
}
