use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;

use fff::{ListenerSubscription, SharedFilePicker, WatchId, WatchOptions};
use fff_service::program::{MAX_WAIT_READY, write_frame};
use fff_service::protocol::Watcher;
use fff_service::protocol::{Reply, ServiceError, send_message};
use fff_service::watch::{WatchEvent, WatchEventKind, WatchFrame, WatchRequest};
use interprocess::local_socket::tokio::{RecvHalf, SendHalf};
use parking_lot::Mutex;
use tokio::io::BufReader;
use tokio::sync::Notify;

use crate::program::closed;
use crate::registry::{Binding, Session, SessionError};

// Distinct paths a subscription holds for a program that reads slower than files change;
// beyond them its changes become one rescan.
const QUEUE_LIMIT: usize = 4096;
const FRAME_EVENTS: usize = 128;
// How often a subscription checks that its watcher runs and its directory exists.
const HEALTH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

// Serves one watch subscription until the program closes its connection.
pub(crate) async fn serve(
    session: Arc<Session>,
    request: WatchRequest,
    mut recv: BufReader<RecvHalf>,
    mut send: SendHalf,
) -> io::Result<()> {
    let deadline = tokio::time::Instant::now() + MAX_WAIT_READY;
    let prepared = tokio::select! {
        prepared = session.prepare_until(deadline) => prepared,
        () = closed(&mut recv) => return Ok(()),
    };
    if let Err(error) = prepared.map(drop) {
        let error = match error {
            SessionError::Service(error) => error,
            error => ServiceError::InvalidRequest {
                reason: error.to_string(),
            },
        };
        return send_message(&mut send, &Reply::Error { error }).await;
    }
    let queue = Arc::new(Queue::default());
    let mut rebindings = session.rebindings();
    rebindings.borrow_and_update();
    // The binding is not held beyond this, so the index is released when the session moves.
    let mut core = {
        let binding = session.snapshot();
        match Core::open(&binding, &session.scope, &request, &queue, 0).await {
            Ok(core) => core,
            Err(error) => return send_message(&mut send, &Reply::Error { error }).await,
        }
    };
    send_message(&mut send, &Reply::Ready { warnings: vec![] }).await?;
    let mut writer = tokio::spawn(write_events(queue.clone(), send));
    let mut health = tokio::time::interval(HEALTH_INTERVAL);
    let mut failed = false;
    let result = loop {
        tokio::select! {
            () = closed(&mut recv) => break Ok(()),
            written = &mut writer => break written.map_err(io::Error::other).and_then(|r| r),
            _ = health.tick(), if !failed => {
                if let Some(error) = failure(&session) {
                    failed = true;
                    queue.fail(error);
                }
            }
            _ = rebindings.changed(), if !failed => {
                let binding = session.snapshot();
                // What happened before the new index's subscription is lost, and the old
                // index's callbacks, also one running now, queue nothing after the rescan.
                let epoch = queue.move_on();
                match Core::open(&binding, &session.scope, &request, &queue, epoch).await {
                    Ok(moved) => core = moved,
                    Err(error) => {
                        failed = true;
                        queue.fail(error);
                    }
                }
            }
        }
    };
    drop(core);
    queue.close();
    writer.abort();
    result
}

// A failure is the last frame; the program then closes the connection.
async fn write_events(queue: Arc<Queue>, mut send: SendHalf) -> io::Result<()> {
    while let Some(next) = queue.next().await {
        match next {
            Ok(events) => {
                for frame in events.chunks(FRAME_EVENTS) {
                    write_frame(&mut send, &WatchFrame::Events(frame.to_vec())).await?;
                }
            }
            Err(error) => return write_frame(&mut send, &WatchFrame::Failed(error)).await,
        }
    }
    Ok(())
}

// Why the subscription cannot go on: its watcher stopped, or its directory is gone.
fn failure(session: &Session) -> Option<ServiceError> {
    let index = session.snapshot().index.clone();
    if matches!(index.watcher(), Watcher::Failed | Watcher::Stopped) {
        return Some(ServiceError::Index {
            root: index.root.clone(),
            reason: "the filesystem watcher stopped".into(),
        });
    }
    (!session.scope.is_dir()).then(|| ServiceError::Scope {
        path: session.scope.clone(),
        reason: "the directory was removed".into(),
    })
}

