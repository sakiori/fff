use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::Instant;

use tracing::{error, info, warn};

use super::changed_paths::ChangedPaths;
use super::intake::intake;
use super::reconcile::reconcile;
use super::repository::LoopRepository;
use super::watched::{Running, Watched};
use super::writer::{Message, Replacement};
use crate::FileSync;
use crate::error::Error;
use crate::rescan_schedule::{Pending, RescanSchedule};
use crate::rescan_stats::RescanReason;
use crate::scan::{PostScan, RequestTicket, ScanConfig, ScanJob, ScanSignals};
use crate::scm::{ScmBackend, ScmWorkspace};
use crate::shared::{SharedFilePicker, SharedFrecency};
use crate::types::ContentCacheBudget;
use crate::watch::{BackgroundWatcher, Installed, Lost};

// What an index's loop alone keeps of it, as Watchman's IO thread of its root: the scan, the
// changes recorded while it walks, and the rescan schedule.
pub(super) struct IndexLoop {
    index: Option<IndexState>,
    scans: u64,
}

struct IndexState {
    key: u64,
    signals: ScanSignals,
    base_path: PathBuf,
    scan: Scan,
    schedule: RescanSchedule,
    repository: LoopRepository,
    // What the loop knows of the watcher: the one source of its published status.
    watcher: Watched,
}

enum Scan {
    Idle,
    // What changes is recorded until the walk's snapshot is published.
    Walking(Box<Walking>),
    // The walk's snapshot is being published.
    Publishing,
    // The snapshot is published; its content index is built.
    Indexing { id: u64 },
}

struct Walking {
    id: u64,
    initial: bool,
    // A scan that installs a watcher waits for it to report in before it walks.
    awaiting_watcher: bool,
    recorded: ChangedPaths,
    git: Option<ScmWorkspace>,
    config: ScanConfig,
    frecency: SharedFrecency,
    // The scan serves a recovery: one whose walk fails is owed again, as a repair if it was one.
    recovers: bool,
    repairs: bool,
}

impl IndexLoop {
    pub(super) fn new() -> Self {
        Self {
            index: None,
            scans: 0,
        }
    }

    // When the pending full rescan is due, if the index could start it now.
    pub(super) fn due(&mut self, shared: &SharedFilePicker) -> Option<Instant> {
        let (interval, stopped) = rescan_interval(shared)?;
        let state = self.adopt(shared)?;
        if stopped {
            state.forgo_replacement();
        }
        if !matches!(state.scan, Scan::Idle) || state.cancelled() {
            return None;
        }
        state.schedule.due(interval, Instant::now())
    }

    // After a turn panicked: the index may be half written, and only a rebuild restores it. A scan
    // that panicked ends with it.
    pub(super) fn recover(&mut self, shared: &SharedFilePicker) {
        let Some(state) = self.index.as_mut() else {
            return;
        };
        // A scan still waiting for its watcher has read nothing yet: it already is the rebuild,
        // and walks once the watcher reports in.
        if matches!(&state.scan, Scan::Walking(walking) if walking.awaiting_watcher) {
            self.publish_status(shared);
            return;
        }
        // A watcher installed by the scan that panicked reports from now on: it tells listeners
        // at once, not only after the rebuild.
        if state.watcher == (Watched::Installed { reporting: false })
            && let Ok(guard) = shared.read()
            && let Some(watcher) = guard.as_ref().and_then(|p| p.background_watcher.as_ref())
        {
            watcher.announce();
        }
        let scanned = !matches!(state.scan, Scan::Idle);
        state.scan = Scan::Idle;
        state.signals.scanning.store(false, Ordering::Release);
        #[cfg(test)]
        if scanned {
            crate::scan::tests::reached(crate::scan::tests::Stage::Finished, &state.base_path);
        }
        #[cfg(not(test))]
        let _ = scanned;
        self.rebuild(shared, true);
    }

