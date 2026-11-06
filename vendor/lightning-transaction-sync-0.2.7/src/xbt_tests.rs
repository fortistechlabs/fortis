// FORTIS XBT: the Esplora sync reports the block ids the index serves. From the BLAKE2b
// activation height an XBT header is 164 bytes and its id is not the double SHA-256 of its
// first 80 bytes, so the sync must never compute an id from a header.
//
// Fixtures: regtest Bitcoin Knots with BLAKE2b active from height 20.

use crate::EsploraSyncClient;

use lightning::chain::transaction::TransactionData;
use lightning::chain::{Confirm, Filter};
use lightning::util::test_utils::TestLogger;

use bitcoin::block::Header;
use bitcoin::hex::FromHex;
use bitcoin::{BlockHash, ScriptBuf, Txid};

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

/// Height 19: an 80-byte header, before activation.
const HASH_19: &str = "381f4f781d8f311efc488ce51d865382f221bb37fe078a71dfc7414bf058320a";
const HEADER_19: &str = "000000207a9a751a955100761138ca871a89cd2fdb3eb077a023f8b467b943ba79445d5052efcab10f34484caac393a8095e013adb867aaa98aa4769a72aaaa88cdb320a548eed6affff7f2000000000";
/// Height 151: a 164-byte BLAKE2b header.
const HASH_151: &str = "66495bdb1bf753b1a34f596ff023b51296f193d342212534a17d605beabe44c6";
const HEADER_151: &str = "000000b05c8c80a0fdc01e90ee9380b84e6917c9cb38baf21f85822f0f04c65996bbf110d61af53bbef8ae21487a38c1b65ea34b76fa90e2e7efa7d7052e1c09bd8cf5186d90ed6affff7f2001000000000000000000000000000000000000000000000000000000000000000300000000000000000000000000000000000000970000000000000000000000000000000000000000000000000000000000000000000000";
/// `gettxoutproof` of TX_151 (the second of three transactions) in block 151.
const PROOF_151: &str = "000000b05c8c80a0fdc01e90ee9380b84e6917c9cb38baf21f85822f0f04c65996bbf110d61af53bbef8ae21487a38c1b65ea34b76fa90e2e7efa7d7052e1c09bd8cf5186d90ed6affff7f200100000000000000000000000000000000000000000000000000000000000000030000000000000000000000000000000000000097000000000000000000000000000000000000000000000000000000000000000000000003000000031f60db2ada70f4ec2859c1c47ee3a5df915af65deec62d8691585f3428dd3ae76388863321f81ec79acb111110742f2947bbdfb44b1a51b0d33cd9f4a5dcaf06e81273e76476ea86f63af52d1219d871194d7a82a991e311112a4a66b78a9e8d010b";
const TX_151: &str = "06afdca5f4d93cd3b0511a4bb4dfbb47292f74101111cb9ac71ef82133868863";
const RAW_TX_151: &str = "0200000001ccc58101d49ec5c662ca1cfce999288f7a9f3409d3693563ec44098ca3d40ff80000000069463043021f79632804a840a47e9d7cb1c2f02ca83bbff70e2a85e1a02de44a6a9656a948022067842a193b7cdfadac9187e4ec295dcd32c8524a34ae66dfcde43ca0a62ab3ed21210201ccffca3c6dc39eaecf87af1a1c77426a6cb6972417b650f326f2e057ed6ac8fdffffff02a8ff0f24010000001976a9147b0f02f7d75fc8f58eba96a36e6f35259ef97e3388ac00e1f505000000001600140855fb8e686600129602416ca60098343ef8bef796000000";

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// A minimal Esplora server: GET of a known path answers 200 with its body, anything else 404.
fn mock_esplora() -> (String, Routes) {
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let url = format!("http://{}", listener.local_addr().unwrap());
	let routes: Routes = Arc::new(Mutex::new(HashMap::new()));
	let served = Arc::clone(&routes);
	std::thread::spawn(move || {
		for stream in listener.incoming() {
			let mut stream = stream.unwrap();
			let mut request = Vec::new();
			let mut buf = [0u8; 1024];
			while !request.windows(4).any(|w| w == b"\r\n\r\n") {
				let n = stream.read(&mut buf).unwrap();
				if n == 0 {
					break;
				}
				request.extend_from_slice(&buf[..n]);
			}
			let request = String::from_utf8_lossy(&request);
			let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
			let body = served.lock().unwrap().get(&path).cloned();
			let (status, body) = match body {
				Some(body) => ("200 OK", body),
				None => ("404 Not Found", b"not found".to_vec()),
			};
			let head = format!(
				"HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
				body.len()
			);
			let _ = stream.write_all(head.as_bytes());
			let _ = stream.write_all(&body);
		}
	});
	(url, routes)
}

