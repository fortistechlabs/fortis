//! Parallel, ordered block fetching: a hash stage looks heights up in batches
//! of 1000, `workers` threads fetch and parse raw blocks, and a reorder stage
//! hands `ParsedBlock`s to the single writer strictly in height order.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::Result;
use bitcoin::BlockHash;
use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender};

use crate::chain::HeaderFormat;
use crate::extract::{parse_and_extract, ParsedBlock};
use crate::source::BlockSource;

pub struct FetchConfig {
    pub workers: usize,
}

const HASH_BATCH: u32 = 1000;
const TICK: Duration = Duration::from_millis(10);

struct State {
    /// Next height to hand to the consumer.
    next: u32,
    ready: BTreeMap<u32, Result<ParsedBlock>>,
    /// The stream ended (finished, failed, or the consumer went away).
    done: bool,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
    stop: Arc<AtomicBool>,
}

impl Shared {
    fn halted(&self, st: &State) -> bool {
        st.done || self.stop.load(Ordering::SeqCst)
    }

    fn put(&self, h: u32, r: Result<ParsedBlock>) {
        self.state.lock().unwrap().ready.insert(h, r);
        self.cv.notify_all();
    }

    fn finish(&self) {
        self.state.lock().unwrap().done = true;
        self.cv.notify_all();
    }
}

/// Stream blocks `from..=to` in ascending height order. The first error is
/// delivered after the good prefix before it and ends the stream; setting
/// `stop` (or dropping the receiver) ends it within ~10 ms. At most
/// `2 * workers` blocks are fetched ahead of the channel, which holds
/// `4 * workers`.
pub fn fetch_range(
    src: Arc<dyn BlockSource>,
    fmt: HeaderFormat,
    from: u32,
    to: u32,
    cfg: &FetchConfig,
    stop: Arc<AtomicBool>,
) -> Receiver<Result<ParsedBlock>> {
    let workers = cfg.workers.max(1);
    let window = 2 * workers as u32;
    let (out_tx, out_rx) = crossbeam_channel::bounded(4 * workers);
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            next: from,
            ready: BTreeMap::new(),
            done: to < from,
        }),
        cv: Condvar::new(),
        stop,
    });
    let (job_tx, job_rx) = crossbeam_channel::bounded::<(u32, BlockHash)>(2 * HASH_BATCH as usize);

    {
        let (shared, src) = (shared.clone(), src.clone());
        std::thread::spawn(move || hash_stage(&*src, &shared, from, to, job_tx));
    }
    for _ in 0..workers {
        let (shared, src, job_rx) = (shared.clone(), src.clone(), job_rx.clone());
        std::thread::spawn(move || worker(&*src, fmt, &shared, window, job_rx));
    }
    std::thread::spawn(move || deliver(&shared, to, out_tx));
    out_rx
}

fn hash_stage(
    src: &dyn BlockSource,
    shared: &Shared,
    from: u32,
    to: u32,
    jobs: Sender<(u32, BlockHash)>,
) {
    let mut start = from;
    while start <= to {
        let n = (to - start + 1).min(HASH_BATCH);
        let hashes = match src.hashes(start, n) {
            Ok(h) => h,
            Err(e) => {
                return shared.put(start, Err(e.context(format!("block hashes from {start}"))))
            }
        };
        for (h, hash) in (start..).zip(hashes) {
            let mut job = (h, hash);
            loop {
                if shared.halted(&shared.state.lock().unwrap()) {
                    return;
                }
                match jobs.send_timeout(job, TICK) {
                    Ok(()) => break,
                    Err(SendTimeoutError::Timeout(j)) => job = j,
                    Err(SendTimeoutError::Disconnected(_)) => return,
                }
            }
        }
        start += n;
    }
}