    // The one recovery for a panicked turn, a lost watcher or a failed rebuild walk, with a
    // supervisor's restart limit: at once unless one started within the interval, else after it.
    fn rebuild(&mut self, shared: &SharedFilePicker, repairs: bool) {
        let Some((interval, _)) = rescan_interval(shared) else {
            return;
        };
        let Some(state) = self.index.as_mut() else {
            return;
        };
        if state.cancelled() {
            return;
        }
        state.owe_rebuild(shared, interval, repairs);
        self.start_due(shared);
        self.publish_status(shared);
    }

    pub(super) fn handle(&mut self, shared: &SharedFilePicker, message: Message) {
        match message {
            Message::Replace(replacement) => self.replace(shared, *replacement),
            Message::Remove { removed } => {
                let index = shared.write().ok().and_then(|mut guard| guard.take());
                self.index = None;
                // Dropped off the lock: dropping an index stops its watcher and cancels its work.
                drop(index);
                let _ = removed.send(());
            }
            Message::Events {
                index,
                events,
                frecency,
                applied,
            } => self.take_events(shared, index, events, frecency, applied),
            Message::Rescan {
                index,
                reason,
                immediate,
                ticket,
                started,
            } => {
                let started_now = self.request(shared, index, reason, immediate, ticket);
                if let Some(started) = started {
                    let _ = started.send(started_now);
                }
            }
            Message::Watching {
                index,
                scan,
                watcher,
            } => self.watching(shared, index, scan, watcher),
            Message::WatcherLost { index } => self.watcher_lost(shared, index),
            Message::Walked {
                index,
                scan,
                walked,
            } => self.publish(shared, index, scan, *walked),
            Message::ContentIndexed {
                index,
                scan,
                bigram,
            } => {
                let indexing = self.current(shared, index).map(|state| &state.scan);
                if matches!(indexing, Some(Scan::Indexing { id }) if *id == scan)
                    && let Ok(mut guard) = shared.write()
                    && let Some(picker) = guard.as_mut()
                    && !picker.signals.cancelled.load(Ordering::Acquire)
                {
                    picker.set_bigram_index(*bigram);
                }
            }
            Message::PostScanned { index, scan } => {
                let indexing = self.current(shared, index).map(|state| &state.scan);
                if matches!(indexing, Some(Scan::Indexing { id }) if *id == scan) {
                    self.end_scan(shared);
                }
            }
            Message::Cancel { index } => {
                if let Some(state) = self.current(shared, index) {
                    state.schedule = RescanSchedule::default();
                }
            }
            #[cfg(debug_assertions)]
            Message::EndInterval { index, done } => {
                if let Some(state) = self.current(shared, index) {
                    state.schedule.end_interval(Instant::now());
                }
                self.start_due(shared);
                let _ = done.send(());
            }
            #[cfg(test)]
            Message::Due(reply) => {
                let _ = reply.send(self.due(shared));
            }
            #[cfg(test)]
            Message::Work { index, work } => {
                if self.current(shared, index).is_some() {
                    guarded(shared, |picker| work(picker));
                }
            }
            #[cfg(test)]
            Message::Wake => {}
        }
    }

    // Readiness and watcher status from the loop's state, stored under the picker's lock for the
    // lock-free public status; a rescan due now starts this turn, an explicit one by its ticket.
    pub(super) fn publish_status(&mut self, shared: &SharedFilePicker) {
        let Some(state) = self.index.as_mut() else {
            return;
        };
        let pending = state.running() != Running::Nothing;
        state.signals.scan_pending.store(pending, Ordering::SeqCst);
        if let Ok(guard) = shared.read()
            && let Some(picker) = guard.as_ref()
        {
            state.publish_watcher(picker);
        }
    }

    // Starts the pending full rescan once it is due and no scan runs.
    pub(super) fn start_due(&mut self, shared: &SharedFilePicker) {
        let Some((interval, stopped)) = rescan_interval(shared) else {
            return;
        };
        let Some(state) = self.index.as_mut() else {
            return;
        };
        if stopped {
            state.forgo_replacement();
        }
        if !matches!(state.scan, Scan::Idle) || state.cancelled() {
            return;
        }
        if let Some(pending) = state.schedule.take_due(interval, Instant::now()) {
            self.start_rescan(shared, pending);
        }
    }