fn serve(routes: &Routes, path: String, body: impl Into<Vec<u8>>) {
	routes.lock().unwrap().insert(path, body.into());
}

fn serve_tip(routes: &Routes, hash: &str, header: &str, height: u32) {
	serve(routes, "/blocks/tip/hash".into(), hash);
	serve(routes, format!("/block/{hash}/header"), header);
	let status = format!(r#"{{"in_best_chain":true,"height":{height},"next_best":null}}"#);
	serve(routes, format!("/block/{hash}/status"), status);
}

/// Records what the sync reports; the methods that would leave LDK to compute an id panic.
#[derive(Default)]
struct Recorder {
	best_blocks: Mutex<Vec<(BlockHash, u32)>>,
	confirmed: Mutex<Vec<(BlockHash, u32, Txid)>>,
}

impl Confirm for Recorder {
	fn transactions_confirmed(&self, _: &Header, _: &TransactionData, _: u32) {
		panic!("the sync must pass the index's block id");
	}
	fn transaction_unconfirmed(&self, _: &Txid) {}
	fn best_block_updated(&self, _: &Header, _: u32) {
		panic!("the sync must pass the index's block id");
	}
	fn get_relevant_txids(&self) -> Vec<(Txid, u32, Option<BlockHash>)> {
		Vec::new()
	}
	fn transactions_confirmed_with_hash(
		&self, _: &Header, block_hash: BlockHash, txdata: &TransactionData, height: u32,
	) {
		let mut confirmed = self.confirmed.lock().unwrap();
		for (_, tx) in txdata {
			confirmed.push((block_hash, height, tx.compute_txid()));
		}
	}
	fn best_block_updated_with_hash(&self, _: &Header, block_hash: BlockHash, height: u32) {
		self.best_blocks.lock().unwrap().push((block_hash, height));
	}
}

#[test]
fn xbt_sync_uses_index_hashes() {
	let (url, routes) = mock_esplora();
	let logger = TestLogger::new();
	let sync = EsploraSyncClient::new(url, &logger);
	let recorder = Recorder::default();
	let hash_19 = BlockHash::from_str(HASH_19).unwrap();
	let hash_151 = BlockHash::from_str(HASH_151).unwrap();
	let txid = Txid::from_str(TX_151).unwrap();

	// An 80-byte header: its id is also its double SHA-256.
	serve_tip(&routes, HASH_19, HEADER_19, 19);
	sync.sync(vec![&recorder]).unwrap();
	assert_eq!(*recorder.best_blocks.lock().unwrap(), vec![(hash_19, 19)]);

	// A 164-byte header, with a watched transaction confirmed in it.
	serve_tip(&routes, HASH_151, HEADER_151, 151);
	serve(&routes, format!("/tx/{TX_151}/merkleblock-proof"), PROOF_151);
	let status = format!(
		r#"{{"confirmed":true,"block_height":151,"block_hash":"{HASH_151}","block_time":1793953901}}"#
	);
	serve(&routes, format!("/tx/{TX_151}/status"), status);
	serve(&routes, format!("/tx/{TX_151}/raw"), Vec::<u8>::from_hex(RAW_TX_151).unwrap());
	sync.register_tx(&txid, &ScriptBuf::new());
	sync.sync(vec![&recorder]).unwrap();

	let header_151: Header =
		bitcoin::consensus::deserialize(&Vec::<u8>::from_hex(&HEADER_151[..160]).unwrap()).unwrap();
	assert_ne!(header_151.block_hash(), hash_151);
	assert_eq!(*recorder.best_blocks.lock().unwrap(), vec![(hash_19, 19), (hash_151, 151)]);
	assert_eq!(*recorder.confirmed.lock().unwrap(), vec![(hash_151, 151, txid)]);
}
