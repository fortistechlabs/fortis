//! The hardware-wallet import bindings: text and animated-QR paths both end in
//! a canonical `xpub` plus the device fingerprint, checked as a BIP-84 account.

use wallet_ffi::{import_account, UrAccountScanner};

// BCR-2020-015 test vector (see wallet-core's account_import tests).
const ACCOUNT_UR: &str = "ur:crypto-account/oeadcyemrewytyaolttaadeetaadmutaaddloxaxhdclaxwmfmdeiamecsdsemgtvsjzcncygrkowtrontzschgezokstswkkscfmklrtauteyaahdcxiehfonurdppfyntapejpproypegrdawkgmaewejlsfdtsrfybdehcaflmtrlbdhpamtaaddyoeadlncsdwykaeykaeykaocyemrewytyaycynlytsnyltaadeetaadmhtaadmwtaaddloxaxhdclaostvelfemdyynwydwyaievosrgmambklovabdgypdglldvespsthysadamhpmjeinaahdcxntdllnaaeykoytdacygegwhgjsiyonpywmcmrpwphsvodsrerozsbyaxluzcoxdpamtaaddyoeadlncsehykaeykaeykaocyemrewytyaycypdbskeuytaadeetaadmwtaaddloxaxhdclaxzcfxeegdrpmogrgwkbzctlttweadkiengrwlhtprremouoluutqdpfbncedkynfhaahdcxjpwevdeogthttkmeswzcolcpsaahcfnshkhtehytclmnteatmoteadtlwynnftloamtaaddyoeadlncsghykaeykaeykaocyemrewytyaycybthlvytstaadeetaadmhtaadnytaaddloxaxhdclaxhhsnhdrpftdwuocntilydibehnecmovdfekpjkclcslasbhkpawsaddmcmmnahnyaahdcxlotedtndfymyltclhlmtpfsadscnhtztaolbnnkistaedegwfmmedreetnwmcycnamtaaddyoeadlfcsdpykaocyemrewytyaycyemrewytytaadeetaadmhtaadmetaadnytaaddloxaxhdclaxdwkswmztpytnswtsecnblfbayajkdldeclqzzolrsnhljedsgminetytbnahatbyaahdcxkkguwsvyimjkvwteytwztyswvendtpmncpasfrrylprnhtkblndrgrmkoyjtbkrpamtaaddyoeadlocsdyykaeykaeykadykaocyemrewytyaycyhkrpnddrtaadeetaadmetaadnytaaddloxaxhdclaohnhffmvsbndslrfgclpfjejyatbdpebacnzokotofxntaoemvskpaowmryfnotfgaahdcxdlnbvecentssfsssgylnhkrstoytecrdlyadrekirfaybglahltalsrfcaeerobwamtaaddyoeadlocsdyykaeykaeykaoykaocyemrewytyaycyhkrpnddrtaadeetaadnltaaddloxaxhdclaorkrhkeytwsoykorletwstbwycagtbsotmeptjkesgwrfcmveskvdmngujzttgtdpaahdcxgrfgmuvyylmwcxjtttechplslgoegagaptdniatidmhdmebdwfryfsnsdkcplyvaamtaaddyoeadlncshfykaeykaeykaocyemrewytyaycytostatbngmdavolk";
const ACCOUNT_84_XPUB: &str = "xpub6BkU445MSEBXbPjD3g2c2ch6mn8yy1SXXQUM7EwjgYiq6Wt1NDwDZ45npqWcV8uQC5oi2gHuVukoCoZZyT4HKq8EpotPMqGqxdZRuapCQ23";
const ACCOUNT_49_XPUB: &str = "xpub6CtR1iF4dZPkEyXDwVf3HE74tSwXNMcHtBzX4gwz2UnPhJ54Jz5unHx2syYCCDkvVUmsmoYTmcaHXe1wJppvct4GMMaN5XAbRk7yGScRSte";

#[test]
fn text_import_returns_canonical_xpub_and_origin() {
    let a = import_account(format!("[37b5eed4/84h/0h/0h]{ACCOUNT_84_XPUB}"), "btc".into(), "mainnet".into()).unwrap();
    assert_eq!(a.xpub, ACCOUNT_84_XPUB);
    assert_eq!(a.master_fingerprint.as_deref(), Some("37b5eed4"));
    assert_eq!(a.path.as_deref(), Some("m/84'/0'/0'"));
}

#[test]
fn single_part_ur_import_works_on_both_chains() {
    for chain in ["btc", "xbt"] {
        let a = import_account(ACCOUNT_UR.into(), chain.into(), "mainnet".into()).unwrap();
        assert_eq!(a.xpub, ACCOUNT_84_XPUB, "{chain}");
    }
}

#[test]
fn non_bip84_account_is_refused_with_a_recognisable_message() {
    let e = import_account(format!("[37b5eed4/49h/0h/0h]{ACCOUNT_49_XPUB}"), "btc".into(), "mainnet".into())
        .unwrap_err()
        .to_string();
    assert!(e.contains("Native SegWit"), "{e}");
}

#[test]
fn scanner_reassembles_an_animated_qr() {
    let cbor = ur::ur::decode(ACCOUNT_UR).unwrap().1;
    let mut enc = ur::ur::Encoder::new(&cbor, 80, "crypto-account").unwrap();
    let s = UrAccountScanner::new();
    let mut frames = 0;
    while !s.is_complete() && frames < enc.fragment_count() * 3 {
        s.receive(enc.next_part().unwrap()).unwrap();
        frames += 1;
    }
    assert!(s.is_complete());
    assert_eq!(s.progress_percent(), 100);
    let a = s.result("btc".into(), "mainnet".into()).unwrap();
    assert_eq!(a.xpub, ACCOUNT_84_XPUB);
    assert_eq!(a.master_fingerprint.as_deref(), Some("37b5eed4"));
}