    // The published index, whose state is new when it replaced the one known.
    fn adopt(&mut self, shared: &SharedFilePicker) -> Option<&mut IndexState> {
        let current = (shared.read().ok()).and_then(|guard| {
            let picker = guard.as_ref()?;
            Some((picker.scan_signals(), picker.base_path().to_path_buf()))
        });
        let Some((signals, base_path)) = current else {
            self.index = None;
            return None;
        };
        let key = signals.index_key();
        if self.index.as_ref().is_none_or(|state| state.key != key) {
            self.index = Some(IndexState {
                key,
                signals,
                base_path,
                scan: Scan::Idle,
                schedule: RescanSchedule::default(),
                repository: LoopRepository::default(),
                watcher: Watched::Missing { owed: false },
            });
        }
        self.index.as_mut()
    }

    // The state of the index a message was made for, while it is the published one.
    fn current(&mut self, shared: &SharedFilePicker, index: u64) -> Option<&mut IndexState> {
        self.adopt(shared).filter(|state| state.key == index)
    }

    // Only the loop replaces an index, so every write it makes after this goes to the new one.
    fn replace(&mut self, shared: &SharedFilePicker, replacement: Replacement) {
        let Replacement {
            picker,
            frecency,
            config,
            replaced,
        } = replacement;
        let signals = picker.scan_signals();
        let base_path = picker.base_path().to_path_buf();
        // Published as scanning, so consumers that take its status right after never see it idle
        // before its walk (e.g. lua `wait_for_initial_scan` after `restart_index_in_path`).
        signals.scanning.store(true, Ordering::Release);
        signals.scan_pending.store(true, Ordering::SeqCst);
        let old = shared
            .write()
            .ok()
            .and_then(|mut guard| guard.replace(picker));
        // Dropped off the lock: dropping the old index stops its watcher and cancels its work.
        drop(old);
        self.index = Some(IndexState {
            key: signals.index_key(),
            signals,
            base_path,
            scan: Scan::Idle,
            schedule: RescanSchedule::default(),
            repository: LoopRepository::default(),
            watcher: Watched::Missing { owed: false },
        });
        let _ = replaced.send(());
        self.begin_walk(shared, config, frecency, true, Pending::default());
    }

    fn take_events(
        &mut self,
        shared: &SharedFilePicker,
        index: u64,
        events: Vec<notify_debouncer_full::DebouncedEvent>,
        frecency: Option<SharedFrecency>,
        applied: Option<Sender<bool>>,
    ) {
        let Some(state) = self.current(shared, index) else {
            return;
        };
        if state.cancelled() {
            return;
        }
        #[cfg(test)]
        super::writer::tests::before_job(shared);
        let base_path = state.base_path.clone();
        let IndexState {
            scan, repository, ..
        } = state;
        let applied_now = match scan {
            // A scan holds changes: they join its set, filtered against the index as it is.
            Scan::Walking(walking) => {
                let repo = repository.at(walking.git.as_ref());
                walking.recorded.add(intake(shared, &events, repo));
                false
            }
            _ => {
                let (frecency, workspace) = (shared.read().ok())
                    .and_then(|guard| {
                        let picker = guard.as_ref()?;
                        let workspace = picker.scm_workspace().cloned();
                        Some((
                            frecency.unwrap_or_else(|| picker.frecency.clone()),
                            workspace,
                        ))
                    })
                    .unzip();
                let repo = repository.at(workspace.flatten().as_ref());
                let rescan = frecency.and_then(|frecency| {
                    let mut changed = ChangedPaths::new(&base_path);
                    changed.add(intake(shared, &events, repo));
                    guarded(shared, |picker| {
                        reconcile(picker, &frecency, changed, true, repo)
                    })
                });
                if let Some(reason) = rescan {
                    self.request(shared, index, reason, false, None);
                }
                true
            }
        };
        if let Some(applied) = applied {
            let _ = applied.send(applied_now);
        }
    }

