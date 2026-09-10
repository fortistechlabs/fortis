//! One-off recovery helper: decrypt a wallet's sealed seed blob given the
//! password, printing the mnemonic. Opt-in — needs the three env vars set.
//!
//! ```sh
//! FORTIS_BLOB=<hex> FORTIS_SALT=<hex> FORTIS_PW=<password> \
//!   cargo test -p wallet-ffi --test unseal_recover -- --ignored --nocapture
//! ```

#[test]
#[ignore = "manual recovery tool"]
fn unseal() {
    let blob = std::env::var("FORTIS_BLOB").expect("FORTIS_BLOB");
    let salt = hex::decode(std::env::var("FORTIS_SALT").expect("FORTIS_SALT")).expect("salt hex");
    let pw = std::env::var("FORTIS_PW").expect("FORTIS_PW");

    let payload = wallet_ffi::unseal_mnemonic_with_password(blob, pw, salt).expect("unseal");
    // payload is "<mnemonic>\n<passphrase>"
    let (mnemonic, passphrase) = payload.split_once('\n').unwrap_or((&payload, ""));
    println!("MNEMONIC: {mnemonic}");
    println!("PASSPHRASE: {:?}", passphrase);
}
