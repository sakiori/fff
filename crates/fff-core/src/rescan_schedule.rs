use std::time::{Duration, Instant};

use crate::constants::{
    LARGE_INDEX_FILE_COUNT, RESCAN_MIN_INTERVAL, RESCAN_MIN_INTERVAL_LARGE_INDEX,
};
use crate::rescan_stats::{RescanReason, RescanReasons};
use crate::scan::RequestTicket;

/// Least time from the end of a full scan to a full rescan the watcher requests.
pub(crate) fn rescan_interval(live_files: usize, has_git_repo: bool) -> Duration {
    if !has_git_repo && live_files >= LARGE_INDEX_FILE_COUNT {
        RESCAN_MIN_INTERVAL_LARGE_INDEX
    } else {
        RESCAN_MIN_INTERVAL
    }
}

// The one full rescan queued behind the running scan, kept by the index's loop; requests join it.
// It waits an interval after the last one, unless explicit or a recovery first in its interval.
#[derive(Default)]
pub(crate) struct RescanSchedule {
    pending: Option<Pending>,
    // The explicit requests the pending rescan serves, counted until it starts.
    tickets: Vec<RequestTicket>,
    last_scan_end: Option<Instant>,
    // When the last full rescan that served a recovery started.
    recovered: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pending {
    // What was requested; a recovery adds its own reason.
    requested: RescanReasons,
    // Starts right after the running scan.
    immediate: bool,
    // Waits the interval from the end of the last full scan.
    waits: bool,
    // When the recovery it serves is due, fixed once the recovery was first requested.
    recovery: Option<Instant>,
    // The recovery rebuilds an index that may be half written, not only a lost watcher's.
    repairs: bool,
}

impl RescanSchedule {
    // Joins the pending rescan; one that may not wait makes it start at once. Returns whether
    // one was pending already.
    pub(crate) fn request(
        &mut self,
        reason: RescanReason,
        immediate: bool,
        ticket: Option<RequestTicket>,
    ) -> bool {
        self.tickets.extend(ticket);
        let joined = self.pending.is_some();
        let pending = self.pending.get_or_insert_default();
        pending.requested.insert(reason);
        pending.immediate |= immediate;
        pending.waits |= !immediate;
        joined
    }

    // Joins the pending rescan with a recovery: due at once, or an interval after the last
    // recovery started. Returns whether one was pending already, and when the recovery is due.
    pub(crate) fn recover(
        &mut self,
        now: Instant,
        interval: Duration,
        repairs: bool,
    ) -> (bool, Instant) {
        let due = (self.recovered).map_or(now, |started| (started + interval).max(now));
        let joined = self.pending.is_some();
        let pending = self.pending.get_or_insert_default();
        let due = pending.recovery.map_or(due, |known| known.min(due));
        pending.recovery = Some(due);
        pending.repairs |= repairs;
        (joined, due)
    }

    // The watcher was stopped: a recovery that only replaced a lost one is dropped, and what
    // else was requested keeps its own time.
    pub(crate) fn forgo_replacement(&mut self) {
        let Some(pending) = self.pending.as_mut().filter(|pending| !pending.repairs) else {
            return;
        };
        pending.recovery = None;
        if !pending.immediate && !pending.waits {
            self.pending = None;
        }
    }

    // When the pending rescan may start, if one is pending.
    pub(crate) fn due(&self, interval: Duration, now: Instant) -> Option<Instant> {
        let pending = self.pending?;
        let after_last_scan = self.last_scan_end.map_or(now, |end| end + interval);
        let immediate = pending.immediate.then_some(now);
        let waits = pending.waits.then_some(after_last_scan);
        [immediate, waits, pending.recovery]
            .into_iter()
            .flatten()
            .min()
    }

    // Takes the pending rescan once it is due. One that serves a recovery is its start, recorded
    // before the start is attempted.
    pub(crate) fn take_due(&mut self, interval: Duration, now: Instant) -> Option<Pending> {
        if self.due(interval, now)? > now {
            return None;
        }
        let pending = self.pending.take()?;
        if pending.recovers() {
            self.recovered = Some(now);
        }
        Some(pending)
    }

    // The tickets of the rescan that starts now.
    pub(crate) fn take_tickets(&mut self) -> Vec<RequestTicket> {
        std::mem::take(&mut self.tickets)
    }

    pub(crate) fn scan_ended(&mut self, at: Instant) {
        self.last_scan_end = Some(at);
    }