    // A request joins the pending full rescan; returns whether a rescan started now.
    fn request(
        &mut self,
        shared: &SharedFilePicker,
        index: u64,
        reason: RescanReason,
        immediate: bool,
        ticket: Option<RequestTicket>,
    ) -> bool {
        let Some(state) = self.current(shared, index) else {
            return false;
        };
        if state.cancelled() {
            return false;
        }
        let joined = state.schedule.request(reason, immediate, ticket);
        let idle = matches!(state.scan, Scan::Idle);
        if joined {
            shared.rescans().record_throttled(reason);
        }
        tracing::debug!(%reason, immediate, joined, "Full rescan pending");
        let before = self.scans;
        if idle {
            self.start_due(shared);
        }
        self.scans != before
    }

    // Every full rescan that starts is logged and counted once, as Watchman reports its recrawls.
    fn start_rescan(&mut self, shared: &SharedFilePicker, pending: Pending) {
        let Some((mut config, frecency)) = (shared.read().ok()).and_then(|guard| {
            let picker = guard.as_ref()?;
            Some((ScanConfig::of_rescan(picker), picker.frecency.clone()))
        }) else {
            return;
        };
        if let Some(state) = self.index.as_ref() {
            let owed = state.watcher == Watched::Missing { owed: true };
            config.install_watcher = owed && config.watch;
        }
        let reasons = pending.reasons();
        warn!(%reasons, "Starting a full rescan");
        shared.rescans().record(reasons);
        // The requests it serves stop counting once it runs.
        let tickets = (self.index.as_mut()).map(|state| state.schedule.take_tickets());
        self.begin_walk(shared, config, frecency, false, pending);
        drop(tickets);
    }

    // Records from here, after every earlier batch, and has the pool walk once the watcher a first
    // scan or a rebuild needs is installed off the loop: nothing changed after it goes unseen.
    fn begin_walk(
        &mut self,
        shared: &SharedFilePicker,
        config: ScanConfig,
        frecency: SharedFrecency,
        initial: bool,
        serves: Pending,
    ) {
        let Some(state) = self.index.as_mut() else {
            return;
        };
        self.scans += 1;
        let id = self.scans;
        let base_path = state.base_path.clone();
        state.signals.scanning.store(true, Ordering::Release);
        #[cfg(test)]
        if !initial {
            crate::scan::tests::reached(crate::scan::tests::Stage::Claimed, &base_path);
        }
        let git = FileSync::discover_scm(
            scm_backend(shared),
            &base_path,
            config.ignore_options.disable_git,
        );
        let watch = config.install_watcher && config.watch;
        let options = watch.then(|| config.clone());
        state.scan = Scan::Walking(Box::new(Walking {
            id,
            initial,
            awaiting_watcher: false,
            recorded: ChangedPaths::new(&base_path),
            git: git.clone(),
            config,
            frecency,
            recovers: serves.recovers(),
            repairs: serves.repairs(),
        }));
        #[cfg(test)]
        if initial {
            crate::scan::tests::reached(crate::scan::tests::Stage::Installing, &base_path);
        }
        #[cfg(test)]
        let refused = watch && crate::scan::tests::refuses_watcher(&base_path);
        #[cfg(not(test))]
        let refused = false;
        let cancelled = state.cancelled();
        // A rebuild's new watcher stays quiet until its rebuild announces every repository.
        let started = (options.filter(|_| !refused && !cancelled))
            .is_some_and(|config| start_watcher(shared, state.key, id, git, &config, !initial));
        // A cancelled index starts none and reads stopped.
        if watch {
            state.watcher = match started || cancelled {
                true => Watched::Installing,
                false => Watched::Missing { owed: false },
            };
        }
        match &mut state.scan {
            Scan::Walking(walking) if started => walking.awaiting_watcher = true,
            _ => self.walk(shared),
        }
    }

