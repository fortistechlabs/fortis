//! Importing a hardware wallet's account key for a watch-only wallet: the text
//! a device shows or exports (`zpub…`, `[fp/84h/0h/0h]xpub…`), or a BC-UR QR
//! code — single-part or an animated multi-part sequence — carrying a
//! `crypto-account` (BCR-2020-015) or `crypto-hdkey` (BCR-2020-007), in either
//! the v1 (`303`…) or v2 (`40303`…) CBOR tag family.
//!
//! Pure parsing — no keys beyond public ones, no I/O.

use std::str::FromStr;

use bitcoin::bip32::{ChainCode, ChildNumber, DerivationPath, Fingerprint, Xpub};
use bitcoin::secp256k1::PublicKey;
use bitcoin::NetworkKind;
use minicbor::Decoder;

use crate::chain::ChainParams;
use crate::error::{Result, WalletError};
use crate::keys::parse_account_xpub;

/// An account key plus whatever origin information came with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountImport {
    pub xpub: Xpub,
    /// The device's master key fingerprint, when the export carried it.
    pub master_fingerprint: Option<Fingerprint>,
    /// Full derivation path from the master key, when the export carried it.
    pub path: Option<DerivationPath>,
}

// CBOR tags: v1 (BCR-2020-006) and their v2 renumbering (BCR-2020-006 rev. 2023).
const TAG_HDKEY: [u64; 2] = [303, 40303];
const TAG_KEYPATH: [u64; 2] = [304, 40304];
const TAG_COIN_INFO: [u64; 2] = [305, 40305];
const TAG_OUTPUT: [u64; 2] = [308, 40308];
/// `wpkh(…)` — the only script expression a BIP-84 account comes wrapped in.
const TAG_WPKH: u64 = 404;

fn bad(msg: impl Into<String>) -> WalletError {
    WalletError::InvalidInput(format!("account import: {}", msg.into()))
}

fn cbor_err(e: minicbor::decode::Error) -> WalletError {
    bad(format!("malformed CBOR: {e}"))
}

/// Parse a pasted or single-frame-scanned export.
pub fn parse_account_import(text: &str) -> Result<AccountImport> {
    let text = text.trim();
    if text.len() > 3 && text[..3].eq_ignore_ascii_case("ur:") {
        let (kind, payload) = ur::ur::decode(text).map_err(|e| bad(format!("invalid UR: {e}")))?;
        if kind != ur::ur::Kind::SinglePart {
            return Err(bad("this is one frame of an animated QR code — scan it with the camera"));
        }
        return from_ur(&ur_type(text), &payload);
    }
    if let Some(rest) = text.strip_prefix('[') {
        let (origin, key) = rest.split_once(']').ok_or_else(|| bad("unterminated key origin"))?;
        let (fp, path) = origin.split_once('/').unwrap_or((origin, ""));
        let fp = Fingerprint::from_str(fp).map_err(|_| bad("invalid fingerprint in key origin"))?;
        let path = DerivationPath::from_str(&format!("m/{path}"))
            .map_err(|_| bad("invalid path in key origin"))?;
        return Ok(AccountImport {
            xpub: parse_account_xpub(key)?,
            master_fingerprint: Some(fp),
            path: Some(path),
        });
    }
    Ok(AccountImport { xpub: parse_account_xpub(text)?, master_fingerprint: None, path: None })
}

/// The `<type>` of `ur:<type>/…`, lower-cased.
fn ur_type(text: &str) -> String {
    text[3..].split('/').next().unwrap_or("").to_ascii_lowercase()
}

fn from_ur(ur_type: &str, cbor: &[u8]) -> Result<AccountImport> {
    let mut d = Decoder::new(cbor);
    match ur_type {
        "crypto-account" => decode_account(&mut d),
        "crypto-hdkey" | "hdkey" => decode_hdkey(&mut d),
        other => Err(bad(format!("unsupported QR code type `{other}` — export the account's public key"))),
    }
}

