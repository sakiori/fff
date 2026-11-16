use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

type Listener<E> = Arc<dyn Fn(&E) + Send + Sync>;

// Listeners of one shared picker, each with what it asked for; they outlive picker
// replacements.
pub(crate) struct Listeners<E: ?Sized, S = ()> {
    next_id: AtomicU64,
    listeners: Mutex<Vec<(u64, S, Listener<E>)>>,
}

pub(crate) type CoverageListeners = Listeners<()>;
pub(crate) type WorkTreeListeners = Listeners<Path>;
// Ref listeners hear the work trees related to their scope.
pub(crate) type RefListeners = Listeners<Path, PathBuf>;

impl<E: ?Sized, S> Default for Listeners<E, S> {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(0),
            listeners: Mutex::new(Vec::new()),
        }
    }
}

impl<E: ?Sized + 'static> Listeners<E> {
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        listener: impl Fn(&E) + Send + Sync + 'static,
    ) -> ListenerSubscription {
        self.subscribe_with((), listener)
    }

    pub(crate) fn notify(&self, event: &E) {
        self.notify_where(event, |()| true);
    }
}

impl<E: ?Sized + 'static, S: Clone + Send + 'static> Listeners<E, S> {
    pub(crate) fn subscribe_with(
        self: &Arc<Self>,
        asked: S,
        listener: impl Fn(&E) + Send + Sync + 'static,
    ) -> ListenerSubscription {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().push((id, asked, Arc::new(listener)));
        let listeners: Arc<dyn Remove> = self.clone();
        ListenerSubscription {
            listeners: Arc::downgrade(&listeners),
            id,
            after_drop: None,
        }
    }

    // What each listener asked for.
    pub(crate) fn asked(&self) -> Vec<S> {
        (self.listeners.lock().iter())
            .map(|(_, asked, _)| asked.clone())
            .collect()
    }

    // Listeners run outside the registration lock so they may subscribe or unsubscribe.
    pub(crate) fn notify_where(&self, event: &E, wants: impl Fn(&S) -> bool) {
        let listeners: Vec<_> = (self.listeners.lock().iter())
            .filter(|(_, asked, _)| wants(asked))
            .map(|(_, _, listener)| Arc::clone(listener))
            .collect();
        for listener in listeners {
            listener(event);
        }
    }
}

trait Remove: Send + Sync {
    fn remove(&self, id: u64);
}

impl<E: ?Sized + 'static, S: Send + 'static> Remove for Listeners<E, S> {
    // The listener is dropped after the lock is released: it may hold the last handle of the
    // index, whose drop waits for a notification that needs this lock.
    fn remove(&self, id: u64) {
        let removed = {
            let mut listeners = self.listeners.lock();
            let position = listeners.iter().position(|(own, _, _)| *own == id);
            position.map(|position| listeners.remove(position))
        };
        drop(removed);
    }
}

/// Registration returned by [`crate::SharedFilePicker::subscribe_coverage`],
/// [`crate::SharedFilePicker::subscribe_repository_state`] and
/// [`crate::SharedFilePicker::subscribe_ref_changes`]. Dropping it removes the listener.
#[must_use = "dropping the subscription removes the listener"]
pub struct ListenerSubscription {
    listeners: Weak<dyn Remove>,
    id: u64,
    after_drop: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl ListenerSubscription {
    // Runs `hook` once the listener is removed.
    pub(crate) fn after_drop(mut self, hook: impl FnOnce() + Send + Sync + 'static) -> Self {
        self.after_drop = Some(Box::new(hook));
        self
    }
}

impl std::fmt::Debug for ListenerSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListenerSubscription")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for ListenerSubscription {
    fn drop(&mut self) {
        if let Some(listeners) = self.listeners.upgrade() {
            listeners.remove(self.id);
        }
        if let Some(hook) = self.after_drop.take() {
            hook();
        }
    }
}