    // A scan's new watcher reported: installed unless its index went or was cancelled meanwhile,
    // then the scan's walk starts, unless a recovery replaced the scan.
    fn watching(
        &mut self,
        shared: &SharedFilePicker,
        index: u64,
        scan: u64,
        watcher: Result<Box<BackgroundWatcher>, Error>,
    ) {
        let Some(state) = self.current(shared, index) else {
            return;
        };
        let waiting = matches!(&state.scan, Scan::Walking(walking) if walking.id == scan);
        let installing = state.watcher == Watched::Installing;
        match watcher {
            Ok(watcher) => {
                let mut watcher = Some(watcher);
                if installing
                    && let Ok(mut guard) = shared.write()
                    && let Some(picker) = guard.as_mut()
                    && !picker.signals.cancelled.load(Ordering::Acquire)
                {
                    // One whose install was in flight when the watcher was stopped is dropped.
                    if picker.watcher_stopped() {
                        state.watcher = Watched::Missing { owed: false };
                        state.publish_watcher(picker);
                    } else if picker.background_watcher.is_none() {
                        picker.background_watcher = watcher.take().map(|watcher| *watcher);
                        state.watcher = Watched::Installed { reporting: false };
                        state.publish_watcher(picker);
                        // A ref listener may have subscribed before the watcher was in place.
                        if let Some(installed) = &picker.background_watcher {
                            installed.send(super::background_watcher::WatchTask::FollowRefs(None));
                        }
                    }
                }
                // One not installed stops, off the lock.
                drop(watcher);
            }
            Err(e) => {
                error!(?e, "failed to initialize background watcher");
                if installing {
                    state.watcher = Watched::Missing { owed: false };
                }
            }
        }
        if waiting {
            self.walk(shared);
        }
    }

    // What the lost watcher watched is unknown: it leaves, and the full rescan that installs a
    // new one is the index's recovery.
    fn watcher_lost(&mut self, shared: &SharedFilePicker, index: u64) {
        let Some(state) = self.current(shared, index) else {
            return;
        };
        if state.cancelled() {
            return;
        }
        // The slot empties, the rebuild is owed and the status says so in one critical section.
        let lost = (shared.write().ok()).and_then(|mut guard| {
            let picker = guard.as_mut()?;
            let lost = picker.background_watcher.take();
            state.watcher = Watched::Missing { owed: true };
            state.owe_rebuild(shared, picker.rescan_cooldown(), false);
            #[cfg(test)]
            if crate::scan::tests::forgets_rebuild(&state.base_path) {
                state.schedule = RescanSchedule::default();
            }
            state.publish_watcher(picker);
            lost
        });
        drop(lost);
        self.start_due(shared);
    }

    // Has the pool walk for the scan that records.
    fn walk(&mut self, shared: &SharedFilePicker) {
        let Some(state) = self.index.as_mut() else {
            return;
        };
        let cancelled = state.cancelled();
        let Scan::Walking(walking) = &mut state.scan else {
            return;
        };
        walking.awaiting_watcher = false;
        let Some((mode, counter, span)) = (shared.read().ok()).and_then(|guard| {
            let picker = guard.as_ref()?;
            let counter = picker.scanned_files_counter();
            Some((picker.mode(), counter, picker.trace_span()))
        }) else {
            return;
        };
        // Per-directory backends subscribe each directory before the walker reads it.
        let dir_hook = (shared.read().ok()).and_then(|guard| {
            let watcher = (guard.as_ref())
                .filter(|_| !cancelled)
                .and_then(|picker| picker.background_watcher.as_ref())?;
            if walking.config.install_watcher {
                watcher.walk_begins();
            }
            BackgroundWatcher::dir_hook(watcher)
        });
        ScanJob {
            id: walking.id,
            shared_picker: shared.clone(),
            shared_frecency: walking.frecency.clone(),
            base_path: state.base_path.clone(),
            mode,
            index: state.key,
            config: walking.config.clone(),
            git: walking.git.clone(),
            dir_hook,
            scanned_files_counter: counter,
            trace_span: span,
        }
        .spawn();
    }