    // A recovery is pending, at once or after its interval.
    pub(crate) fn recovers(&self) -> bool {
        self.pending.as_ref().is_some_and(Pending::recovers)
    }

    // Test control: the interval since the last scan, and since the last recovery, has passed.
    #[cfg(debug_assertions)]
    pub(crate) fn end_interval(&mut self, now: Instant) {
        self.last_scan_end = None;
        self.recovered = None;
        if let Some(due) = self
            .pending
            .as_mut()
            .and_then(|pending| pending.recovery.as_mut())
        {
            *due = (*due).min(now);
        }
    }
}

impl Pending {
    pub(crate) fn recovers(&self) -> bool {
        self.recovery.is_some()
    }

    pub(crate) fn repairs(&self) -> bool {
        self.recovery.is_some() && self.repairs
    }

    pub(crate) fn reasons(&self) -> RescanReasons {
        let mut reasons = self.requested;
        if self.recovers() {
            reasons.insert(RescanReason::IndexUpdateRejected);
        }
        reasons
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const INTERVAL: Duration = Duration::from_secs(30);

    fn reasons(reasons: &[RescanReason]) -> RescanReasons {
        let mut set = RescanReasons::default();
        reasons.iter().for_each(|reason| set.insert(*reason));
        set
    }

    #[test]
    fn requests_coalesce_into_one_pending_rescan() {
        let mut schedule = RescanSchedule::default();
        let reason = RescanReason::OverflowCapacity;
        assert!(!schedule.request(reason, false, None));
        for _ in 0..4 {
            assert!(schedule.request(reason, false, None));
        }
        let start = Instant::now();
        let pending = schedule.take_due(INTERVAL, start).unwrap();
        assert_eq!(pending.reasons(), reason.into());
        assert!(!pending.recovers());
        assert_eq!(schedule.take_due(INTERVAL, start), None);
    }

    #[test]
    fn a_watcher_rescan_waits_the_interval_from_the_end_of_the_last_scan() {
        let mut schedule = RescanSchedule::default();
        let end = Instant::now();
        schedule.scan_ended(end);
        schedule.request(RescanReason::IndexUpdateRejected, false, None);
        assert_eq!(schedule.due(INTERVAL, end), Some(end + INTERVAL));
        assert!(schedule.due(INTERVAL, end) > Some(end));
        assert_eq!(schedule.take_due(INTERVAL, end + INTERVAL / 2), None);
        assert!(schedule.take_due(INTERVAL, end + INTERVAL).is_some());
    }

    #[test]
    fn an_explicit_request_waits_for_no_interval_and_keeps_every_reason() {
        let mut schedule = RescanSchedule::default();
        let end = Instant::now();
        schedule.scan_ended(end);
        schedule.request(RescanReason::OverflowCapacity, false, None);
        schedule.request(RescanReason::Explicit, true, None);
        assert_eq!(schedule.due(INTERVAL, end), Some(end));
        let pending = schedule.take_due(INTERVAL, end).unwrap();
        let both = reasons(&[RescanReason::Explicit, RescanReason::OverflowCapacity]);
        assert_eq!(pending.reasons(), both);
    }

    #[test]
    fn the_first_recovery_starts_at_once_and_the_next_an_interval_after_it() {
        let mut schedule = RescanSchedule::default();
        let start = Instant::now();
        schedule.scan_ended(start);
        assert_eq!(schedule.recover(start, INTERVAL, true), (false, start));
        assert_eq!(schedule.due(INTERVAL, start), Some(start));
        assert!(schedule.take_due(INTERVAL, start).unwrap().recovers());
        let again = start + INTERVAL / 3;
        assert_eq!(
            schedule.recover(again, INTERVAL, true),
            (false, start + INTERVAL)
        );
        assert_eq!(schedule.due(INTERVAL, again), Some(start + INTERVAL));
        assert_eq!(schedule.take_due(INTERVAL, start + INTERVAL / 2), None);
        assert!(schedule.take_due(INTERVAL, start + INTERVAL).is_some());
    }

    #[test]
    fn a_recovery_counts_from_the_start_of_the_last_one_that_waited() {
        let mut schedule = RescanSchedule::default();
        let start = Instant::now();
        schedule.recover(start, INTERVAL, true);
        schedule.take_due(INTERVAL, start);
        schedule.recover(start + INTERVAL / 2, INTERVAL, true);
        let deferred = start + INTERVAL;
        assert!(schedule.take_due(INTERVAL, deferred).is_some());
        // Lost just after the interval from the first: the second started since.
        let lost = deferred + INTERVAL / 10;
        assert_eq!(
            schedule.recover(lost, INTERVAL, true).1,
            deferred + INTERVAL
        );
        assert_eq!(schedule.due(INTERVAL, lost), Some(deferred + INTERVAL));
    }

    #[test]
    fn a_recovery_after_a_quiet_interval_starts_at_once() {
        let mut schedule = RescanSchedule::default();
        let start = Instant::now();
        schedule.recover(start, INTERVAL, true);
        schedule.take_due(INTERVAL, start);
        let quiet = start + INTERVAL * 2;
        assert_eq!(schedule.recover(quiet, INTERVAL, true).1, quiet);
        assert!(schedule.take_due(INTERVAL, quiet).is_some());
    }

    #[test]
    fn failures_before_a_waiting_recovery_starts_never_postpone_it() {
        let mut schedule = RescanSchedule::default();
        let start = Instant::now();
        schedule.recover(start, INTERVAL, true);
        schedule.take_due(INTERVAL, start);
        for tenth in 1..10 {
            let failed = start + INTERVAL * tenth / 10;
            assert_eq!(
                schedule.recover(failed, INTERVAL, true),
                (tenth > 1, start + INTERVAL)
            );
            schedule.scan_ended(failed);
        }
        assert_eq!(schedule.due(INTERVAL, start), Some(start + INTERVAL));
        assert!(schedule.take_due(INTERVAL, start + INTERVAL).is_some());
    }

    #[test]
    fn a_waiting_recovery_leaves_an_explicit_request_at_once_and_counted() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let mut schedule = RescanSchedule::default();
        let start = Instant::now();
        schedule.recover(start, INTERVAL, true);
        schedule.take_due(INTERVAL, start);
        let requested = start + INTERVAL / 4;
        let ticket = RequestTicket::new(&in_flight);
        schedule.request(RescanReason::Explicit, true, Some(ticket));
        schedule.recover(requested, INTERVAL, true);
        assert_eq!(schedule.due(INTERVAL, requested), Some(requested));
        assert_eq!(in_flight.load(Ordering::SeqCst), 1);
        let pending = schedule.take_due(INTERVAL, requested).unwrap();
        let both = reasons(&[RescanReason::Explicit, RescanReason::IndexUpdateRejected]);
        assert_eq!((pending.reasons(), pending.recovers()), (both, true));
        drop(schedule.take_tickets());
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
        // The explicit rescan served the recovery: the next one counts from its start.
        let lost = requested + INTERVAL / 4;
        assert_eq!(
            schedule.recover(lost, INTERVAL, true).1,
            requested + INTERVAL
        );
    }

