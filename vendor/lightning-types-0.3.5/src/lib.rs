// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

#![crate_name = "lightning_types"]

//! Various types which are used in the lightning network.
//!
//! See the `lightning` crate for usage of these.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(fuzzing), deny(missing_docs))]
#![forbid(unsafe_code)]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::private_intra_doc_links)]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;
extern crate core;

pub mod features;
pub mod payment;
pub mod routing;
pub mod string;
mod unicode;

/// FORTIS XBT: the process-wide XBT (BLAKE2b chain) switch; see `lightning::xbt`.
pub mod xbt {
	use core::sync::atomic::{AtomicBool, Ordering};
	static ENABLED: AtomicBool = AtomicBool::new(false);
	/// Turns XBT mode on for this process (before any node is built).
	pub fn set_enabled(on: bool) {
		ENABLED.store(on, Ordering::SeqCst);
		bitcoin::block::xbt_ids::set_enabled(on);
	}
	/// Whether this process follows the BLAKE2b rules.
	pub fn enabled() -> bool {
		ENABLED.load(Ordering::Relaxed)
	}
}