    // Commits the walk's snapshot, then checks again what changed while it walked, on top of it.
    fn publish(
        &mut self,
        shared: &SharedFilePicker,
        index: u64,
        scan: u64,
        walked: Result<FileSync, Error>,
    ) {
        let Some(state) = self.current(shared, index) else {
            return;
        };
        let walking = match std::mem::replace(&mut state.scan, Scan::Publishing) {
            Scan::Walking(walking) if walking.id == scan => walking,
            other => {
                state.scan = other;
                return;
            }
        };
        let Walking {
            initial,
            recorded,
            git,
            config,
            frecency,
            recovers,
            repairs,
            ..
        } = *walking;
        let signals = state.signals.clone();
        let base_path = state.base_path.clone();
        let cancelled = signals.cancelled.load(Ordering::Acquire);
        let sync = match walked {
            Ok(sync) if !cancelled => sync,
            Ok(_) => {
                info!("scan cancelled between walk and commit, discarding");
                signals.scanning.store(false, Ordering::Relaxed);
                return self.end_scan(shared);
            }
            Err(_) => {
                if initial && !cancelled {
                    // An index whose initial walk failed cannot follow the tree.
                    let stopped = (shared.write().ok()).and_then(|mut guard| {
                        let picker = guard.as_mut()?;
                        let stopped = picker.background_watcher.take();
                        state.watcher = Watched::Missing { owed: false };
                        state.publish_watcher(picker);
                        stopped
                    });
                    drop(stopped);
                } else if !cancelled {
                    // The live index takes what was recorded, and the watcher reports.
                    let workspace = scm_workspace(shared);
                    let repo = state.repository.at(workspace.as_ref());
                    let rescan = reconcile(shared, &frecency, recorded, true, repo);
                    if let Some(reason) = rescan
                        && state.schedule.request(reason, false, None)
                    {
                        shared.rescans().record_throttled(reason);
                    }
                    self.reported(shared, initial);
                    if recovers {
                        self.rebuild(shared, repairs);
                    }
                }
                self.publish_status(shared);
                signals.scanning.store(false, Ordering::Relaxed);
                return self.end_scan(shared);
            }
        };

        let Some((previous, scm_status_worker)) = shared.write().ok().and_then(|mut guard| {
            let picker = guard.as_mut()?;
            let live_count = sync.live_count;
            let previous = picker.commit_new_sync(sync);
            if config.auto_cache_budget && !picker.has_explicit_cache_budget() {
                picker.set_cache_budget(ContentCacheBudget::new_for_repo(live_count));
            }
            Some((previous, Arc::clone(&picker.scm_status_worker)))
        }) else {
            return self.end_scan(shared);
        };
        #[cfg(test)]
        crate::scan::tests::committed(&base_path);

        if git.is_some() {
            scm_status_worker.on_workspace_scan(shared.weaken(), frecency.clone());
        }

        #[cfg(test)]
        {
            crate::scan::tests::recorded(&base_path, recorded.entry_paths());
            crate::scan::tests::reached(crate::scan::tests::Stage::Rechecking, &base_path);
        }
        // The recorded changes reach the new snapshot without events of their own: subscribers
        // hear of the whole rebuild at once below.
        let workspace = scm_workspace(shared);
        let rescan = (self.index.as_mut()).and_then(|state| {
            let repo = state.repository.at(workspace.as_ref());
            reconcile(shared, &frecency, recorded, false, repo)
        });
        if let Some(state) = self.index.as_mut()
            && let Some(reason) = rescan
            && state.schedule.request(reason, false, None)
        {
            shared.rescans().record_throttled(reason);
        }
        // Every full scan but an index's first tells subscribers that it replaced the index, once
        // its state is in place, as Watchman's and Buck2's fresh instance.
        if !initial {
            shared.watch_registry().dispatch_rescan(&base_path);
        }
        // Every change since the walk began is now indexed or queued behind the live watcher. A
        // rescan that recovers from a first publication that failed halfway leaves it ready too.
        self.reported(shared, initial);
        if let Some(state) = self.index.as_mut() {
            state.scan = Scan::Indexing { id: scan };
        }
        self.publish_status(shared);

        // BUG pinning: take the snapshot *before* the storing the scan=true, otherwise there is a tiny
        // race window when there scanned is set to true, but `post_scan_indexing_active` flag is `false`
        let snapshot = (shared.read().ok()).and_then(|guard| {
            guard
                .as_ref()
                .and_then(|picker| unsafe { picker.post_scan_snapshot() })
        });
        signals.scanning.store(false, Ordering::Relaxed); // file are searchable
        // The post scan skips files the previous index already read as text and that did not change
        let known_text = (shared.read().ok())
            .and_then(|guard| Some(guard.as_ref()?.unchanged_text_files(&previous)))
            .unwrap_or_default();
        drop(previous);

        let trace_span = (shared.read().ok())
            .and_then(|guard| Some(guard.as_ref()?.trace_span()))
            .unwrap_or_else(tracing::Span::current);
        PostScan {
            shared_picker: shared.clone(),
            index,
            scan,
            signals,
            config,
            snapshot,
            known_text,
            trace_span,
        }
        .spawn();
    }