fn worker(
    src: &dyn BlockSource,
    fmt: HeaderFormat,
    shared: &Shared,
    window: u32,
    jobs: Receiver<(u32, BlockHash)>,
) {
    loop {
        let (h, hash) = match jobs.recv_timeout(TICK) {
            Ok(j) => j,
            Err(RecvTimeoutError::Timeout) => {
                if shared.halted(&shared.state.lock().unwrap()) {
                    return;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // Don't run more than `window` blocks ahead of the consumer.
        let mut st = shared.state.lock().unwrap();
        while h >= st.next + window && !shared.halted(&st) {
            st = shared.cv.wait_timeout(st, TICK).unwrap().0;
        }
        if shared.halted(&st) {
            return;
        }
        drop(st);
        let r = src
            .raw_block(&hash)
            .and_then(|bytes| parse_and_extract(h, hash, &bytes, &fmt))
            .map_err(|e| e.context(format!("block {h} ({hash})")));
        shared.put(h, r);
    }
}

fn deliver(shared: &Shared, to: u32, out: Sender<Result<ParsedBlock>>) {
    loop {
        let mut st = shared.state.lock().unwrap();
        let item = loop {
            if shared.halted(&st) {
                drop(st);
                return shared.finish();
            }
            let next = st.next;
            if let Some(r) = st.ready.remove(&next) {
                break r;
            }
            st = shared.cv.wait_timeout(st, TICK).unwrap().0;
        };
        drop(st);
        let failed = item.is_err();
        let mut item = item;
        loop {
            if shared.stop.load(Ordering::SeqCst) {
                return shared.finish();
            }
            match out.send_timeout(item, TICK) {
                Ok(()) => break,
                Err(SendTimeoutError::Timeout(i)) => item = i,
                Err(SendTimeoutError::Disconnected(_)) => return shared.finish(),
            }
        }
        let mut st = shared.state.lock().unwrap();
        st.next += 1;
        if failed || st.next > to {
            st.done = true;
        }
        drop(st);
        shared.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::mem::MemSource;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    fn run(
        src: Arc<MemSource>,
        from: u32,
        to: u32,
        workers: usize,
    ) -> (Receiver<Result<ParsedBlock>>, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let rx = fetch_range(
            src,
            HeaderFormat::BTC,
            from,
            to,
            &FetchConfig { workers },
            stop.clone(),
        );
        (rx, stop)
    }

    #[test]
    fn delivers_every_block_in_height_order_despite_random_worker_delays() {
        let src = Arc::new(MemSource::new(299));
        // Deterministic pseudo-random 0–3 ms per height.
        src.set_delay(|h| Duration::from_micros((h as u64 * 2_654_435_761) % 4000));
        let (rx, _stop) = run(src.clone(), 0, 299, 8);
        let heights: Vec<u32> = rx.iter().map(|b| b.unwrap().height).collect();
        assert_eq!(heights, (0..300).collect::<Vec<_>>());
        let b = {
            let (rx, _s) = run(src.clone(), 5, 5, 2);
            rx.recv().unwrap().unwrap()
        };
        assert_eq!((b.height, b.hash, b.prev), (5, src.hash(5), src.hash(4)));
        assert_eq!(b.txs.len(), 1);
    }

    #[test]
    fn an_error_ends_the_stream_after_the_good_prefix() {
        let src = Arc::new(MemSource::new(199));
        *src.fail_at.lock().unwrap() = Some(50);
        let (rx, _stop) = run(src, 0, 199, 8);
        let got: Vec<Result<ParsedBlock>> = rx.iter().collect();
        assert_eq!(got.len(), 51);
        for (i, b) in got[..50].iter().enumerate() {
            assert_eq!(b.as_ref().unwrap().height, i as u32);
        }
        assert!(got[50].is_err());
    }

    #[test]
    fn an_unknown_height_is_an_error_not_a_hang() {
        let src = Arc::new(MemSource::new(9));
        let (rx, _stop) = run(src, 5, 20, 4);
        let got: Vec<Result<ParsedBlock>> = rx.iter().collect();
        assert!(got.last().unwrap().is_err());
        assert!(got.iter().take(got.len() - 1).all(|b| b.is_ok()));
    }

    #[test]
    fn stop_flag_closes_the_stream_promptly() {
        let src = Arc::new(MemSource::new(299));
        src.set_delay(|_| Duration::from_millis(1));
        let (rx, stop) = run(src, 0, 299, 4);
        for _ in 0..10 {
            rx.recv().unwrap().unwrap();
        }
        stop.store(true, Ordering::SeqCst);
        let t = Instant::now();
        let mut n = 10;
        while rx.recv_timeout(Duration::from_secs(1)).is_ok() {
            n += 1;
        }
        assert!(
            t.elapsed() < Duration::from_millis(200),
            "took {:?}",
            t.elapsed()
        );
        assert!(n < 300, "delivered {n}");
    }

    #[test]
    fn never_buffers_more_than_the_channel_bound() {
        let workers = 4;
        let src = Arc::new(MemSource::new(299));
        let (rx, _stop) = run(src.clone(), 0, 299, workers);
        let mut received = 0usize;
        let mut worst = 0usize;
        while let Ok(b) = rx.recv_timeout(Duration::from_secs(5)) {
            b.unwrap();
            received += 1;
            std::thread::sleep(Duration::from_millis(2));
            worst = worst.max(src.started.load(Ordering::SeqCst) - received);
        }
        assert_eq!(received, 300);
        assert!(
            worst <= workers + 4 * workers + workers,
            "{worst} blocks outstanding"
        );
    }
}
