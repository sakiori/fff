use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::constants::{
    LARGE_INDEX_FILE_COUNT, RESCAN_MIN_INTERVAL, RESCAN_MIN_INTERVAL_LARGE_INDEX,
};
use crate::rescan_stats::RescanReason;

/// Cooldown between watcher rescans for an index of this size.
pub(crate) fn rescan_cooldown(live_files: usize, has_git_repo: bool) -> Duration {
    if !has_git_repo && live_files >= LARGE_INDEX_FILE_COUNT {
        RESCAN_MIN_INTERVAL_LARGE_INDEX
    } else {
        RESCAN_MIN_INTERVAL
    }
}

/// A throttled rescan that still has to run once the cooldown allows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DeferredRescan {
    pub(crate) reason: RescanReason,
    pub(crate) due: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    Admitted,
    /// Throttled, and the index cannot recover without the rescan.
    Deferred(DeferredRescan),
    /// Throttled; the next admitted request covers it.
    Dropped,
}

// Rate-limits watcher rescans. Requests inside the cooldown are dropped unless
// their reason leaves the index knowingly incomplete: those stay deferred until due.
#[derive(Default)]
pub(crate) struct RescanThrottle {
    state: Mutex<ThrottleState>,
}

#[derive(Default)]
struct ThrottleState {
    last_admitted: Option<Instant>,
    deferred: Option<DeferredRescan>,
}

impl RescanThrottle {
    /// Admission records the rescan and settles any deferred one.
    pub(crate) fn admit(&self, reason: RescanReason, cooldown: Duration) -> Admission {
        self.admit_at(Instant::now(), reason, cooldown)
    }

    /// Like [`Self::admit`], but a throttled request always stays deferred: the
    /// caller dropped changes that only the rescan recovers.
    pub(crate) fn admit_deferring(&self, reason: RescanReason, cooldown: Duration) -> Admission {
        self.admit_with(Instant::now(), reason, true, cooldown)
    }

    fn admit_at(&self, now: Instant, reason: RescanReason, cooldown: Duration) -> Admission {
        self.admit_with(now, reason, reason.defers_when_throttled(), cooldown)
    }

    fn admit_with(
        &self,
        now: Instant,
        reason: RescanReason,
        defers: bool,
        cooldown: Duration,
    ) -> Admission {
        let mut state = self.state.lock();
        if let Some(last) = state.last_admitted
            && now.saturating_duration_since(last) < cooldown
        {
            if !defers {
                return Admission::Dropped;
            }
            // Later requesters join the pending rescan; the cooldown may have grown.
            let reason = state.deferred.map_or(reason, |deferred| deferred.reason);
            let deferred = DeferredRescan {
                reason,
                due: last + cooldown,
            };
            state.deferred = Some(deferred);
            return Admission::Deferred(deferred);
        }
        state.last_admitted = Some(now);
        state.deferred = None;
        Admission::Admitted
    }

    /// Records an explicit (unthrottled) scan: the index is fresh, so watcher
    /// requests right after it are throttled and nothing stays deferred.
    pub(crate) fn note_explicit_scan(&self) {
        let mut state = self.state.lock();
        state.last_admitted = Some(Instant::now());
        state.deferred = None;
    }

    pub(crate) fn deferred(&self) -> Option<DeferredRescan> {
        self.state.lock().deferred
    }