    // A full scan is published with the watcher in place. A rebuild's quiet new watcher now
    // announces every repository it follows, after the rescan event, as a fresh Watchman instance.
    fn reported(&mut self, shared: &SharedFilePicker, initial: bool) {
        let Some(state) = self.index.as_mut() else {
            return;
        };
        if state.watcher != (Watched::Installed { reporting: false }) {
            return;
        }
        let installed = (shared.read().ok()).is_some_and(|guard| {
            let watcher = guard.as_ref().and_then(|p| p.background_watcher.as_ref());
            (watcher.filter(|_| !initial)).map(BackgroundWatcher::announce);
            watcher.is_some()
        });
        if installed {
            state.watcher = Watched::Installed { reporting: true };
        }
    }

    // The scan ended, post-scan included: the interval before a watcher's rescan counts from now.
    fn end_scan(&mut self, shared: &SharedFilePicker) {
        let Some(state) = self.index.as_mut() else {
            return;
        };
        state.scan = Scan::Idle;
        if !state.cancelled() {
            info!(base_path = %state.base_path.display(), "Full scan ended");
            state.schedule.scan_ended(Instant::now());
        }
        #[cfg(test)]
        crate::scan::tests::reached(crate::scan::tests::Stage::Finished, &state.base_path);
        self.start_due(shared);
    }
}

impl IndexState {
    fn cancelled(&self) -> bool {
        self.signals.cancelled.load(Ordering::Acquire)
    }

    fn owe_rebuild(
        &mut self,
        shared: &SharedFilePicker,
        interval: std::time::Duration,
        repairs: bool,
    ) {
        let now = Instant::now();
        let (joined, due) = self.schedule.recover(now, interval, repairs);
        if joined {
            shared
                .rescans()
                .record_throttled(RescanReason::IndexUpdateRejected);
        }
        if due > now {
            warn!("Rebuilding the index one rescan interval after the last rebuild started");
        }
    }

    fn running(&self) -> Running {
        match &self.scan {
            Scan::Idle => Running::Nothing,
            Scan::Walking(walking) if walking.initial => Running::Initial,
            _ => Running::Rescan,
        }
    }

    // A stopped watcher is replaced no more: neither a replacement owed nor its rebuild stays.
    fn forgo_replacement(&mut self) {
        if self.watcher == (Watched::Missing { owed: true }) {
            self.watcher = Watched::Missing { owed: false };
        }
        self.schedule.forgo_replacement();
    }