// A subscription on one index's watcher; dropping it unsubscribes.
struct Core {
    picker: SharedFilePicker,
    id: WatchId,
    repository: Option<ListenerSubscription>,
    refs: Option<ListenerSubscription>,
}

impl Core {
    // Subscribing takes the index's locks and changes its watches, so it runs off the runtime.
    async fn open(
        binding: &Arc<Binding>,
        scope: &Path,
        request: &WatchRequest,
        queue: &Arc<Queue>,
        epoch: u64,
    ) -> Result<Self, ServiceError> {
        let (binding, scope) = (binding.clone(), scope.to_path_buf());
        let (request, queue) = (request.clone(), queue.clone());
        let root = binding.index.root.clone();
        let subscribe = move || Self::subscribe(&binding, &scope, &request, &queue, epoch);
        tokio::task::spawn_blocking(subscribe)
            .await
            .unwrap_or_else(|error| {
                Err(ServiceError::Index {
                    root,
                    reason: error.to_string(),
                })
            })
    }

    // The core runs every subscriber's callback on one thread, so these only queue.
    fn subscribe(
        binding: &Binding,
        scope: &Path,
        request: &WatchRequest,
        queue: &Arc<Queue>,
        epoch: u64,
    ) -> Result<Self, ServiceError> {
        let picker = binding.index.picker.clone();
        let options = WatchOptions {
            ignore: request.ignore.clone(),
        };
        let events = {
            let queue = queue.clone();
            let scope = scope.to_path_buf();
            move |_: WatchId, events: &[fff::WatchEvent]| {
                let events = events.iter().filter_map(|event| wire_event(event, &scope));
                queue.push(epoch, events);
            }
        };
        let id = picker
            .watch_within(scope, &request.pattern, options, events)
            .map_err(|error| match error {
                fff::Error::InvalidGlobPattern { .. } => ServiceError::InvalidRequest {
                    reason: error.to_string(),
                },
                error => ServiceError::Index {
                    root: binding.index.root.clone(),
                    reason: error.to_string(),
                },
            })?;
        let repository = request.repository.then(|| {
            let forward = work_tree_event(queue, epoch, scope, WatchEventKind::Repository);
            picker.subscribe_repository_state(forward)
        });
        let refs = request.refs.then(|| {
            let forward = work_tree_event(queue, epoch, scope, WatchEventKind::Refs);
            picker.subscribe_ref_changes(scope, forward)
        });
        Ok(Self {
            picker,
            id,
            repository,
            refs,
        })
    }
}

// Unsubscribing changes the index's watches too, so it leaves the runtime's threads.
impl Drop for Core {
    fn drop(&mut self) {
        let (picker, id) = (self.picker.clone(), self.id);
        let listeners = (self.repository.take(), self.refs.take());
        let release = move || {
            picker.unwatch(id);
            drop(listeners);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(release)),
            Err(_) => release(),
        }
    }
}

// Queues the events of a work tree's git state or refs. The index reports every repository it
// watches; the subscription hears those inside its directory and the one containing it.
fn work_tree_event(
    queue: &Arc<Queue>,
    epoch: u64,
    scope: &Path,
    kind: WatchEventKind,
) -> impl Fn(&Path) + Send + Sync + 'static {
    let queue = queue.clone();
    let scope = scope.to_path_buf();
    move |root| {
        // Git spells a work tree with a trailing separator.
        let root: std::path::PathBuf = root.components().collect();
        let path = match root.strip_prefix(&scope) {
            Ok(inside) => wire_path(inside),
            Err(_) if scope.starts_with(&root) => wire_path(&root),
            Err(_) => return,
        };
        queue.push(
            epoch,
            [WatchEvent {
                path,
                kind,
                from: None,
            }],
        );
    }
}

// An event of the index as the subscription's directory sees it. A move across the
// directory's boundary is a creation or a removal there.
fn wire_event(event: &fff::WatchEvent, scope: &Path) -> Option<WatchEvent> {
    let inside = |path: &Path| path.strip_prefix(scope).ok().map(wire_path);
    let kind = match event.kind {
        fff::WatchEventKind::Rescan => return Some(WatchEvent::rescan()),
        fff::WatchEventKind::Renamed => {
            let from = event.from.as_deref().and_then(inside);
            return match (inside(&event.path), from) {
                (Some(path), Some(from)) => Some(WatchEvent {
                    path,
                    kind: WatchEventKind::Renamed,
                    from: Some(from),
                }),
                (Some(path), None) => Some(WatchEvent {
                    path,
                    kind: WatchEventKind::Created,
                    from: None,
                }),
                (None, Some(path)) => Some(WatchEvent {
                    path,
                    kind: WatchEventKind::Removed,
                    from: None,
                }),
                (None, None) => None,
            };
        }
        fff::WatchEventKind::Created => WatchEventKind::Created,
        fff::WatchEventKind::Modified => WatchEventKind::Modified,
        fff::WatchEventKind::Removed => WatchEventKind::Removed,
    };
    Some(WatchEvent {
        path: inside(&event.path)?,
        kind,
        from: None,
    })
}

