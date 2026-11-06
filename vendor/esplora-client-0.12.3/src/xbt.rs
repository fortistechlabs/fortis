// FORTIS XBT: block headers of the Bitcoin BLAKE2b chain (XBT).
//
// From the BLAKE2b activation height an XBT header is 164 bytes. Bytes 0..80
// keep version, prev, merkle root, time, bits and nonce at their usual
// offsets, so those 80 bytes are parsed as a [`Header`]. The block's id is
// not the double SHA-256 of those 80 bytes: callers take it from the index
// (the hash they asked for, `/blocks/tip/hash`, or a transaction's status),
// never from [`Header::block_hash`].

use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use bitcoin::merkle_tree::PartialMerkleTree;
use bitcoin::MerkleBlock;

use crate::Error;

/// Length of a Bitcoin header, and of the part of an XBT header parsed as one.
const HEADER_LEN: usize = 80;
/// Length of an XBT header from the BLAKE2b activation height.
const XBT_HEADER_LEN: usize = 164;

/// A header as served by `/block/:hash/header`: 80 bytes, or 164 on XBT (its first 80 bytes
/// are the [`Header`]). Any other length is an error.
pub(crate) fn header_from_bytes(bytes: &[u8]) -> Result<Header, Error> {
    match bytes.len() {
        HEADER_LEN | XBT_HEADER_LEN => Ok(deserialize(&bytes[..HEADER_LEN])?),
        _ => Err(Error::InvalidResponse),
    }
}

/// A `/tx/:txid/merkleblock-proof` whose header is 80 or 164 bytes. Both layouts can parse, so
/// the one whose partial merkle tree commits to its header's merkle root is taken; if neither
/// does, the 80-byte reading is returned as upstream would.
pub(crate) fn merkle_block_from_bytes(bytes: &[u8]) -> Result<MerkleBlock, Error> {
    let btc = deserialize::<MerkleBlock>(bytes);
    if matches!(&btc, Ok(mb) if proves_its_root(mb)) {
        return Ok(btc?);
    }
    if bytes.len() > XBT_HEADER_LEN {
        let header = deserialize::<Header>(&bytes[..HEADER_LEN]);
        let txn = deserialize::<PartialMerkleTree>(&bytes[XBT_HEADER_LEN..]);
        if let (Ok(header), Ok(txn)) = (header, txn) {
            let mb = MerkleBlock { header, txn };
            if proves_its_root(&mb) {
                return Ok(mb);
            }
        }
    }
    Ok(btc?)
}

fn proves_its_root(mb: &MerkleBlock) -> bool {
    let (mut matches, mut indexes) = (Vec::new(), Vec::new());
    mb.txn.extract_matches(&mut matches, &mut indexes).ok() == Some(mb.header.merkle_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hex::FromHex;
    use bitcoin::{BlockHash, Txid};
    use std::str::FromStr;

    // Regtest Bitcoin Knots with BLAKE2b active from height 20.
    /// Height 19: an 80-byte header, before activation.
    const HEADER_19: &str = "000000207a9a751a955100761138ca871a89cd2fdb3eb077a023f8b467b943ba79445d5052efcab10f34484caac393a8095e013adb867aaa98aa4769a72aaaa88cdb320a548eed6affff7f2000000000";
    const HASH_19: &str = "381f4f781d8f311efc488ce51d865382f221bb37fe078a71dfc7414bf058320a";
    /// Height 151: a 164-byte BLAKE2b header.
    const HEADER_151: &str = "000000b05c8c80a0fdc01e90ee9380b84e6917c9cb38baf21f85822f0f04c65996bbf110d61af53bbef8ae21487a38c1b65ea34b76fa90e2e7efa7d7052e1c09bd8cf5186d90ed6affff7f2001000000000000000000000000000000000000000000000000000000000000000300000000000000000000000000000000000000970000000000000000000000000000000000000000000000000000000000000000000000";
    const HASH_151: &str = "66495bdb1bf753b1a34f596ff023b51296f193d342212534a17d605beabe44c6";
    const MERKLE_ROOT_151: &str = "18f58cbd091c2e05d7a7efe7e290fa764ba35eb6c1387a4821aef8be3bf51ad6";
    /// `gettxoutproof` of TX_151 (the second of three transactions) in block 151.
    const PROOF_151: &str = "000000b05c8c80a0fdc01e90ee9380b84e6917c9cb38baf21f85822f0f04c65996bbf110d61af53bbef8ae21487a38c1b65ea34b76fa90e2e7efa7d7052e1c09bd8cf5186d90ed6affff7f200100000000000000000000000000000000000000000000000000000000000000030000000000000000000000000000000000000097000000000000000000000000000000000000000000000000000000000000000000000003000000031f60db2ada70f4ec2859c1c47ee3a5df915af65deec62d8691585f3428dd3ae76388863321f81ec79acb111110742f2947bbdfb44b1a51b0d33cd9f4a5dcaf06e81273e76476ea86f63af52d1219d871194d7a82a991e311112a4a66b78a9e8d010b";
    const TX_151: &str = "06afdca5f4d93cd3b0511a4bb4dfbb47292f74101111cb9ac71ef82133868863";

    fn bytes(hex: &str) -> Vec<u8> {
        Vec::from_hex(hex).unwrap()
    }

    #[test]
    fn parses_80_and_164_byte_headers() {
        let h19 = header_from_bytes(&bytes(HEADER_19)).unwrap();
        assert_eq!(h19.block_hash(), BlockHash::from_str(HASH_19).unwrap());

        let raw = bytes(HEADER_151);
        assert_eq!(raw.len(), 164);
        let h151 = header_from_bytes(&raw).unwrap();
        assert_eq!(h151.merkle_root.to_string(), MERKLE_ROOT_151);
        assert_eq!(h151.time, 1793953901);
        // The id is not the double SHA-256 of the first 80 bytes.
        assert_ne!(h151.block_hash(), BlockHash::from_str(HASH_151).unwrap());

        assert!(header_from_bytes(&raw[..100]).is_err());
        assert!(header_from_bytes(&raw[..79]).is_err());
    }

    fn assert_proves_tx_151(mb: &MerkleBlock) {
        let mut matches = Vec::new();
        let mut indexes = Vec::new();
        let root = mb.txn.extract_matches(&mut matches, &mut indexes).unwrap();
        assert_eq!(root, mb.header.merkle_root);
        assert_eq!(root.to_string(), MERKLE_ROOT_151);
        assert_eq!(matches, vec![Txid::from_str(TX_151).unwrap()]);
        assert_eq!(indexes, vec![1]);
    }

    #[test]
    fn merkle_block_164_byte_header() {
        let raw = bytes(PROOF_151);
        let mb = merkle_block_from_bytes(&raw).unwrap();
        assert_proves_tx_151(&mb);
        assert_eq!(mb.header, header_from_bytes(&bytes(HEADER_151)).unwrap());

        // The same proof with an 80-byte header still parses.
        let mut btc = raw[..80].to_vec();
        btc.extend_from_slice(&raw[164..]);
        assert_proves_tx_151(&merkle_block_from_bytes(&btc).unwrap());

        assert!(merkle_block_from_bytes(&raw[..200]).is_err());
    }
}
