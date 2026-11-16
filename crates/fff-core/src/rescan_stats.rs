#[cfg(rescan_stats)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Whether rescan accounting is compiled in.
pub const RESCAN_STATS_ENABLED: bool = cfg!(rescan_stats);

/// Cause recorded for a filesystem rescan request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RescanReason {
    /// Requested through the public API (refresh, directory change).
    Explicit,
    /// The kernel dropped events and asked us to re-read the subtree. No longer requested:
    /// the subtree the lost events name is reconciled instead.
    KernelEventLoss,
    /// A `.gitignore`/`.ignore` changed, so the cached ignore rules are stale. No longer
    /// requested: the rules of the file's directory are read again and its subtree reconciled.
    IgnoreFileChanged,
    /// A single debounce batch touched more paths than we apply incrementally. No longer
    /// requested: such a batch is reconciled where it changed things.
    EventBatchOverflow,
    /// The picker refused an incremental insert/update, a turn of the index's loop panicked, or
    /// its watcher was lost: the index is rebuilt.
    IndexUpdateRejected,
    /// The post-scan overflow region ran out of slots.
    OverflowCapacity,
}

impl RescanReason {
    pub const ALL: [RescanReason; 6] = [
        RescanReason::Explicit,
        RescanReason::KernelEventLoss,
        RescanReason::IgnoreFileChanged,
        RescanReason::EventBatchOverflow,
        RescanReason::IndexUpdateRejected,
        RescanReason::OverflowCapacity,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            RescanReason::Explicit => "explicit",
            RescanReason::KernelEventLoss => "kernel_event_loss",
            RescanReason::IgnoreFileChanged => "ignore_file_changed",
            RescanReason::EventBatchOverflow => "event_batch_overflow",
            RescanReason::IndexUpdateRejected => "index_update_rejected",
            RescanReason::OverflowCapacity => "overflow_capacity",
        }
    }

    const fn slot(self) -> usize {
        match self {
            RescanReason::Explicit => 0,
            RescanReason::KernelEventLoss => 1,
            RescanReason::IgnoreFileChanged => 2,
            RescanReason::EventBatchOverflow => 3,
            RescanReason::IndexUpdateRejected => 4,
            RescanReason::OverflowCapacity => 5,
        }
    }
}

impl std::fmt::Display for RescanReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// Every reason that asked for one full rescan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RescanReasons(u8);

impl RescanReasons {
    pub(crate) fn insert(&mut self, reason: RescanReason) {
        self.0 |= 1 << reason.slot();
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = RescanReason> {
        (RescanReason::ALL.into_iter()).filter(move |reason| self.0 & (1 << reason.slot()) != 0)
    }

    // Whether the watcher asked, not only a program.
    pub(crate) fn include_the_watcher_s(self) -> bool {
        self.iter().any(|reason| reason != RescanReason::Explicit)
    }
}

impl From<RescanReason> for RescanReasons {
    fn from(reason: RescanReason) -> Self {
        let mut reasons = Self::default();
        reasons.insert(reason);
        reasons
    }
}

impl std::fmt::Display for RescanReasons {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (position, reason) in self.iter().enumerate() {
            if position > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{reason}")?;
        }
        Ok(())
    }
}

/// Snapshot of full rescans grouped by reason: each rescan that started counts once in `total`,
/// and under every reason that asked for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RescanStats {
    pub total: usize,
    /// Requests that did not start a rescan of their own because they joined the one already
    /// pending.
    pub throttled: usize,
    counts: [usize; RescanReason::ALL.len()],
    throttled_counts: [usize; RescanReason::ALL.len()],
    watcher: usize,
}

impl RescanStats {
    /// Rescans `reason` asked for, alone or with other reasons.
    pub fn count(&self, reason: RescanReason) -> usize {
        self.counts[reason.slot()]
    }

    pub fn count_throttled(&self, reason: RescanReason) -> usize {
        self.throttled_counts[reason.slot()]
    }

    /// Rescans the watcher asked for, also those a request through the public API joined.
    pub fn watcher_triggered(&self) -> usize {
        self.watcher
    }

