use crate::scan::WatcherStatus;

// What an index's loop knows of its watcher, the one source of the status it publishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Watched {
    // None is installed; `owed` while the next full scan is to install one, as after a loss.
    Missing { owed: bool },
    // A scan started one and walks once it reports in.
    Installing,
    // Installed; `reporting` once a full scan published with it in place.
    Installed { reporting: bool },
}

// The full scan that runs, as the status tells an index's first scan from a rebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Running {
    Nothing,
    Initial,
    Rescan,
}

// The watcher's status as the loop publishes it. `Replacing` reads as starting and takes watch
// subscriptions: the rebuild's rescan event covers what changes meanwhile.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Published {
    Disabled,
    Starting,
    Replacing,
    Ready,
    Failed,
    Stopped,
}

impl Watched {
    // The status, from whether the index watches, was stopped and scans; an owed replacement reads
    // as starting until its rebuild publishes, whose rescan event covers what changed meanwhile.
    pub(super) fn published(self, watch: bool, stopped: bool, running: Running) -> Published {
        let initial = running == Running::Initial;
        match self {
            _ if !watch => Published::Disabled,
            // Once stopped, no scan installs a watcher again.
            _ if stopped => Published::Stopped,
            Watched::Installing if initial => Published::Starting,
            Watched::Installing => Published::Replacing,
            Watched::Installed { reporting: true } => Published::Ready,
            Watched::Installed { .. } if initial => Published::Starting,
            Watched::Installed { .. } if running == Running::Rescan => Published::Replacing,
            // Its rebuild failed and waits the interval: the watcher reports meanwhile.
            Watched::Installed { .. } => Published::Ready,
            Watched::Missing { owed: true } => Published::Replacing,
            // As before an index's first scan ended, one whose install failed reads starting.
            Watched::Missing { .. } if initial => Published::Starting,
            Watched::Missing { .. } => Published::Failed,
        }
    }
}

impl Published {
    pub(crate) fn from_u8(value: u8) -> Self {
        [
            Self::Disabled,
            Self::Starting,
            Self::Replacing,
            Self::Ready,
            Self::Failed,
            Self::Stopped,
        ]
        .into_iter()
        .find(|status| *status as u8 == value)
        .unwrap_or(Self::Starting)
    }

    // Watch subscriptions are taken while the watcher reports or is being replaced.
    pub(crate) fn accepts_watches(self) -> bool {
        matches!(self, Self::Ready | Self::Replacing)
    }

    pub(crate) fn status(self) -> WatcherStatus {
        match self {
            Self::Disabled => WatcherStatus::Disabled,
            Self::Starting | Self::Replacing => WatcherStatus::Starting,
            Self::Ready => WatcherStatus::Ready,
            Self::Failed => WatcherStatus::Failed,
            Self::Stopped => WatcherStatus::Stopped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Published::*;
    use super::Running::{Initial, Nothing, Rescan};
    use super::*;

    const MISSING: Watched = Watched::Missing { owed: false };
    const OWED: Watched = Watched::Missing { owed: true };
    const INSTALLING: Watched = Watched::Installing;
    const INSTALLED: Watched = Watched::Installed { reporting: false };
    const REPORTING: Watched = Watched::Installed { reporting: true };

    // Every combination: what the loop knows, whether the watcher was stopped, the scan that
    // runs, and the status it publishes.
    #[rustfmt::skip]
    const TABLE: &[(Watched, bool, Running, Published)] = &[
        (MISSING, false, Nothing, Failed),
        (MISSING, false, Initial, Starting),
        (MISSING, false, Rescan, Failed),
        (MISSING, true, Nothing, Stopped),
        (MISSING, true, Initial, Stopped),
        (MISSING, true, Rescan, Stopped),
        (OWED, false, Nothing, Replacing),
        (OWED, false, Initial, Replacing),
        (OWED, false, Rescan, Replacing),
        (OWED, true, Nothing, Stopped),
        (OWED, true, Initial, Stopped),
        (OWED, true, Rescan, Stopped),
        (INSTALLING, false, Nothing, Replacing),
        (INSTALLING, false, Initial, Starting),
        (INSTALLING, false, Rescan, Replacing),
        (INSTALLING, true, Nothing, Stopped),
        (INSTALLING, true, Initial, Stopped),
        (INSTALLING, true, Rescan, Stopped),
        (INSTALLED, false, Nothing, Ready),
        (INSTALLED, false, Initial, Starting),
        (INSTALLED, false, Rescan, Replacing),
        (INSTALLED, true, Nothing, Stopped),
        (INSTALLED, true, Initial, Stopped),
        (INSTALLED, true, Rescan, Stopped),
        (REPORTING, false, Nothing, Ready),
        (REPORTING, false, Initial, Ready),
        (REPORTING, false, Rescan, Ready),
        (REPORTING, true, Nothing, Stopped),
        (REPORTING, true, Initial, Stopped),
        (REPORTING, true, Rescan, Stopped),
    ];

    #[test]
    fn the_status_follows_the_loop_s_state_in_every_combination() {
        assert_eq!(TABLE.len(), 5 * 2 * 3);
        for &(watched, stopped, running, expected) in TABLE {
            let row = (watched, stopped, running);
            assert_eq!(
                watched.published(true, stopped, running),
                expected,
                "{row:?}"
            );
            assert_eq!(
                watched.published(false, stopped, running),
                Disabled,
                "{row:?}"
            );
        }
    }

    #[test]
    fn a_published_status_reads_back_as_itself() {
        for status in [Disabled, Starting, Replacing, Ready, Failed, Stopped] {
            assert_eq!(Published::from_u8(status as u8), status);
        }
        assert_eq!(Replacing.status(), WatcherStatus::Starting);
    }
}