    #[test]
    fn a_recovery_whose_start_failed_waits_the_interval_from_that_start() {
        let mut schedule = RescanSchedule::default();
        let start = Instant::now();
        schedule.recover(start, INTERVAL, true);
        assert!(schedule.take_due(INTERVAL, start).is_some());
        // Starting it panicked: the loop asks for the recovery again.
        assert_eq!(schedule.recover(start, INTERVAL, true).1, start + INTERVAL);
        assert_eq!(schedule.due(INTERVAL, start), Some(start + INTERVAL));
    }

    #[test]
    fn a_stopped_watcher_forgoes_a_recovery_that_only_replaced_it() {
        let mut schedule = RescanSchedule::default();
        let start = Instant::now();
        schedule.recover(start, INTERVAL, false);
        schedule.forgo_replacement();
        assert_eq!(schedule.due(INTERVAL, start), None);
        assert!(!schedule.recovers());
    }

    #[test]
    fn a_stopped_watcher_leaves_a_repair_and_every_request() {
        let mut schedule = RescanSchedule::default();
        let start = Instant::now();
        schedule.scan_ended(start);
        schedule.recover(start, INTERVAL, false);
        schedule.request(RescanReason::OverflowCapacity, false, None);
        schedule.forgo_replacement();
        // The request waits its interval, and no recovery's reason is left on it.
        assert_eq!(schedule.due(INTERVAL, start), Some(start + INTERVAL));
        let pending = schedule.take_due(INTERVAL, start + INTERVAL).unwrap();
        assert_eq!(pending.reasons(), RescanReason::OverflowCapacity.into());
        assert!(!pending.recovers());
        schedule.recover(start, INTERVAL, false);
        schedule.recover(start, INTERVAL, true);
        schedule.forgo_replacement();
        assert!(schedule.recovers());
    }
}
