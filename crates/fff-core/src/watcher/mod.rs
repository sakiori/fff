mod background_watcher;
pub use background_watcher::*;

mod dir_walk;

mod ref_watches;
#[cfg(feature = "libgit2")]
mod reftable;
pub(crate) use ref_watches::common_dir_of;

mod changed_paths;
mod git_changes;
pub(crate) use git_changes::is_git_file;
mod intake;
#[cfg(test)]
pub(crate) use intake::recrawls_logged;
mod reconcile;
#[cfg(test)]
pub(crate) use reconcile::apply_events;
mod renames;
mod repository;

mod rescan_tap;

mod index_loop;
mod watched;
pub(crate) use watched::Published;

mod writer;
#[cfg(test)]
pub(crate) use writer::Work;
#[cfg(test)]
pub(crate) use writer::tests::{
    jobs_queued, most_at_once, on_applying, refuse_next_spawn, writer_loops,
};
pub(crate) use writer::{IndexWriter, Message, Replacement};

mod watch;
pub use watch::*;

// The harness reads rescan counters, which release builds compile out.
#[cfg(all(test, rescan_stats))]
mod rescan_tests;
