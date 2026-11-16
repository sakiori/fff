use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Instant;

use notify_debouncer_full::DebouncedEvent;
use parking_lot::Mutex;
use tracing::error;

use super::background_watcher::BackgroundWatcher;
use super::index_loop::IndexLoop;
use crate::FileSync;
use crate::error::Error;
use crate::file_picker::FilePicker;
use crate::index::BigramFilter;
use crate::rescan_stats::RescanReason;
use crate::scan::{RequestTicket, ScanConfig};
use crate::shared::{SharedFilePicker, SharedFrecency, WeakFilePicker};

#[cfg(test)]
pub(crate) type Work = Box<dyn FnOnce(&SharedFilePicker) + Send>;

// What reaches an index's loop; `index` names the index a message was made for, and a message
// for any other is dropped unapplied.
pub(crate) enum Message {
    // A new index takes the handle's place and starts its initial scan.
    Replace(Box<Replacement>),
    // The index leaves the handle, which holds none from then on.
    Remove {
        removed: Sender<()>,
    },
    // Watcher events, or changes a program reported; `applied` hears whether they were applied
    // rather than kept for a running scan.
    Events {
        index: u64,
        events: Vec<DebouncedEvent>,
        frecency: Option<SharedFrecency>,
        applied: Option<Sender<bool>>,
    },
    // An explicit request carries its ticket, which readiness counts until its rescan starts.
    Rescan {
        index: u64,
        reason: RescanReason,
        immediate: bool,
        ticket: Option<RequestTicket>,
        started: Option<Sender<bool>>,
    },
    // The watcher was lost to a task that panicked: the index is rebuilt with a new one.
    WatcherLost {
        index: u64,
    },
    // A scan's new watcher is installed, or could not be; its walk starts now.
    Watching {
        index: u64,
        scan: u64,
        watcher: Result<Box<BackgroundWatcher>, Error>,
    },
    Walked {
        index: u64,
        scan: u64,
        walked: Box<Result<FileSync, Error>>,
    },
    ContentIndexed {
        index: u64,
        scan: u64,
        bigram: Box<BigramFilter>,
    },
    PostScanned {
        index: u64,
        scan: u64,
    },
    Cancel {
        index: u64,
    },
    #[cfg(debug_assertions)]
    EndInterval {
        index: u64,
        done: Sender<()>,
    },
    #[cfg(test)]
    Due(Sender<Option<Instant>>),
    #[cfg(test)]
    Work {
        index: u64,
        work: Work,
    },
    #[cfg(test)]
    Wake,
}

pub(crate) struct Replacement {
    pub(crate) picker: FilePicker,
    pub(crate) frecency: SharedFrecency,
    pub(crate) config: ScanConfig,
    pub(crate) replaced: Sender<()>,
}

// An index's one writer, as Watchman's IO thread per root and Syncthing's folder loop: the loop
// owns the index's lifecycle, and everything that changes the index reaches it as a message.
#[derive(Default)]
pub(crate) struct IndexWriter {
    messages: Mutex<Option<Sender<Message>>>,
}

impl IndexWriter {
    // Queues a message, starting the loop with the first, never waiting; false if it cannot start.
    // A started loop lives as long as a handle, so it never restarts without its state.
    pub(crate) fn send(&self, picker: &SharedFilePicker, message: Message) -> bool {
        #[cfg(test)]
        tests::queued(&message);
        let mut messages = self.messages.lock();
        if let Some(sender) = messages.as_ref() {
            let sent = sender.send(message);
            // Dropping an unsent message can take this lock again, as a scan's end does.
            drop(messages);
            return sent.is_ok();
        }
        let (sender, receiver) = mpsc::channel();
        let weak = picker.weaken();
        #[cfg(test)]
        let refused = tests::refuse_spawn();
        #[cfg(not(test))]
        let refused = false;
        let spawned = match refused {
            true => Err(std::io::Error::other("refused by a test")),
            false => std::thread::Builder::new()
                .name("fff-index-writer".into())
                .spawn(move || run(weak, receiver)),
        };
        match spawned {
            Ok(_) => {
                let _ = sender.send(message);
                *messages = Some(sender);
                true
            }
            Err(e) => {
                error!(?e, "Failed to start the index writer");
                // Dropping a message can take this lock again, as a scan's end does.
                drop(messages);
                drop(message);
                false
            }
        }
    }
}

// Holds the index weakly and ends with the last handle; between messages it starts the pending
// rescan when due. A panicked turn may leave the index half written: the loop has it rebuilt.
fn run(picker: WeakFilePicker, messages: Receiver<Message>) {
    // The loop walks changed subtrees at the priority of the scans' pool.
    crate::parallelism::pin_user_initiated();
    let mut index_loop = IndexLoop::new();
    loop {
        match catch_unwind(AssertUnwindSafe(|| {
            turn(&picker, &messages, &mut index_loop)
        })) {
            Ok(true) => {}
            Ok(false) => return,
            Err(_) => {
                error!("The index's loop panicked; rescanning");
                let Some(shared) = picker.upgrade() else {
                    return;
                };
                let recovered = catch_unwind(AssertUnwindSafe(|| index_loop.recover(&shared)));
                if recovered.is_err() {
                    error!("Recovering the index panicked; rescanning after the interval");
                    let _ = catch_unwind(AssertUnwindSafe(|| index_loop.recover(&shared)));
                }
            }
        }
    }
}

