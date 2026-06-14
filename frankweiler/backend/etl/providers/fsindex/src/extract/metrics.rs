//! Atomic counters + phase timings for fsindex.
//!
//! USE-method-ish: utilization, saturation, errors. Single-process
//! single-threaded walker today, so true U/S/U is moot — what we
//! track is throughput (entries/sec, bytes/sec hashed), the
//! producer→consumer channel high-water-mark (the only real
//! saturation signal here), and per-phase wall time. Atomic
//! increments are submicrosecond so the cost is negligible vs the
//! syscalls per entry.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[derive(Debug, Default)]
pub struct WalkerCounters {
    pub dirs_visited: AtomicU64,
    pub files_visited: AtomicU64,
    pub symlinks_visited: AtomicU64,
    pub files_reused: AtomicU64,
    pub files_rehashed: AtomicU64,
    pub bytes_hashed: AtomicU64,
    pub bytes_skipped_cache: AtomicU64,
    pub ignored_entries: AtomicU64,
    pub stat_errors: AtomicU64,
    pub read_errors: AtomicU64,
    pub non_utf8_paths: AtomicU64,
    pub batches_emitted: AtomicU64,
    pub rows_emitted: AtomicU64,
}

impl WalkerCounters {
    pub fn entries_total(&self) -> u64 {
        self.dirs_visited.load(Ordering::Relaxed)
            + self.files_visited.load(Ordering::Relaxed)
            + self.symlinks_visited.load(Ordering::Relaxed)
    }
}

/// Tracks the maximum observed depth of the producer→consumer
/// channel. The only direct saturation signal we have today: if the
/// HWM equals the channel capacity often, the writer is the
/// bottleneck; if HWM stays near zero, the walker is.
#[derive(Debug, Default)]
pub struct ChannelHwm {
    inner: AtomicUsize,
}

impl ChannelHwm {
    pub fn observe(&self, depth: usize) {
        let mut cur = self.inner.load(Ordering::Relaxed);
        while depth > cur {
            match self
                .inner
                .compare_exchange_weak(cur, depth, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    pub fn load(&self) -> usize {
        self.inner.load(Ordering::Relaxed)
    }
}
