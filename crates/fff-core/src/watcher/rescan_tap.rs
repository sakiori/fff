use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use notify::{
    Config, Event, EventHandler, PathOp, RecursiveMode, WatchPathConfig, Watcher, WatcherKind,
};
use notify_debouncer_full::DebouncedEvent;
use parking_lot::Mutex;

// Past this many paths the recrawl requests stand for the whole root.
const MAX_LOGGED: usize = 1024;

// Every recrawl request a watcher reported, once per path. notify-debouncer-full keeps only the
// latest pending one, so a second subtree's request in the same window would replace the first.
#[derive(Clone, Default)]
pub(crate) struct RescanLog(Arc<Mutex<Vec<Event>>>);

// Clears the log handed to the next watcher of this thread on every way out of building it.
pub(crate) struct Installed;

impl Drop for Installed {
    fn drop(&mut self) {
        NEXT_LOG.with(|next| next.borrow_mut().take());
    }
}

impl RescanLog {
    // Hands the log to the next `Tapped` watcher built on this thread, as the debouncer builds
    // its watcher itself, until the returned guard is dropped.
    pub(crate) fn install_for_next_watcher(&self) -> Installed {
        NEXT_LOG.with(|next| *next.borrow_mut() = Some(self.clone()));
        Installed
    }

    // Replaces the batch's recrawl requests with every one reported since the last batch.
    pub(crate) fn merge_into(&self, events: &mut Vec<DebouncedEvent>) {
        if !events.iter().any(|event| event.need_rescan()) {
            return;
        }
        let logged = std::mem::take(&mut *self.0.lock());
        if logged.is_empty() {
            return;
        }
        let time = (events.iter())
            .find(|event| event.need_rescan())
            .map(|event| event.time)
            .unwrap_or_else(std::time::Instant::now);
        events.retain(|event| !event.need_rescan());
        events.extend(
            logged
                .into_iter()
                .map(|event| DebouncedEvent::new(event, time)),
        );
    }

    // A request without a path, as an overflowed queue sends, covers every other one.
    fn record(&self, event: &Event) {
        let mut log = self.0.lock();
        if log.iter().any(|logged| logged.paths.is_empty()) {
            return;
        }
        if event.paths.is_empty() || log.len() >= MAX_LOGGED {
            let mut root = event.clone();
            root.paths.clear();
            *log = vec![root];
            return;
        }
        if !log.iter().any(|logged| logged.paths == event.paths) {
            log.push(event.clone());
        }
    }
}

thread_local! {
    static NEXT_LOG: RefCell<Option<RescanLog>> = const { RefCell::new(None) };
}

// A watcher whose recrawl requests are also logged before the debouncer coalesces them.
pub(crate) struct Tapped<W>(W);

struct TapHandler<F> {
    handler: F,
    log: Option<RescanLog>,
    // Test seam: deliveries of a backend that is no polling one are recorded.
    #[cfg(all(test, target_os = "macos"))]
    native: bool,
}

impl<F: EventHandler> EventHandler for TapHandler<F> {
    fn handle_event(&mut self, event: notify::Result<Event>) {
        #[cfg(test)]
        let mut event = event;
        #[cfg(test)]
        if let Ok(event) = &mut event
            && !super::background_watcher::deliverable(event)
        {
            return;
        }
        #[cfg(all(test, target_os = "macos"))]
        if let (Ok(event), true) = (&event, self.native) {
            super::background_watcher::record_delivery(event);
        }
        if let (Ok(event), Some(log)) = (&event, &self.log)
            && event.need_rescan()
        {
            log.record(event);
        }
        self.handler.handle_event(event);
    }
}

impl<W: Watcher> Watcher for Tapped<W> {
    fn new<F: EventHandler>(event_handler: F, config: Config) -> notify::Result<Self> {
        let log = NEXT_LOG.with(|next| next.borrow_mut().take());
        let handler = TapHandler {
            handler: event_handler,
            log,
            #[cfg(all(test, target_os = "macos"))]
            native: W::kind() != WatcherKind::PollWatcher,
        };
        W::new(handler, config).map(Self)
    }

    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> notify::Result<()> {
        self.0.watch(path, recursive_mode)
    }

    fn watch_with(&mut self, path: &Path, config: WatchPathConfig) -> notify::Result<()> {
        self.0.watch_with(path, config)
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.0.unwatch(path)
    }

    fn update_paths(&mut self, ops: Vec<PathOp>) -> Result<(), notify::UpdatePathsError> {
        self.0.update_paths(ops)
    }

    fn configure(&mut self, option: Config) -> notify::Result<bool> {
        self.0.configure(option)
    }

    fn watched_paths(&self) -> notify::Result<Vec<(PathBuf, RecursiveMode)>> {
        self.0.watched_paths()
    }

    fn kind() -> WatcherKind {
        W::kind()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::EventKind;
    use notify::event::{CreateKind, Flag};
    use std::time::Instant;

    fn lost(path: &str) -> Event {
        Event::new(EventKind::Other)
            .set_flag(Flag::Rescan)
            .add_path(PathBuf::from(path))
    }

    #[test]
    fn the_log_keeps_each_path_once_and_a_root_request_covers_the_rest() {
        let log = RescanLog::default();
        log.record(&lost("/w/a"));
        log.record(&lost("/w/a"));
        log.record(&lost("/w/b"));
        assert_eq!(log.0.lock().len(), 2);
        for i in 0..MAX_LOGGED {
            log.record(&lost(&format!("/w/d{i}")));
        }
        let logged = log.0.lock().clone();
        assert_eq!(logged.len(), 1);
        assert!(logged[0].paths.is_empty() && logged[0].need_rescan());
        log.record(&lost("/w/c"));
        assert_eq!(log.0.lock().len(), 1);
    }

    #[test]
    fn building_a_watcher_never_leaves_the_log_installed() {
        let log = RescanLog::default();
        drop(log.install_for_next_watcher());
        assert!(NEXT_LOG.with(|next| next.borrow().is_none()));
    }

    #[test]
    fn every_recrawl_request_survives_the_debouncer_keeping_one() {
        let log = RescanLog::default();
        let forwarded = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&forwarded);
        let mut handler = TapHandler {
            handler: move |event: notify::Result<Event>| seen.lock().push(event.unwrap()),
            log: Some(log.clone()),
            #[cfg(target_os = "macos")]
            native: false,
        };
        handler.handle_event(Ok(lost("/w/a")));
        handler.handle_event(Ok(Event::new(EventKind::Create(CreateKind::File))));
        handler.handle_event(Ok(lost("/w/b")));
        assert_eq!(forwarded.lock().len(), 3);

        // The debouncer hands over only the latest request.
        let mut batch = vec![DebouncedEvent::new(lost("/w/b"), Instant::now())];
        log.merge_into(&mut batch);
        let paths: Vec<_> = batch.iter().map(|event| event.paths.clone()).collect();
        assert_eq!(paths, [[PathBuf::from("/w/a")], [PathBuf::from("/w/b")]]);

        // A batch without a request leaves the log for the batch that carries it.
        handler.handle_event(Ok(lost("/w/c")));
        let mut plain = vec![DebouncedEvent::new(
            Event::new(EventKind::Create(CreateKind::File)),
            Instant::now(),
        )];
        log.merge_into(&mut plain);
        assert_eq!(plain.len(), 1);
        let mut batch = vec![DebouncedEvent::new(lost("/w/c"), Instant::now())];
        log.merge_into(&mut batch);
        assert_eq!(batch.len(), 1);
    }
}
