use notify_debouncer_full::DebouncedEvent;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::background_watcher::MAX_BATCH_PATHS;

// Watcher events held while a scan builds the index that replaces the live one.
#[derive(Default)]
pub(crate) struct EventHold(Mutex<HoldState>);

#[derive(Default)]
struct HoldState {
    last_owner: u64,
    held: Option<Held>,
}

struct Held {
    owner: u64,
    events: Vec<DebouncedEvent>,
    paths: usize,
    overflowed: bool,
}

pub(crate) enum Released {
    Events(Vec<DebouncedEvent>),
    // More paths arrived than one batch may carry; they were dropped.
    Overflowed,
    // Nothing is held any more; events are handled as they arrive.
    Done,
}

impl EventHold {
    // A hold left behind by a replaced index is dropped. Checked under the lock, a
    // cancelled scan never takes over from the scan of the index that replaced it.
    pub(crate) fn hold(&self, cancelled: &AtomicBool) -> Option<u64> {
        let mut state = self.0.lock();
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        state.last_owner += 1;
        let owner = state.last_owner;
        state.held = Some(Held {
            owner,
            events: Vec::new(),
            paths: 0,
            overflowed: false,
        });
        Some(owner)
    }

    // Returns the events to handle now, or keeps them while a hold is active.
    pub(crate) fn defer(&self, events: Vec<DebouncedEvent>) -> Option<Vec<DebouncedEvent>> {
        let mut state = self.0.lock();
        let Some(held) = state.held.as_mut() else {
            return Some(events);
        };
        if held.overflowed {
            return None;
        }
        held.paths += events.iter().map(|event| event.paths.len()).sum::<usize>();
        if held.paths > MAX_BATCH_PATHS {
            held.overflowed = true;
            held.events = Vec::new();
        } else {
            held.events.extend(events);
        }
        None
    }

    // Takes the held events in arrival order; the hold ends once none remain.
    pub(crate) fn release(&self, owner: u64) -> Released {
        let mut state = self.0.lock();
        let Some(held) = state.held.as_mut().filter(|held| held.owner == owner) else {
            return Released::Done;
        };
        if held.overflowed {
            state.held = None;
            return Released::Overflowed;
        }
        if held.events.is_empty() {
            state.held = None;
            return Released::Done;
        }
        held.paths = 0;
        Released::Events(std::mem::take(&mut held.events))
    }

    pub(crate) fn discard(&self, owner: u64) {
        let mut state = self.0.lock();
        if state.held.as_ref().is_some_and(|held| held.owner == owner) {
            state.held = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn held_count(&self) -> usize {
        self.0
            .lock()
            .held
            .as_ref()
            .map_or(0, |held| held.events.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::Event;
    use notify::event::{CreateKind, EventKind};
    use std::path::PathBuf;
    use std::time::Instant;

    static LIVE: AtomicBool = AtomicBool::new(false);

    fn created(count: usize) -> Vec<DebouncedEvent> {
        (0..count)
            .map(|i| {
                let event = Event::new(EventKind::Create(CreateKind::File))
                    .add_path(PathBuf::from(format!("/w/{i}")));
                DebouncedEvent::new(event, Instant::now())
            })
            .collect()
    }

    fn released_events(hold: &EventHold, owner: u64) -> usize {
        match hold.release(owner) {
            Released::Events(events) => events.len(),
            Released::Overflowed => panic!("unexpected overflow"),
            Released::Done => 0,
        }
    }

    #[test]
    fn held_events_are_released_in_order_then_pass_through() {
        let hold = EventHold::default();
        assert!(hold.defer(created(1)).is_some());
        let owner = hold.hold(&LIVE).unwrap();
        assert!(hold.defer(created(2)).is_none());
        assert!(hold.defer(created(1)).is_none());
        assert_eq!(released_events(&hold, owner), 3);
        assert_eq!(released_events(&hold, owner), 0);
        assert!(hold.defer(created(1)).is_some());
    }

    #[test]
    fn a_hold_beyond_one_batch_drops_its_events() {
        let hold = EventHold::default();
        let owner = hold.hold(&LIVE).unwrap();
        assert!(hold.defer(created(MAX_BATCH_PATHS)).is_none());
        assert!(hold.defer(created(1)).is_none());
        assert!(hold.defer(created(1)).is_none());
        assert!(matches!(hold.release(owner), Released::Overflowed));
        assert!(hold.defer(created(1)).is_some());
    }

    #[test]
    fn a_new_hold_supersedes_a_stale_one() {
        let hold = EventHold::default();
        let stale = hold.hold(&LIVE).unwrap();
        let current = hold.hold(&LIVE).unwrap();
        assert!(hold.defer(created(1)).is_none());
        hold.discard(stale);
        assert!(matches!(hold.release(stale), Released::Done));
        assert_eq!(released_events(&hold, current), 1);
    }

    #[test]
    fn a_cancelled_scan_never_takes_the_hold() {
        let hold = EventHold::default();
        let current = hold.hold(&LIVE).unwrap();
        assert!(hold.hold(&AtomicBool::new(true)).is_none());
        assert!(hold.defer(created(1)).is_none());
        assert_eq!(released_events(&hold, current), 1);
    }
}