fn wire_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

// Pending events of one subscription, one per path, or a rescan that replaces them.
#[derive(Default)]
struct Queue {
    state: Mutex<QueueState>,
    ready: Notify,
}

#[derive(Default)]
struct QueueState {
    // A replaced change leaves a gap, skipped when the queue drains.
    events: Vec<Option<WatchEvent>>,
    live: usize,
    // Repository, refs and file events never share a slot.
    positions: HashMap<(Slot, String), usize>,
    rescan: bool,
    failed: Option<ServiceError>,
    closed: bool,
    // Which subscription may queue: the one made since the last move.
    epoch: u64,
}

impl Queue {
    // Events of a subscription made before the queue moved on are refused.
    fn push(&self, epoch: u64, events: impl IntoIterator<Item = WatchEvent>) {
        let mut state = self.state.lock();
        if state.epoch != epoch {
            return;
        }
        let mut pushed = false;
        for event in events {
            // A pending rescan covers every change after it too.
            if state.closed || state.rescan {
                break;
            }
            pushed = true;
            if event.kind == WatchEventKind::Rescan {
                state.lose();
                break;
            }
            state.coalesce(event);
            if state.live > QUEUE_LIMIT {
                state.lose();
                break;
            }
        }
        drop(state);
        if pushed {
            self.ready.notify_one();
        }
    }

    // The subscription moves to another index: a rescan replaces what came before, and only
    // the subscription made with the returned epoch queues anything after it.
    fn move_on(&self) -> u64 {
        let mut state = self.state.lock();
        if !state.closed {
            state.lose();
        }
        state.epoch += 1;
        let epoch = state.epoch;
        drop(state);
        self.ready.notify_one();
        epoch
    }

    fn close(&self) {
        self.state.lock().closed = true;
        self.ready.notify_one();
    }

    // Ends the subscription after what is queued; later changes are dropped.
    fn fail(&self, error: ServiceError) {
        let mut state = self.state.lock();
        state.failed = Some(error);
        state.closed = true;
        drop(state);
        self.ready.notify_one();
    }

    // The next batch, the rescan first, then a failure; `None` once closed.
    async fn next(&self) -> Option<Result<Vec<WatchEvent>, ServiceError>> {
        loop {
            {
                let mut state = self.state.lock();
                if std::mem::take(&mut state.rescan) {
                    return Some(Ok(vec![WatchEvent::rescan()]));
                }
                if state.live > 0 {
                    state.positions.clear();
                    state.live = 0;
                    let events = std::mem::take(&mut state.events);
                    return Some(Ok(events.into_iter().flatten().collect()));
                }
                if let Some(error) = state.failed.take() {
                    return Some(Err(error));
                }
                if state.closed {
                    return None;
                }
            }
            self.ready.notified().await;
        }
    }
}

impl QueueState {
    fn lose(&mut self) {
        self.events.clear();
        self.live = 0;
        self.positions.clear();
        self.rescan = true;
    }

    // A later change of a queued path replaces it at the end of the queue, except that a
    // file created or moved in stays so when it is modified afterwards. A rename frees its
    // source's slot, so later changes of the source follow it, and a removal of its
    // destination stays a removal of the source in its place, followed by the destination's.
    fn coalesce(&mut self, event: WatchEvent) {
        let key = (Slot::of(event.kind), event.path.clone());
        if event.kind == WatchEventKind::Renamed
            && let Some(from) = &event.from
        {
            self.positions.remove(&(Slot::File, from.clone()));
        }
        let Some(&position) = self.positions.get(&key) else {
            self.append(key, event);
            return;
        };
        let Some(queued) = self.events[position].as_mut() else {
            return self.append(key, event);
        };
        match (queued.kind, event.kind) {
            (WatchEventKind::Created | WatchEventKind::Renamed, WatchEventKind::Modified) => {}
            (WatchEventKind::Renamed, WatchEventKind::Removed) => {
                queued.kind = WatchEventKind::Removed;
                queued.path = queued.from.take().unwrap_or_default();
                self.append(key, event);
            }
            _ => {
                self.events[position] = None;
                self.live -= 1;
                self.append(key, event);
            }
        }
    }