    // Called with the picker's lock held, also where the loop changes the watcher's slot: a stop
    // recorded under that lock is learnt here, ordered with the status it publishes.
    fn publish_watcher(&self, picker: &crate::FilePicker) {
        if picker.scan_signals().index_key() != self.key {
            return;
        }
        let stopped = picker.watcher_stopped();
        // A replacement is owed only while its rebuild is pending: a breach is logged and counted.
        let owed = self.watcher == Watched::Missing { owed: true };
        if owed && !self.cancelled() && !self.schedule.recovers() {
            error!("A replacement watcher is owed with no rebuild pending");
            #[cfg(test)]
            crate::scan::tests::owed_alone(&self.base_path);
        }
        let status = (self.watcher).published(picker.has_watcher(), stopped, self.running());
        #[cfg(test)]
        crate::scan::tests::publishing(&self.base_path);
        self.signals.publish_watcher(status);
    }
}

// Starts a scan's new watcher on its own thread, which tells the loop once it is installed, or
// lost; false once the watcher was stopped, so no streams start only to be dropped.
fn start_watcher(
    shared: &SharedFilePicker,
    index: u64,
    scan: u64,
    git: Option<ScmWorkspace>,
    config: &ScanConfig,
    quiet: bool,
) -> bool {
    let Some((base_path, options, git_status, span)) = (shared.read().ok()).and_then(|guard| {
        let picker = guard.as_ref().filter(|picker| !picker.watcher_stopped())?;
        Some((
            picker.base_path().to_path_buf(),
            config.watcher_options(picker.mode()),
            Arc::clone(&picker.scm_status_worker),
            picker.trace_span(),
        ))
    }) else {
        return false;
    };
    let loop_handle = shared.weaken();
    let lost_handle = shared.weaken();
    let lost: Lost = Box::new(move || {
        if let Some(shared) = lost_handle.upgrade() {
            shared.send(Message::WatcherLost { index });
        }
    });
    let installed: Installed = Box::new(move |watcher| {
        if let Some(shared) = loop_handle.upgrade() {
            let watcher = watcher.map(Box::new);
            shared.send(Message::Watching {
                index,
                scan,
                watcher,
            });
        }
    });
    BackgroundWatcher::start(
        base_path, git, shared, options, git_status, span, quiet, installed, lost,
    )
}

// The rescan interval, and whether the watcher was stopped.
fn rescan_interval(shared: &SharedFilePicker) -> Option<(std::time::Duration, bool)> {
    let guard = shared.read().ok()?;
    let picker = guard.as_ref()?;
    Some((picker.rescan_cooldown(), picker.watcher_stopped()))
}

// The published index's repository.
fn scm_workspace(shared: &SharedFilePicker) -> Option<ScmWorkspace> {
    let guard = shared.read().ok()?;
    guard.as_ref()?.scm_workspace().cloned()
}

// The published index's backend, or the process's before an index is published.
fn scm_backend(shared: &SharedFilePicker) -> ScmBackend {
    (shared.read().ok())
        .and_then(|guard| Some(guard.as_ref()?.scm_backend))
        .unwrap_or_else(ScmBackend::configured)
}

// Applies changes; a panic reaches the loop's turn, which has the index rebuilt.
fn guarded<R>(shared: &SharedFilePicker, work: impl FnOnce(&SharedFilePicker) -> R) -> R {
    #[cfg(test)]
    super::writer::tests::entered(shared);
    #[cfg(test)]
    let left = scopeguard_left(shared);
    let result = work(shared);
    #[cfg(test)]
    drop(left);
    result
}

// Counts a batch as left also when it panicked.
#[cfg(test)]
fn scopeguard_left(shared: &SharedFilePicker) -> impl Drop + '_ {
    struct Left<'a>(&'a SharedFilePicker);
    impl Drop for Left<'_> {
        fn drop(&mut self) {
            super::writer::tests::left(self.0);
        }
    }
    Left(shared)
}