    /// Per-reason delta against an earlier snapshot.
    pub fn since(&self, earlier: &RescanStats) -> RescanStats {
        let mut counts = [0usize; RescanReason::ALL.len()];
        let mut throttled_counts = [0usize; RescanReason::ALL.len()];
        for slot in 0..RescanReason::ALL.len() {
            counts[slot] = self.counts[slot].saturating_sub(earlier.counts[slot]);
            throttled_counts[slot] =
                self.throttled_counts[slot].saturating_sub(earlier.throttled_counts[slot]);
        }

        RescanStats {
            total: self.total.saturating_sub(earlier.total),
            throttled: self.throttled.saturating_sub(earlier.throttled),
            counts,
            throttled_counts,
            watcher: self.watcher.saturating_sub(earlier.watcher),
        }
    }
}

impl std::fmt::Display for RescanStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} rescan(s)", self.total)?;
        let mut first = true;
        for reason in RescanReason::ALL {
            let count = self.count(reason);
            if count == 0 {
                continue;
            }
            f.write_str(if first { " [" } else { ", " })?;
            write!(f, "{reason}={count}")?;
            first = false;
        }
        if !first {
            f.write_str("]")?;
        }
        if self.throttled > 0 {
            write!(f, ", {} throttled", self.throttled)?;
        }
        Ok(())
    }
}

#[cfg(rescan_stats)]
#[derive(Default)]
pub(crate) struct RescanCounters {
    total: AtomicUsize,
    watcher: AtomicUsize,
    counters: [AtomicUsize; RescanReason::ALL.len()],
    throttled: [AtomicUsize; RescanReason::ALL.len()],
}

#[cfg(rescan_stats)]
impl RescanCounters {
    pub(crate) fn record(&self, reasons: RescanReasons) {
        self.total.fetch_add(1, Ordering::Relaxed);
        if reasons.include_the_watcher_s() {
            self.watcher.fetch_add(1, Ordering::Relaxed);
        }
        for reason in reasons.iter() {
            self.counters[reason.slot()].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_throttled(&self, reason: RescanReason) {
        self.throttled[reason.slot()].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> RescanStats {
        let mut stats = RescanStats {
            total: self.total.load(Ordering::Relaxed),
            watcher: self.watcher.load(Ordering::Relaxed),
            ..RescanStats::default()
        };
        for reason in RescanReason::ALL {
            stats.counts[reason.slot()] = self.counters[reason.slot()].load(Ordering::Relaxed);

            let throttled = self.throttled[reason.slot()].load(Ordering::Relaxed);
            stats.throttled_counts[reason.slot()] = throttled;
            stats.throttled += throttled;
        }
        stats
    }

    pub(crate) fn reset(&self) {
        let totals = [&self.total, &self.watcher];
        for counter in (self.counters.iter().chain(&self.throttled)).chain(totals) {
            counter.store(0, Ordering::Relaxed);
        }
    }
}

// Release builds retain the API without counter storage.
#[cfg(not(rescan_stats))]
#[derive(Default)]
pub(crate) struct RescanCounters;

#[cfg(not(rescan_stats))]
impl RescanCounters {
    pub(crate) fn record(&self, _reasons: RescanReasons) {}

    pub(crate) fn record_throttled(&self, _reason: RescanReason) {}

    pub(crate) fn snapshot(&self) -> RescanStats {
        RescanStats::default()
    }

    pub(crate) fn reset(&self) {}
}

#[cfg(all(test, rescan_stats))]
mod tests {
    use super::*;

    #[test]
    fn counters_attribute_and_diff_per_reason() {
        let counters = RescanCounters::default();
        counters.record(RescanReason::Explicit.into());
        let baseline = counters.snapshot();

        counters.record(RescanReason::IgnoreFileChanged.into());
        counters.record(RescanReason::IgnoreFileChanged.into());
        let mut joined = RescanReasons::from(RescanReason::Explicit);
        joined.insert(RescanReason::OverflowCapacity);
        counters.record(joined);

        let stats = counters.snapshot();
        assert_eq!(stats.total, 4);
        assert_eq!(stats.watcher_triggered(), 3);
        assert_eq!(stats.count(RescanReason::Explicit), 2);

        let delta = stats.since(&baseline);
        assert_eq!(delta.total, 3);
        assert_eq!(delta.watcher_triggered(), 3);
        assert_eq!(delta.count(RescanReason::Explicit), 1);
        assert_eq!(delta.count(RescanReason::IgnoreFileChanged), 2);
        assert_eq!(
            delta.to_string(),
            "3 rescan(s) [explicit=1, ignore_file_changed=2, overflow_capacity=1]"
        );
        assert_eq!(joined.to_string(), "explicit, overflow_capacity");

        counters.reset();
        assert_eq!(counters.snapshot(), RescanStats::default());
    }
}