    fn append(&mut self, key: (Slot, String), event: WatchEvent) {
        self.positions.insert(key, self.events.len());
        self.events.push(Some(event));
        self.live += 1;
        // Gaps never outnumber the changes, so each costs O(1) over time.
        if self.events.len() > 2 * self.live + FRAME_EVENTS {
            self.compact();
        }
    }

    fn compact(&mut self) {
        let mut moved = HashMap::with_capacity(self.events.len());
        let mut kept = Vec::with_capacity(self.live);
        for (old, slot) in std::mem::take(&mut self.events).into_iter().enumerate() {
            if let Some(event) = slot {
                moved.insert(old, kept.len());
                kept.push(Some(event));
            }
        }
        for position in self.positions.values_mut() {
            *position = moved[position];
        }
        self.events = kept;
    }
}

// What a queued event coalesces with: a later event of the same path and slot.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Slot {
    File,
    Repository,
    Refs,
}

impl Slot {
    fn of(kind: WatchEventKind) -> Self {
        match kind {
            WatchEventKind::Repository => Slot::Repository,
            WatchEventKind::Refs => Slot::Refs,
            _ => Slot::File,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn event(path: &str, kind: WatchEventKind) -> WatchEvent {
        WatchEvent {
            path: path.into(),
            kind,
            from: None,
        }
    }

    #[test]
    fn a_move_to_another_index_leaves_nothing_of_the_old_one_after_its_rescan() {
        let queue = Queue::default();
        queue.push(0, [event("before", WatchEventKind::Modified)]);
        let epoch = queue.move_on();
        // The old index's callback, still running, and the new one's before the rescan went out.
        queue.push(0, [event("stale", WatchEventKind::Modified)]);
        queue.push(epoch, [event("early", WatchEventKind::Modified)]);
        assert_eq!(take(&queue), vec![WatchEvent::rescan()]);
        queue.push(0, [event("stale", WatchEventKind::Modified)]);
        queue.push(epoch, [event("after", WatchEventKind::Modified)]);
        assert_eq!(take(&queue), vec![event("after", WatchEventKind::Modified)]);
    }

    fn take(queue: &Queue) -> Vec<WatchEvent> {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(queue.next())
            .unwrap()
            .unwrap()
    }

    fn renamed(from: &str, to: &str) -> WatchEvent {
        WatchEvent {
            path: to.into(),
            kind: WatchEventKind::Renamed,
            from: Some(from.into()),
        }
    }

    #[test]
    fn renames_keep_the_order_of_changes_to_their_source() {
        // Saving as vim does: the file moves to a backup, a new file takes its name, and
        // the backup goes.
        let queue = Queue::default();
        queue.push(
            0,
            [
                renamed("a.txt", "a.txt~"),
                event("a.txt", WatchEventKind::Created),
                event("a.txt~", WatchEventKind::Removed),
            ],
        );
        assert_eq!(
            take(&queue),
            vec![
                event("a.txt", WatchEventKind::Removed),
                event("a.txt", WatchEventKind::Created),
                event("a.txt~", WatchEventKind::Removed),
            ]
        );
        queue.push(
            0,
            [
                event("b", WatchEventKind::Modified),
                renamed("b", "c"),
                event("b", WatchEventKind::Created),
                event("c", WatchEventKind::Modified),
            ],
        );
        assert_eq!(
            take(&queue),
            vec![
                event("b", WatchEventKind::Modified),
                renamed("b", "c"),
                event("b", WatchEventKind::Created),
            ]
        );
    }

    #[test]
    fn a_replaced_change_moves_to_the_end() {
        let queue = Queue::default();
        queue.push(
            0,
            [
                event("a", WatchEventKind::Modified),
                event("x", WatchEventKind::Modified),
                renamed("x", "a"),
            ],
        );
        assert_eq!(
            take(&queue),
            vec![event("x", WatchEventKind::Modified), renamed("x", "a")],
            "x is gone once it moved onto a"
        );
        queue.push(
            0,
            [
                event("a", WatchEventKind::Modified),
                event("b", WatchEventKind::Created),
                event("a", WatchEventKind::Removed),
                event("c", WatchEventKind::Modified),
            ],
        );
        assert_eq!(
            take(&queue),
            vec![
                event("b", WatchEventKind::Created),
                event("a", WatchEventKind::Removed),
                event("c", WatchEventKind::Modified),
            ]
        );
    }

    #[test]
    fn replacing_a_change_leaves_bounded_gaps() {
        let queue = Queue::default();
        for n in 0..10_000 {
            let kind = if n % 2 == 0 {
                WatchEventKind::Removed
            } else {
                WatchEventKind::Created
            };
            queue.push(
                0,
                [event("hot", kind), event(&format!("cold{}", n % 3), kind)],
            );
        }
        let slots = queue.state.lock().events.len();
        assert!(slots <= 2 * 4 + FRAME_EVENTS, "{slots} slots for 4 changes");
        assert_eq!(take(&queue).len(), 4);
    }

    #[test]
    fn a_failure_follows_what_is_queued_and_ends_the_queue() {
        let queue = Queue::default();
        queue.push(0, [event("a", WatchEventKind::Modified)]);
        queue.fail(ServiceError::ShuttingDown);
        queue.push(0, [event("b", WatchEventKind::Modified)]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert_eq!(
            runtime.block_on(queue.next()).unwrap().unwrap(),
            vec![event("a", WatchEventKind::Modified)]
        );
        assert_eq!(
            runtime.block_on(queue.next()).unwrap(),
            Err(ServiceError::ShuttingDown)
        );
        assert!(runtime.block_on(queue.next()).is_none());
    }

    #[test]
    fn changes_coalesce_by_path() {
        let queue = Queue::default();
        queue.push(
            0,
            [
                event("a", WatchEventKind::Created),
                event("b", WatchEventKind::Modified),
                event("a", WatchEventKind::Modified),
                event("b", WatchEventKind::Removed),
                event("", WatchEventKind::Repository),
                event("", WatchEventKind::Refs),
            ],
        );
        queue.push(
            0,
            [
                event("", WatchEventKind::Modified),
                event("", WatchEventKind::Refs),
                event("", WatchEventKind::Repository),
            ],
        );
        assert_eq!(
            take(&queue),
            vec![
                event("a", WatchEventKind::Created),
                event("b", WatchEventKind::Removed),
                event("", WatchEventKind::Modified),
                event("", WatchEventKind::Refs),
                event("", WatchEventKind::Repository),
            ]
        );
    }

    #[test]
    fn overflow_becomes_one_rescan_that_covers_later_changes() {
        let queue = Queue::default();
        queue.push(
            0,
            (0..=QUEUE_LIMIT).map(|n| event(&n.to_string(), WatchEventKind::Created)),
        );
        queue.push(0, [event("later", WatchEventKind::Modified)]);
        assert_eq!(take(&queue), vec![WatchEvent::rescan()]);
        queue.push(0, [event("after", WatchEventKind::Modified)]);
        assert_eq!(take(&queue), vec![event("after", WatchEventKind::Modified)]);
    }

    #[test]
    fn moves_across_the_directory_boundary_are_creations_and_removals() {
        let scope = Path::new("/index/scope");
        let renamed = |from: &str, to: &str| fff::WatchEvent {
            path: PathBuf::from(to),
            kind: fff::WatchEventKind::Renamed,
            from: Some(PathBuf::from(from)),
        };
        let wire = |event| wire_event(&event, scope);
        assert_eq!(
            wire(renamed("/index/other/a", "/index/scope/a")),
            Some(event("a", WatchEventKind::Created))
        );
        assert_eq!(
            wire(renamed("/index/scope/a", "/index/other/a")),
            Some(event("a", WatchEventKind::Removed))
        );
        assert_eq!(wire(renamed("/index/x", "/index/y")), None);
        let within = wire(renamed("/index/scope/a", "/index/scope/b/c")).unwrap();
        assert_eq!(within.from.as_deref(), Some("a"));
        assert_eq!(within.path, "b/c");
        let rescan = fff::WatchEvent {
            path: PathBuf::from("/index"),
            kind: fff::WatchEventKind::Rescan,
            from: None,
        };
        assert_eq!(wire(rescan), Some(WatchEvent::rescan()));
    }
}
