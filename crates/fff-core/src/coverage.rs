use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

type CoverageListener = Arc<dyn Fn() + Send + Sync>;

// Listeners of one shared picker; they outlive picker replacements.
#[derive(Default)]
pub(crate) struct CoverageListeners {
    next_id: AtomicU64,
    listeners: Mutex<Vec<(u64, CoverageListener)>>,
}

impl CoverageListeners {
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        listener: impl Fn() + Send + Sync + 'static,
    ) -> CoverageSubscription {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().push((id, Arc::new(listener)));
        CoverageSubscription {
            listeners: Arc::downgrade(self),
            id,
        }
    }

    // Listeners run outside the registration lock so they may subscribe or unsubscribe.
    pub(crate) fn notify(&self) {
        let listeners: Vec<_> = self
            .listeners
            .lock()
            .iter()
            .map(|(_, listener)| Arc::clone(listener))
            .collect();
        for listener in listeners {
            listener();
        }
    }
}

/// Registration returned by [`crate::SharedFilePicker::subscribe_coverage`].
/// Dropping it removes the listener.
#[must_use = "dropping the subscription removes the listener"]
pub struct CoverageSubscription {
    listeners: Weak<CoverageListeners>,
    id: u64,
}

impl std::fmt::Debug for CoverageSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverageSubscription")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for CoverageSubscription {
    fn drop(&mut self) {
        if let Some(listeners) = self.listeners.upgrade() {
            listeners.listeners.lock().retain(|(id, _)| *id != self.id);
        }
    }
}
