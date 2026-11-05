// FORTIS PATCH: keeps the network graph, the scorers and the sync
// timestamps (node metrics) on the device.
//
// They are public, rebuildable data (the graph from gossip, the scores from
// payments made), and the graph alone is over a megabyte: read from VSS at
// every start, it was most of a mobile start's time. Everything else stays in
// the remote (VSS) store.

use std::future::Future;
use lightning::io;
use std::pin::Pin;
use std::sync::Arc;

use lightning::util::persist::{
	KVStore, KVStoreSync, NETWORK_GRAPH_PERSISTENCE_KEY, SCORER_PERSISTENCE_KEY,
};

use crate::io::utils::EXTERNAL_PATHFINDING_SCORES_CACHE_KEY;
use crate::types::DynStore;

type Fut<T> = Pin<Box<dyn Future<Output = Result<T, io::Error>> + Send>>;

pub(crate) struct SplitStore {
	remote: Arc<DynStore>,
	local: Arc<DynStore>,
}

impl SplitStore {
	pub(crate) fn new(remote: Arc<DynStore>, local: Arc<DynStore>) -> Self {
		Self { remote, local }
	}

	fn pick(&self, primary: &str, secondary: &str, key: &str) -> &Arc<DynStore> {
		if is_local(primary, secondary, key) {
			&self.local
		} else {
			&self.remote
		}
	}
}

/// The keys kept on the device (all in the root namespace).
pub(crate) fn is_local(primary: &str, secondary: &str, key: &str) -> bool {
	primary.is_empty()
		&& secondary.is_empty()
		&& (key == NETWORK_GRAPH_PERSISTENCE_KEY
			// Sync timestamps, kept with the graph: a device with no local
			// graph must not take a recent RGS timestamp from VSS and fetch
			// only an increment, and a restored wallet then rescans on-chain.
			|| key == crate::io::NODE_METRICS_KEY
			|| key == SCORER_PERSISTENCE_KEY
			|| key == EXTERNAL_PATHFINDING_SCORES_CACHE_KEY)
}

impl KVStoreSync for SplitStore {
	fn read(&self, p: &str, s: &str, k: &str) -> Result<Vec<u8>, io::Error> {
		KVStoreSync::read(&**self.pick(p, s, k), p, s, k)
	}
	fn write(&self, p: &str, s: &str, k: &str, buf: Vec<u8>) -> Result<(), io::Error> {
		KVStoreSync::write(&**self.pick(p, s, k), p, s, k, buf)
	}
	fn remove(&self, p: &str, s: &str, k: &str, lazy: bool) -> Result<(), io::Error> {
		KVStoreSync::remove(&**self.pick(p, s, k), p, s, k, lazy)
	}
	fn list(&self, p: &str, s: &str) -> Result<Vec<String>, io::Error> {
		let mut keys: Vec<String> = KVStoreSync::list(&*self.remote, p, s)?
			.into_iter()
			.filter(|k| !is_local(p, s, k))
			.collect();
		if p.is_empty() && s.is_empty() {
			for k in KVStoreSync::list(&*self.local, p, s).unwrap_or_default() {
				if is_local(p, s, &k) {
					keys.push(k);
				}
			}
		}
		Ok(keys)
	}
}

impl KVStore for SplitStore {
	fn read(&self, p: &str, s: &str, k: &str) -> Fut<Vec<u8>> {
		KVStore::read(&**self.pick(p, s, k), p, s, k)
	}
	fn write(&self, p: &str, s: &str, k: &str, buf: Vec<u8>) -> Fut<()> {
		KVStore::write(&**self.pick(p, s, k), p, s, k, buf)
	}
	fn remove(&self, p: &str, s: &str, k: &str, lazy: bool) -> Fut<()> {
		KVStore::remove(&**self.pick(p, s, k), p, s, k, lazy)
	}
	fn list(&self, p: &str, s: &str) -> Fut<Vec<String>> {
		let remote = KVStore::list(&*self.remote, p, s);
		let local = if p.is_empty() && s.is_empty() {
			Some(KVStore::list(&*self.local, p, s))
		} else {
			None
		};
		let (p, s) = (p.to_string(), s.to_string());
		Box::pin(async move {
			let mut keys: Vec<String> =
				remote.await?.into_iter().filter(|k| !is_local(&p, &s, k)).collect();
			if let Some(local) = local {
				for k in local.await.unwrap_or_default() {
					if is_local(&p, &s, &k) {
						keys.push(k);
					}
				}
			}
			Ok(keys)
		})
	}
}