    /// Drops a deferred rescan whose index is gone or cannot be rescanned.
    pub(crate) fn cancel_deferred(&self) {
        self.state.lock().deferred = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COOLDOWN: Duration = Duration::from_secs(30);

    fn admitted_at(now: Instant) -> RescanThrottle {
        let throttle = RescanThrottle::default();
        let reason = RescanReason::KernelEventLoss;
        assert_eq!(
            throttle.admit_at(now, reason, COOLDOWN),
            Admission::Admitted
        );
        throttle
    }

    #[test]
    fn first_request_is_always_admitted() {
        let throttle = RescanThrottle::default();
        assert_eq!(
            throttle.admit(RescanReason::KernelEventLoss, COOLDOWN),
            Admission::Admitted
        );
    }

    #[test]
    fn requests_inside_the_cooldown_are_dropped() {
        let start = Instant::now();
        let throttle = admitted_at(start);
        let later = start + Duration::from_secs(1);
        for reason in [
            RescanReason::KernelEventLoss,
            RescanReason::IgnoreFileChanged,
        ] {
            assert_eq!(
                throttle.admit_at(later, reason, COOLDOWN),
                Admission::Dropped
            );
        }
        assert_eq!(throttle.deferred(), None);
    }

    #[test]
    fn a_large_index_outside_a_git_repo_uses_the_slower_cadence() {
        assert_eq!(rescan_cooldown(100, false), RESCAN_MIN_INTERVAL);
        assert_eq!(
            rescan_cooldown(LARGE_INDEX_FILE_COUNT, true),
            RESCAN_MIN_INTERVAL
        );
        assert_eq!(
            rescan_cooldown(LARGE_INDEX_FILE_COUNT, false),
            RESCAN_MIN_INTERVAL_LARGE_INDEX
        );
    }

    #[test]
    fn cooldown_expiry_admits_again() {
        let start = Instant::now();
        let throttle = admitted_at(start);
        let reason = RescanReason::KernelEventLoss;
        let expired = start + COOLDOWN;
        assert_eq!(
            throttle.admit_at(expired, reason, COOLDOWN),
            Admission::Admitted
        );
        // Admission rearms the cooldown.
        let soon = expired + Duration::from_secs(1);
        assert_eq!(
            throttle.admit_at(soon, reason, COOLDOWN),
            Admission::Dropped
        );
    }

    #[test]
    fn explicit_scan_rearms_the_cooldown() {
        let throttle = RescanThrottle::default();
        throttle.note_explicit_scan();
        assert_eq!(
            throttle.admit(RescanReason::KernelEventLoss, COOLDOWN),
            Admission::Dropped
        );
    }

    #[test]
    fn rejected_updates_stay_deferred_until_the_cooldown_ends() {
        let start = Instant::now();
        let throttle = admitted_at(start);
        let expected = DeferredRescan {
            reason: RescanReason::IndexUpdateRejected,
            due: start + COOLDOWN,
        };
        let later = start + Duration::from_secs(1);
        assert_eq!(
            throttle.admit_at(later, RescanReason::IndexUpdateRejected, COOLDOWN),
            Admission::Deferred(expected)
        );
        // Later requesters join the pending rescan instead of moving it.
        let overflow = RescanReason::OverflowCapacity;
        let even_later = start + Duration::from_secs(2);
        assert_eq!(
            throttle.admit_at(even_later, overflow, COOLDOWN),
            Admission::Deferred(expected)
        );
        assert_eq!(throttle.deferred(), Some(expected));
    }

    #[test]
    fn a_deferring_request_is_never_dropped() {
        let start = Instant::now();
        let throttle = admitted_at(start);
        let reason = RescanReason::EventBatchOverflow;
        let later = start + Duration::from_secs(1);
        assert_eq!(
            throttle.admit_at(later, reason, COOLDOWN),
            Admission::Dropped
        );
        let expected = DeferredRescan {
            reason,
            due: start + COOLDOWN,
        };
        assert_eq!(
            throttle.admit_with(later, reason, true, COOLDOWN),
            Admission::Deferred(expected)
        );
        assert_eq!(throttle.deferred(), Some(expected));
    }

    #[test]
    fn any_admitted_rescan_settles_the_deferred_one() {
        let start = Instant::now();
        let throttle = admitted_at(start);
        let later = start + Duration::from_secs(1);
        let reason = RescanReason::IndexUpdateRejected;
        assert!(matches!(
            throttle.admit_at(later, reason, COOLDOWN),
            Admission::Deferred(_)
        ));
        let other = RescanReason::KernelEventLoss;
        assert_eq!(
            throttle.admit_at(start + COOLDOWN, other, COOLDOWN),
            Admission::Admitted
        );
        assert_eq!(throttle.deferred(), None);

        assert!(matches!(
            throttle.admit_at(start + COOLDOWN, reason, COOLDOWN),
            Admission::Deferred(_)
        ));
        throttle.note_explicit_scan();
        assert_eq!(throttle.deferred(), None);
    }
}
