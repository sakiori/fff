use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

type Listener<E> = Arc<dyn Fn(&E) + Send + Sync>;

// Listeners of one shared picker; they outlive picker replacements.
pub(crate) struct Listeners<E: ?Sized> {
    next_id: AtomicU64,
    listeners: Mutex<Vec<(u64, Listener<E>)>>,
}

pub(crate) type CoverageListeners = Listeners<()>;
pub(crate) type RepositoryListeners = Listeners<std::path::Path>;

impl<E: ?Sized> Default for Listeners<E> {
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
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().push((id, Arc::new(listener)));
        let listeners: Arc<dyn Remove> = self.clone();
        ListenerSubscription {
            listeners: Arc::downgrade(&listeners),
            id,
        }
    }

    // Listeners run outside the registration lock so they may subscribe or unsubscribe.
    pub(crate) fn notify(&self, event: &E) {
        let listeners: Vec<_> = self
            .listeners
            .lock()
            .iter()
            .map(|(_, listener)| Arc::clone(listener))
            .collect();
        for listener in listeners {
            listener(event);
        }
    }
}

trait Remove: Send + Sync {
    fn remove(&self, id: u64);
}

impl<E: ?Sized + 'static> Remove for Listeners<E> {
    fn remove(&self, id: u64) {
        self.listeners.lock().retain(|(own, _)| *own != id);
    }
}

/// Registration returned by [`crate::SharedFilePicker::subscribe_coverage`] and
/// [`crate::SharedFilePicker::subscribe_repository_state`]. Dropping it removes the listener.
#[must_use = "dropping the subscription removes the listener"]
pub struct ListenerSubscription {
    listeners: Weak<dyn Remove>,
    id: u64,
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
    }
}