/// `{1: master-fingerprint, 2: [output-descriptor…]}` → its `wpkh` key.
fn decode_account(d: &mut Decoder) -> Result<AccountImport> {
    let n = d.map().map_err(cbor_err)?.ok_or_else(|| bad("indefinite-length map"))?;
    let mut master = None;
    let mut found = None;
    for _ in 0..n {
        match d.u32().map_err(cbor_err)? {
            1 => master = Some(Fingerprint::from(d.u32().map_err(cbor_err)?.to_be_bytes())),
            2 => {
                let len = d.array().map_err(cbor_err)?.ok_or_else(|| bad("indefinite-length array"))?;
                for _ in 0..len {
                    let outer = d.tag().map_err(cbor_err)?.as_u64();
                    if !TAG_OUTPUT.contains(&outer) {
                        return Err(bad(format!("unexpected tag {outer} in output descriptors")));
                    }
                    let script = d.tag().map_err(cbor_err)?.as_u64();
                    if script == TAG_WPKH && found.is_none() {
                        let key = d.tag().map_err(cbor_err)?.as_u64();
                        if !TAG_HDKEY.contains(&key) {
                            return Err(bad("wpkh descriptor without an HD key"));
                        }
                        found = Some(decode_hdkey_map(d)?);
                    } else {
                        d.skip().map_err(cbor_err)?;
                    }
                }
            }
            _ => d.skip().map_err(cbor_err)?,
        }
    }
    let mut a = found.ok_or_else(|| bad("no Native SegWit (BIP-84) account in this export"))?;
    a.master_fingerprint = a.master_fingerprint.or(master);
    Ok(a)
}

/// A lone `crypto-hdkey`, optionally tagged.
fn decode_hdkey(d: &mut Decoder) -> Result<AccountImport> {
    if d.datatype().map_err(cbor_err)? == minicbor::data::Type::Tag {
        let t = d.tag().map_err(cbor_err)?.as_u64();
        if !TAG_HDKEY.contains(&t) {
            return Err(bad(format!("unexpected tag {t} for an HD key")));
        }
    }
    decode_hdkey_map(d)
}

/// The `crypto-hdkey` map body.
fn decode_hdkey_map(d: &mut Decoder) -> Result<AccountImport> {
    let n = d.map().map_err(cbor_err)?.ok_or_else(|| bad("indefinite-length map"))?;
    let (mut is_master, mut is_private) = (false, false);
    let (mut key, mut chain_code) = (None, None);
    let mut network = NetworkKind::Main;
    let mut components: Option<Vec<ChildNumber>> = None;
    let (mut source_fp, mut depth) = (None, None);
    let mut parent_fp = Fingerprint::default();
    for _ in 0..n {
        match d.u32().map_err(cbor_err)? {
            1 => is_master = d.bool().map_err(cbor_err)?,
            2 => is_private = d.bool().map_err(cbor_err)?,
            3 => key = Some(d.bytes().map_err(cbor_err)?.to_vec()),
            4 => chain_code = Some(d.bytes().map_err(cbor_err)?.to_vec()),
            5 => {
                expect_tag(d, &TAG_COIN_INFO)?;
                let m = d.map().map_err(cbor_err)?.ok_or_else(|| bad("indefinite-length map"))?;
                for _ in 0..m {
                    match d.u32().map_err(cbor_err)? {
                        2 => {
                            network = match d.u32().map_err(cbor_err)? {
                                0 => NetworkKind::Main,
                                1 => NetworkKind::Test,
                                other => return Err(bad(format!("unknown network {other}"))),
                            }
                        }
                        _ => d.skip().map_err(cbor_err)?,
                    }
                }
            }
            6 => {
                expect_tag(d, &TAG_KEYPATH)?;
                let m = d.map().map_err(cbor_err)?.ok_or_else(|| bad("indefinite-length map"))?;
                for _ in 0..m {
                    match d.u32().map_err(cbor_err)? {
                        1 => components = Some(decode_components(d)?),
                        2 => source_fp = Some(Fingerprint::from(d.u32().map_err(cbor_err)?.to_be_bytes())),
                        3 => depth = Some(d.u32().map_err(cbor_err)?),
                        _ => d.skip().map_err(cbor_err)?,
                    }
                }
            }
            8 => parent_fp = Fingerprint::from(d.u32().map_err(cbor_err)?.to_be_bytes()),
            _ => d.skip().map_err(cbor_err)?,
        }
    }
    if is_private {
        return Err(bad("this is a private key — export the public key only"));
    }
    let key = key.ok_or_else(|| bad("missing key data"))?;
    let public_key = PublicKey::from_slice(&key).map_err(|_| bad("invalid public key"))?;
    let chain_code: [u8; 32] = chain_code
        .ok_or_else(|| bad("missing chain code"))?
        .try_into()
        .map_err(|_| bad("chain code must be 32 bytes"))?;
    let components = if is_master { Some(Vec::new()) } else { components };
    let depth = match (depth, &components) {
        (Some(d), _) => d,
        (None, Some(c)) => c.len() as u32,
        (None, None) => return Err(bad("missing key origin")),
    };
    let child_number = components
        .as_ref()
        .and_then(|c| c.last().copied())
        .unwrap_or(ChildNumber::Normal { index: 0 });
    let xpub = Xpub {
        network,
        depth: u8::try_from(depth).map_err(|_| bad("derivation too deep"))?,
        parent_fingerprint: parent_fp,
        child_number,
        public_key,
        chain_code: ChainCode::from(chain_code),
    };
    Ok(AccountImport {
        xpub,
        master_fingerprint: source_fp,
        path: components.map(DerivationPath::from),
    })
}