// One message, or the due rescan; false once the index and its handles are gone.
fn turn(picker: &WeakFilePicker, messages: &Receiver<Message>, index_loop: &mut IndexLoop) -> bool {
    {
        let Some(due) = picker.upgrade().map(|picker| index_loop.due(&picker)) else {
            return false;
        };
        let message = match due {
            Some(due) => match messages.recv_timeout(due.saturating_duration_since(Instant::now()))
            {
                Ok(message) => Some(message),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return false,
            },
            None => match messages.recv() {
                Ok(message) => Some(message),
                Err(_) => return false,
            },
        };
        let Some(picker) = picker.upgrade() else {
            return false;
        };
        #[cfg(test)]
        tests::looped(&picker);
        match message {
            Some(message) => index_loop.handle(&picker, message),
            None => index_loop.start_due(&picker),
        }
        index_loop.publish_status(&picker);
    }
    true
}

// Test seams: a hook runs once on the loop before the next batch of its root, and the loop
// counts the batches of each index and how many of them ever ran at once.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    type Hook = Box<dyn FnOnce() + Send>;

    static HOOKS: Mutex<Vec<(PathBuf, Hook)>> = Mutex::new(Vec::new());
    static QUEUED: Mutex<Vec<(u64, usize)>> = Mutex::new(Vec::new());
    static LOOPS: Mutex<Vec<(PathBuf, usize)>> = Mutex::new(Vec::new());
    // Root, batches run, batches running now, most running at once.
    static JOBS: Mutex<Vec<(PathBuf, usize, usize, usize)>> = Mutex::new(Vec::new());

    thread_local! {
        static REFUSE_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    // The next writer this thread starts fails to spawn.
    pub(crate) fn refuse_next_spawn() {
        REFUSE_SPAWN.with(|refuse| refuse.set(true));
    }

    pub(super) fn refuse_spawn() -> bool {
        REFUSE_SPAWN.with(|refuse| refuse.replace(false))
    }

    pub(super) fn queued(message: &Message) {
        let index = match message {
            Message::Events { index, .. } | Message::Work { index, .. } => *index,
            _ => return,
        };
        let mut queued = QUEUED.lock();
        match queued.iter_mut().find(|(known, _)| *known == index) {
            Some((_, count)) => *count += 1,
            None => queued.push((index, 1)),
        }
    }

    // How many batches were queued for the index the id names.
    pub(crate) fn jobs_queued(index: u64) -> usize {
        (QUEUED.lock().iter())
            .find(|(known, _)| *known == index)
            .map_or(0, |(_, count)| *count)
    }

    pub(super) fn looped(picker: &SharedFilePicker) {
        let Some(root) = root_of(picker) else {
            return;
        };
        let mut loops = LOOPS.lock();
        match loops.iter_mut().find(|(path, _)| *path == root) {
            Some((_, count)) => *count += 1,
            None => loops.push((root, 1)),
        }
    }

    // How often the loop of `root`'s index went round.
    pub(crate) fn writer_loops(root: &Path) -> usize {
        (LOOPS.lock().iter())
            .find(|(path, _)| path == root)
            .map_or(0, |(_, count)| *count)
    }

    pub(crate) fn on_applying(root: &Path, hook: impl FnOnce() + Send + 'static) {
        HOOKS.lock().push((root.to_path_buf(), Box::new(hook)));
    }

    pub(crate) fn most_at_once(root: &Path) -> usize {
        (JOBS.lock().iter())
            .find(|(path, ..)| path == root)
            .map_or(0, |(.., most)| *most)
    }

    fn root_of(picker: &SharedFilePicker) -> Option<PathBuf> {
        let guard = picker.read().ok()?;
        Some(guard.as_ref()?.base_path().to_path_buf())
    }

    pub(crate) fn before_job(picker: &SharedFilePicker) {
        let Some(root) = root_of(picker) else {
            return;
        };
        let hook = {
            let mut hooks = HOOKS.lock();
            let position = hooks.iter().position(|(path, _)| *path == root);
            position.map(|position| hooks.remove(position).1)
        };
        if let Some(hook) = hook {
            hook();
        }
    }

    pub(crate) fn entered(picker: &SharedFilePicker) {
        let Some(root) = root_of(picker) else {
            return;
        };
        let mut jobs = JOBS.lock();
        match jobs.iter_mut().find(|(path, ..)| *path == root) {
            Some((_, run, now, most)) => {
                *run += 1;
                *now += 1;
                *most = (*most).max(*now);
            }
            None => jobs.push((root, 1, 1, 1)),
        }
    }

    pub(crate) fn left(picker: &SharedFilePicker) {
        let Some(root) = root_of(picker) else {
            return;
        };
        if let Some((_, _, now, _)) = JOBS.lock().iter_mut().find(|(path, ..)| *path == root) {
            *now = now.saturating_sub(1);
        }
    }
}
