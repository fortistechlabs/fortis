//! Checks on the regtest harness itself. Needs PostgreSQL (`initdb` /
//! `pg_ctl` on `PATH` or `FORTIS_PG_BIN`); run with
//! `cargo test -p fortis-lightning --test harness -- --ignored`.

mod common;

use std::path::Path;

/// Our own `fortis-ln-test-vss-<pid>-` processes (postgres, vss-server).
fn our_vss_procs() -> Vec<u32> {
    common::procs_referencing(&format!("fortis-ln-test-vss-{}-", std::process::id()))
}

/// `Vss::start_with` panicking after PostgreSQL is up must leave no
/// postgres or vss-server behind.
fn assert_start_failure_leaves_nothing(bin: &Path) {
    assert!(our_vss_procs().is_empty(), "stray processes before the test");
    let r = std::panic::catch_unwind(|| common::Vss::start_with(bin));
    let msg = r.err().and_then(|e| e.downcast_ref::<String>().cloned()).expect("start_with must panic");
    eprintln!("start_with({}) panicked as expected: {msg}", bin.display());
    // pg_ctl -w stop returns once postgres has exited; allow stragglers a moment.
    let left = common::poll("no stray processes", 10, || {
        let p = our_vss_procs();
        p.is_empty().then_some(p)
    });
    assert!(left.is_empty());
}

#[test]
#[ignore = "needs PostgreSQL; see module docs"]
fn vss_start_failure_cleans_up() {
    // Spawn fails after postgres is up.
    assert_start_failure_leaves_nothing(Path::new("/nonexistent/vss-server"));
    // Server spawns but exits at once (as an auth-less vss-server build does).
    assert_start_failure_leaves_nothing(Path::new("/bin/false"));
}