fn expect_tag(d: &mut Decoder, tags: &[u64; 2]) -> Result<()> {
    let t = d.tag().map_err(cbor_err)?.as_u64();
    if tags.contains(&t) {
        Ok(())
    } else {
        Err(bad(format!("unexpected tag {t}")))
    }
}

/// `[index, hardened, index, hardened, …]` — wildcards and ranges (arrays in
/// place of an index) aren't an account path.
fn decode_components(d: &mut Decoder) -> Result<Vec<ChildNumber>> {
    let len = d.array().map_err(cbor_err)?.ok_or_else(|| bad("indefinite-length array"))?;
    if len % 2 != 0 {
        return Err(bad("odd-length key path"));
    }
    let mut out = Vec::with_capacity(len as usize / 2);
    for _ in 0..len / 2 {
        if d.datatype().map_err(cbor_err)? == minicbor::data::Type::Array {
            return Err(bad("key path has a wildcard"));
        }
        let index = d.u32().map_err(cbor_err)?;
        let hardened = d.bool().map_err(cbor_err)?;
        let child = if hardened {
            ChildNumber::from_hardened_idx(index)
        } else {
            ChildNumber::from_normal_idx(index)
        };
        out.push(child.map_err(|_| bad("key path index out of range"))?);
    }
    Ok(out)
}

/// Check the import is a BIP-84 (Native SegWit) account on `params`' coin type —
/// the only layout fortis derives addresses for.
pub fn check_bip84_account(import: &AccountImport, params: &ChainParams) -> Result<()> {
    let not_84 = |why: &str| bad(format!("not a Native SegWit (BIP-84) account: {why}"));
    if NetworkKind::from(params.network) != import.xpub.network {
        return Err(not_84("key is for a different network"));
    }
    if import.xpub.depth != 3 || !import.xpub.child_number.is_hardened() {
        return Err(not_84("expected an account-level key (m/84'/coin'/account')"));
    }
    if let Some(path) = &import.path {
        let want_purpose = ChildNumber::from_hardened_idx(84).expect("in range");
        let want_coin = ChildNumber::from_hardened_idx(params.bip44_coin_type).expect("in range");
        match path.as_ref() {
            [purpose, coin, account] if account.is_hardened() => {
                if *purpose != want_purpose {
                    return Err(not_84(&format!("path is {path}")));
                }
                if *coin != want_coin {
                    return Err(not_84(&format!("path {path} is for another coin")));
                }
            }
            _ => return Err(not_84(&format!("path is {path}"))),
        }
    }
    Ok(())
}

enum Frames {
    Empty,
    Single { ur_type: String, payload: Vec<u8> },
    Multi(Box<ur::ur::Decoder>),
}

/// Accumulates the frames of an animated (multi-part) UR QR code. A single-part
/// UR is accepted too and completes at once.
pub struct UrAccountDecoder {
    frames: Frames,
}

impl Default for UrAccountDecoder {
    fn default() -> Self {
        Self { frames: Frames::Empty }
    }
}

impl UrAccountDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one scanned frame. Repeats and out-of-order frames are fine — an
    /// animated QR loops until the decoder has enough.
    pub fn receive(&mut self, part: &str) -> Result<()> {
        let part = part.trim();
        if !(part.len() > 3 && part[..3].eq_ignore_ascii_case("ur:")) {
            return Err(bad("not a UR QR code"));
        }
        let (kind, payload) = ur::ur::decode(part).map_err(|e| bad(format!("invalid UR: {e}")))?;
        match kind {
            ur::ur::Kind::SinglePart => {
                self.frames = Frames::Single { ur_type: ur_type(part), payload };
            }
            ur::ur::Kind::MultiPart => {
                if !matches!(self.frames, Frames::Multi(_)) {
                    self.frames = Frames::Multi(Box::default());
                }
                if let Frames::Multi(d) = &mut self.frames {
                    d.receive(part).map_err(|e| bad(format!("invalid UR frame: {e}")))?;
                }
            }
        }
        Ok(())
    }

    /// 0–100, for a progress bar.
    pub fn progress_percent(&self) -> u8 {
        match &self.frames {
            Frames::Empty => 0,
            Frames::Single { .. } => 100,
            Frames::Multi(d) if d.complete() => 100,
            Frames::Multi(d) => {
                let total = d.fragment_count().max(1);
                let done = d.resolved_fragment_count().unwrap_or(0);
                ((done * 100 / total) as u8).min(99)
            }
        }
    }

    pub fn is_complete(&self) -> bool {
        match &self.frames {
            Frames::Empty => false,
            Frames::Single { .. } => true,
            Frames::Multi(d) => d.complete(),
        }
    }

    /// The decoded account, once complete.
    pub fn result(&self) -> Result<AccountImport> {
        match &self.frames {
            Frames::Single { ur_type, payload } => from_ur(ur_type, payload),
            Frames::Multi(d) => {
                let msg = d
                    .message()
                    .map_err(|e| bad(format!("invalid UR: {e}")))?
                    .ok_or_else(|| bad("QR code not fully scanned yet"))?;
                from_ur(&d.ur_type().unwrap_or("").to_ascii_lowercase(), &msg)
            }
            Frames::Empty => Err(bad("QR code not fully scanned yet")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    // BCR-2020-015 test vector: account #0 of "shield group erode awake lock
    // sausage cash glare wave crew flame glove", BTC mainnet.
    const ACCOUNT_UR: &str = "ur:crypto-account/oeadcyemrewytyaolttaadeetaadmutaaddloxaxhdclaxwmfmdeiamecsdsemgtvsjzcncygrkowtrontzschgezokstswkkscfmklrtauteyaahdcxiehfonurdppfyntapejpproypegrdawkgmaewejlsfdtsrfybdehcaflmtrlbdhpamtaaddyoeadlncsdwykaeykaeykaocyemrewytyaycynlytsnyltaadeetaadmhtaadmwtaaddloxaxhdclaostvelfemdyynwydwyaievosrgmambklovabdgypdglldvespsthysadamhpmjeinaahdcxntdllnaaeykoytdacygegwhgjsiyonpywmcmrpwphsvodsrerozsbyaxluzcoxdpamtaaddyoeadlncsehykaeykaeykaocyemrewytyaycypdbskeuytaadeetaadmwtaaddloxaxhdclaxzcfxeegdrpmogrgwkbzctlttweadkiengrwlhtprremouoluutqdpfbncedkynfhaahdcxjpwevdeogthttkmeswzcolcpsaahcfnshkhtehytclmnteatmoteadtlwynnftloamtaaddyoeadlncsghykaeykaeykaocyemrewytyaycybthlvytstaadeetaadmhtaadnytaaddloxaxhdclaxhhsnhdrpftdwuocntilydibehnecmovdfekpjkclcslasbhkpawsaddmcmmnahnyaahdcxlotedtndfymyltclhlmtpfsadscnhtztaolbnnkistaedegwfmmedreetnwmcycnamtaaddyoeadlfcsdpykaocyemrewytyaycyemrewytytaadeetaadmhtaadmetaadnytaaddloxaxhdclaxdwkswmztpytnswtsecnblfbayajkdldeclqzzolrsnhljedsgminetytbnahatbyaahdcxkkguwsvyimjkvwteytwztyswvendtpmncpasfrrylprnhtkblndrgrmkoyjtbkrpamtaaddyoeadlocsdyykaeykaeykadykaocyemrewytyaycyhkrpnddrtaadeetaadmetaadnytaaddloxaxhdclaohnhffmvsbndslrfgclpfjejyatbdpebacnzokotofxntaoemvskpaowmryfnotfgaahdcxdlnbvecentssfsssgylnhkrstoytecrdlyadrekirfaybglahltalsrfcaeerobwamtaaddyoeadlocsdyykaeykaeykaoykaocyemrewytyaycyhkrpnddrtaadeetaadnltaaddloxaxhdclaorkrhkeytwsoykorletwstbwycagtbsotmeptjkesgwrfcmveskvdmngujzttgtdpaahdcxgrfgmuvyylmwcxjtttechplslgoegagaptdniatidmhdmebdwfryfsnsdkcplyvaamtaaddyoeadlncshfykaeykaeykaocyemrewytyaycytostatbngmdavolk";
    // Its wpkh([37b5eed4/84'/0'/0']…) descriptor key, from the same vector.
    const ACCOUNT_84_XPUB: &str = "xpub6BkU445MSEBXbPjD3g2c2ch6mn8yy1SXXQUM7EwjgYiq6Wt1NDwDZ45npqWcV8uQC5oi2gHuVukoCoZZyT4HKq8EpotPMqGqxdZRuapCQ23";
    const ACCOUNT_49_XPUB: &str = "xpub6CtR1iF4dZPkEyXDwVf3HE74tSwXNMcHtBzX4gwz2UnPhJ54Jz5unHx2syYCCDkvVUmsmoYTmcaHXe1wJppvct4GMMaN5XAbRk7yGScRSte";

    // BCR-2020-007 v2 test vector: a testnet m/44'/1'/1'/0/1 key.
    const HDKEY_V2_UR: &str = "ur:hdkey/onaxhdclaojlvoechgferkdpqdiabdrflawshlhdmdcemtfnlrctghchbdolvwsednvdztbgolaahdcxtottgostdkhfdahdlykkecbbweskrymwflvdylgerkloswtbrpfdbsticmwylklpahtantjsoyaoadamtantjooyadlecsdwykadykadykaewkadwkaycywlcscewfjnkpvllt";

    fn fp(s: &str) -> Fingerprint {
        Fingerprint::from_str(s).unwrap()
    }

    fn path(s: &str) -> DerivationPath {
        DerivationPath::from_str(s).unwrap()
    }

    /// The account CBOR inside [`ACCOUNT_UR`], for building other encodings.
    fn account_cbor() -> Vec<u8> {
        ur::ur::decode(ACCOUNT_UR).unwrap().1
    }

    #[test]
    fn crypto_account_ur_yields_its_bip84_key() {
        let a = parse_account_import(ACCOUNT_UR).unwrap();
        assert_eq!(a.xpub.to_string(), ACCOUNT_84_XPUB);
        assert_eq!(a.master_fingerprint, Some(fp("37b5eed4")));
        assert_eq!(a.path, Some(path("m/84'/0'/0'")));
    }

    #[test]
    fn ur_is_case_insensitive() {
        // QR alphanumeric mode carries URs upper-cased.
        let a = parse_account_import(&ACCOUNT_UR.to_ascii_uppercase()).unwrap();
        assert_eq!(a.xpub.to_string(), ACCOUNT_84_XPUB);
    }

    #[test]
    fn hdkey_v2_ur_decodes_key_and_origin() {
        let a = parse_account_import(HDKEY_V2_UR).unwrap();
        assert_eq!(a.path, Some(path("m/44'/1'/1'/0/1")));
        assert_eq!(a.xpub.depth, 5);
        assert_eq!(a.xpub.parent_fingerprint, fp("e9181cf3"));
        assert_eq!(a.xpub.network, bitcoin::NetworkKind::Test);
        assert_eq!(
            hex::encode(a.xpub.public_key.serialize()),
            "026fe2355745bb2db3630bbc80ef5d58951c963c841f54170ba6e5c12be7fc12a6"
        );
        assert_eq!(a.master_fingerprint, None);
    }

    #[test]
    fn crypto_hdkey_v1_ur_decodes() {
        // The wpkh entry of the account vector, re-wrapped as a lone v1
        // crypto-hdkey: 303({3: key, 4: chain code, 6: 304({1: path, 2: fp}), 8: parent}).
        let key = hex::decode("03fd433450b6924b4f7efdd5d1ed017d364be95ab2b592dc8bddb3b00c1c24f63f").unwrap();
        let cc = hex::decode("72ede7334d5acf91c6fda622c205199c595a31f9218ed30792d301d5ee9e3a88").unwrap();
        let mut buf = Vec::new();
        let mut e = minicbor::Encoder::new(&mut buf);
        e.map(4).unwrap();
        e.u8(3).unwrap().bytes(&key).unwrap();
        e.u8(4).unwrap().bytes(&cc).unwrap();
        e.u8(6).unwrap().tag(minicbor::data::Tag::new(304)).unwrap().map(2).unwrap();
        e.u8(1).unwrap().array(6).unwrap();
        e.u32(84).unwrap().bool(true).unwrap().u32(0).unwrap().bool(true).unwrap().u32(0).unwrap().bool(true).unwrap();
        e.u8(2).unwrap().u32(0x37b5eed4).unwrap();
        e.u8(8).unwrap().u32(224256471).unwrap();
        let text = ur::ur::encode(&buf, &ur::ur::Type::Custom("crypto-hdkey"));

        let a = parse_account_import(&text).unwrap();
        assert_eq!(a.xpub.to_string(), ACCOUNT_84_XPUB);
        assert_eq!(a.master_fingerprint, Some(fp("37b5eed4")));
        assert_eq!(a.path, Some(path("m/84'/0'/0'")));
    }

    #[test]
    fn account_without_a_wpkh_descriptor_is_rejected() {
        // {1: fp, 2: [308(404 … )]} with only the sh(wpkh) entry would need
        // hand-built CBOR; an empty descriptor list exercises the same branch.
        let mut buf = Vec::new();
        let mut e = minicbor::Encoder::new(&mut buf);
        e.map(2).unwrap().u8(1).unwrap().u32(0x37b5eed4).unwrap().u8(2).unwrap().array(0).unwrap();
        let text = ur::ur::encode(&buf, &ur::ur::Type::Custom("crypto-account"));
        let err = parse_account_import(&text).unwrap_err().to_string();
        assert!(err.contains("Native SegWit"), "{err}");
    }

    #[test]
    fn unsupported_ur_type_is_rejected() {
        let text = ur::ur::encode(b"hello", &ur::ur::Type::Bytes);
        assert!(parse_account_import(&text).is_err());
    }

    #[test]
    fn plain_and_slip132_keys_still_parse() {
        let a = parse_account_import(ACCOUNT_84_XPUB).unwrap();
        assert_eq!(a.xpub.to_string(), ACCOUNT_84_XPUB);
        assert_eq!(a.master_fingerprint, None);
        assert_eq!(a.path, None);
    }

    #[test]
    fn origin_prefixed_key_carries_fingerprint_and_path() {
        for text in [
            format!("[37b5eed4/84h/0h/0h]{ACCOUNT_84_XPUB}"),
            format!("[37B5EED4/84'/0'/0']{ACCOUNT_84_XPUB}"),
            format!("  [37b5eed4/84'/0'/0']{ACCOUNT_84_XPUB}\n"),
        ] {
            let a = parse_account_import(&text).unwrap();
            assert_eq!(a.xpub.to_string(), ACCOUNT_84_XPUB, "{text}");
            assert_eq!(a.master_fingerprint, Some(fp("37b5eed4")), "{text}");
            assert_eq!(a.path, Some(path("m/84'/0'/0'")), "{text}");
        }
    }

    #[test]
    fn malformed_origin_is_rejected() {
        assert!(parse_account_import(&format!("[37b5eed4/84h/0h/0h{ACCOUNT_84_XPUB}")).is_err());
        assert!(parse_account_import(&format!("[zz/84h/0h/0h]{ACCOUNT_84_XPUB}")).is_err());
    }

    #[test]
    fn bip84_check_accepts_the_mainnet_account() {
        let a = parse_account_import(ACCOUNT_UR).unwrap();
        check_bip84_account(&a, &ChainParams::bitcoin()).unwrap();
        // Same addresses on XBT — the BLAKE2b fork keeps coin type 0.
        check_bip84_account(&a, &ChainParams::blake2b()).unwrap();
    }

    #[test]
    fn bip84_check_accepts_a_bare_account_level_key() {
        let a = parse_account_import(ACCOUNT_84_XPUB).unwrap();
        check_bip84_account(&a, &ChainParams::bitcoin()).unwrap();
    }

    #[test]
    fn bip84_check_rejects_other_purposes() {
        let a = parse_account_import(&format!("[37b5eed4/49h/0h/0h]{ACCOUNT_49_XPUB}")).unwrap();
        let err = check_bip84_account(&a, &ChainParams::bitcoin()).unwrap_err().to_string();
        assert!(err.contains("Native SegWit"), "{err}");
    }

    #[test]
    fn bip84_check_rejects_wrong_coin_type_and_non_account_depth() {
        let hd = parse_account_import(HDKEY_V2_UR).unwrap();
        assert!(check_bip84_account(&hd, &ChainParams::bitcoin_regtest()).is_err());

        let wrong_coin = AccountImport { path: Some(path("m/84'/1'/0'")), ..parse_account_import(ACCOUNT_84_XPUB).unwrap() };
        assert!(check_bip84_account(&wrong_coin, &ChainParams::bitcoin()).is_err());
    }

    #[test]
    fn bip84_check_rejects_a_master_key() {
        let master = "xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8";
        let a = parse_account_import(master).unwrap();
        assert!(check_bip84_account(&a, &ChainParams::bitcoin()).is_err());
    }

    #[test]
    fn multipart_ur_reassembles() {
        let cbor = account_cbor();
        let mut enc = ur::ur::Encoder::new(&cbor, 60, "crypto-account").unwrap();
        let mut dec = UrAccountDecoder::new();
        let mut last = 0;
        for _ in 0..(enc.fragment_count() * 3) {
            dec.receive(&enc.next_part().unwrap()).unwrap();
            let p = dec.progress_percent();
            assert!(p >= last && p <= 100);
            last = p;
            if dec.is_complete() {
                break;
            }
        }
        assert!(dec.is_complete());
        assert_eq!(dec.progress_percent(), 100);
        assert_eq!(dec.result().unwrap().xpub.to_string(), ACCOUNT_84_XPUB);
    }

    #[test]
    fn decoder_accepts_a_single_part_ur() {
        let mut dec = UrAccountDecoder::new();
        dec.receive(ACCOUNT_UR).unwrap();
        assert!(dec.is_complete());
        assert_eq!(dec.result().unwrap().xpub.to_string(), ACCOUNT_84_XPUB);
    }

    #[test]
    fn decoder_result_before_completion_is_an_error() {
        let cbor = account_cbor();
        let mut enc = ur::ur::Encoder::new(&cbor, 60, "crypto-account").unwrap();
        let mut dec = UrAccountDecoder::new();
        dec.receive(&enc.next_part().unwrap()).unwrap();
        assert!(!dec.is_complete());
        assert!(dec.result().is_err());
    }
}
