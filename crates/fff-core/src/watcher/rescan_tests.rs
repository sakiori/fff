use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::Event;
use notify::EventKind;
use notify::event::{
    AccessKind, AccessMode, CreateKind, DataChange, Flag, MetadataKind, ModifyKind, RemoveKind,
    RenameMode,
};
use notify_debouncer_full::DebouncedEvent;
use tempfile::TempDir;

use super::handle_debounced_events;
use crate::constants::MAX_OVERFLOW_FILES;
use crate::file_picker::{FFFMode, FilePicker, FilePickerOptions};
use crate::frecency::FrecencyTracker;
use crate::git_status_worker::GitStatusWorker;
use crate::rescan_stats::{RescanReason, RescanStats};
use crate::shared::{SharedFilePicker, SharedFrecency};
use crate::watch::{WatchEvent, WatchEventKind, WatchOptions};
use std::sync::mpsc;

#[test]
fn new_empty_directories_are_indexed_without_rescanning() {
    let f = Fixture::new();
    f.write("existing.rs", "fn existing() {}");
    f.index();
    std::fs::create_dir_all(f.path("new/empty/nested")).unwrap();

    super::background_watcher::index_new_directory(
        &f.path("new"),
        &f.picker,
        &f.frecency,
        &f.git_workdir,
        &f.git_worker,
        None,
    );

    for path in ["new/", "new/empty/", "new/empty/nested/"] {
        assert!(
            f.indexed_dirs().contains(&path.to_owned()),
            "missing {path}"
        );
    }
    f.assert_no_rescan(&f.all_rescans(), "indexing empty directories");
}

#[test]
fn walking_a_known_directory_again_reports_only_new_files() {
    let f = Fixture::new();
    f.write("known/indexed.rs", "fn indexed() {}");
    f.index();
    let events = f.subscribe("");
    let index_dir = |rel: &str| {
        super::background_watcher::index_new_directory(
            &f.path(rel),
            &f.picker,
            &f.frecency,
            &f.git_workdir,
            &f.git_worker,
            None,
        )
    };

    // A known directory can be walked again, e.g. after an event for the directory itself.
    index_dir("known");
    f.write("fresh/added.rs", "fn added() {}");
    index_dir("fresh");

    // Events arrive in order, so the first batch shows whether the known file was reported.
    let received = events.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(received.len(), 1, "{received:?}");
    assert_eq!(received[0].path, f.path("fresh/added.rs"));
    assert_eq!(received[0].kind, WatchEventKind::Created);
}

#[test]
fn a_swapped_in_directory_reports_its_changed_files() {
    let f = Fixture::new();
    f.write("d/changed.rs", "fn old() {}");
    f.write("d/same.rs", "fn same() {}");
    f.index();
    let events = f.subscribe("");

    // `mv d d.bak && mv d.new d`: a new directory takes the indexed path.
    std::fs::rename(f.path("d"), f.path("d.bak")).unwrap();
    f.write("d.new/changed.rs", "fn new_and_longer() {}");
    f.write("d.new/same.rs", "fn same() {}");
    let same_mtime = std::fs::metadata(f.path("d.bak/same.rs"))
        .unwrap()
        .modified()
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(f.path("d.new/same.rs"))
        .unwrap()
        .set_modified(same_mtime)
        .unwrap();
    std::fs::rename(f.path("d.new"), f.path("d")).unwrap();
    f.feed([
        event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            f.path("d"),
        ),
        event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            f.path("d.new"),
        ),
    ]);

    let received = events.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(received.len(), 1, "{received:?}");
    assert_eq!(received[0].path, f.path("d/changed.rs"));
    assert_eq!(received[0].kind, WatchEventKind::Modified);
}

#[test]
fn recreated_empty_directories_reuse_and_revive_entries() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.path("empty/nested")).unwrap();
    f.index();
    let original = f.indexed_dirs();
    std::fs::remove_dir_all(f.path("empty")).unwrap();
    f.feed([event(
        EventKind::Remove(RemoveKind::Folder),
        f.path("empty"),
    )]);
    assert!(!f.indexed_dirs().contains(&"empty/".to_owned()));

    std::fs::create_dir_all(f.path("empty/nested")).unwrap();
    for _ in 0..2 {
        super::background_watcher::index_new_directory(
            &f.path("empty"),
            &f.picker,
            &f.frecency,
            &f.git_workdir,
            &f.git_worker,
            None,
        );
    }
    assert_eq!(f.indexed_dirs(), original);
    f.assert_no_rescan(&f.all_rescans(), "recreating empty directories");
}

#[test]
fn new_empty_directories_preserve_ignore_rules() {
    let f = Fixture::with_git();
    f.write(".gitignore", "ignored/\n");
    f.index();
    std::fs::create_dir_all(f.path("new/kept/empty")).unwrap();
    std::fs::create_dir_all(f.path("new/ignored/empty")).unwrap();
    super::background_watcher::index_new_directory(
        &f.path("new"),
        &f.picker,
        &f.frecency,
        &f.git_workdir,
        &f.git_worker,
        None,
    );
    let dirs = f.indexed_dirs();
    assert!(dirs.contains(&"new/kept/empty/".to_owned()));
    assert!(!dirs.iter().any(|p| p.contains("ignored")), "{dirs:?}");
}

#[test]
fn new_empty_directories_preserve_ignore_file_and_hidden_policy() {
    let f = Fixture::new();
    f.write(".ignore", "excluded/\n");
    f.index();
    for path in ["excluded/empty", ".hidden/empty", "included/empty"] {
        std::fs::create_dir_all(f.path(path)).unwrap();
        super::background_watcher::index_new_directory(
            f.path(path).parent().unwrap(),
            &f.picker,
            &f.frecency,
            &f.git_workdir,
            &f.git_worker,
            None,
        );
    }
    assert_eq!(f.indexed_dirs(), vec!["included/", "included/empty/"]);
}

#[test]
fn new_empty_directories_overflow_recovers_with_a_rescan() {
    let f = Fixture::new();
    f.index();
    for i in 0..=MAX_OVERFLOW_FILES {
        std::fs::create_dir_all(f.path(&format!("new/dir{i}"))).unwrap();
    }
    super::background_watcher::index_new_directory(
        &f.path("new"),
        &f.picker,
        &f.frecency,
        &f.git_workdir,
        &f.git_worker,
        None,
    );
    assert_eq!(f.all_rescans().count(RescanReason::IndexUpdateRejected), 1);
    assert!(f.picker.wait_for_indexing_complete(Duration::from_secs(10)));
    assert_eq!(f.indexed_dirs().len(), MAX_OVERFLOW_FILES + 2);
}

#[test]
fn a_deferred_rescan_still_reports_the_files_a_new_directory_added() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    // A cooldown no test outlasts, so the rescan below is deferred whatever the timing.
    let cooldown = Duration::from_secs(24 * 60 * 60);
    f.index_with_rescan_cooldown(cooldown);
    // An explicit scan starts the cooldown.
    f.picker.trigger_full_rescan_async(&f.frecency).unwrap();
    assert!(f.picker.wait_for_indexing_complete(Duration::from_secs(10)));
    assert_eq!(
        f.picker.read().unwrap().as_ref().unwrap().rescan_cooldown(),
        cooldown
    );
    let events = f.subscribe("**");
    let before = f.all_rescans();

    // More files than the overflow region takes: the rest needs a rescan.
    let files: Vec<String> = (0..=MAX_OVERFLOW_FILES)
        .map(|i| format!("new/file{i}.rs"))
        .collect();
    for rel in &files {
        f.write(rel, "");
    }
    super::background_watcher::index_new_directory(
        &f.path("new"),
        &f.picker,
        &f.frecency,
        &f.git_workdir,
        &f.git_worker,
        None,
    );

    let delta = f.all_rescans().since(&before);
    assert_eq!(
        delta.total, 0,
        "the rescan must wait for the cooldown, got {delta}"
    );
    assert_eq!(delta.count_throttled(RescanReason::IndexUpdateRejected), 1);
    assert!(
        f.picker.deferred_rescan_due().is_some(),
        "the rescan must stay deferred"
    );
    assert_eq!(
        f.picker.due_deferred_rescan(),
        None,
        "the cooldown has not ended"
    );

    // The files the walk added before capacity ran out are reported now: no rescan
    // replaces them yet.
    let accepted: Vec<PathBuf> = files
        .iter()
        .filter(|rel| f.is_indexed(rel))
        .map(|rel| f.path(rel))
        .collect();
    assert!(
        !accepted.is_empty() && accepted.len() < files.len(),
        "the walk must add a prefix of the directory, added {}",
        accepted.len()
    );
    // Every batch the walk dispatched has reached the subscriber once the dispatcher
    // is drained; nothing arrives later.
    f.picker.watch_registry().wait_for_dispatch();
    let reported: Vec<WatchEvent> = events.try_iter().flatten().collect();
    assert!(
        reported.iter().all(|e| e.kind == WatchEventKind::Created),
        "{reported:?}"
    );
    let mut reported: Vec<PathBuf> = reported.into_iter().map(|e| e.path).collect();
    reported.sort();
    let mut accepted = accepted;
    accepted.sort();
    assert_eq!(reported, accepted);

    // The deferred rescan keeps the reason it was requested for.
    f.picker.end_rescan_cooldown();
    assert_eq!(
        f.picker.due_deferred_rescan(),
        Some(RescanReason::IndexUpdateRejected)
    );
}

#[test]
fn saving_an_indexed_file_stays_incremental() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    f.write("src/main.rs", "fn main() { println!(); }");
    let delta = f.feed([modify(f.path("src/main.rs"))]);

    f.assert_no_rescan(&delta, "saving a tracked file");
}

#[test]
fn editor_atomic_save_stays_incremental() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    // write-to-temp + rename-over-target, the way vim/VSCode/IntelliJ save.
    f.write("src/main.rs", "fn main() { println!(); }");
    let target = f.path("src/main.rs");
    let temp = f.path("src/.main.rs.swp");
    let delta = f.feed([
        DebouncedEvent::new(
            Event::new(EventKind::Create(CreateKind::File)).add_path(temp.clone()),
            Instant::now(),
        ),
        DebouncedEvent::new(
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
                .add_path(temp.clone()),
            Instant::now(),
        ),
        DebouncedEvent::new(
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To)))
                .add_path(target.clone()),
            Instant::now(),
        ),
        DebouncedEvent::new(
            Event::new(EventKind::Remove(RemoveKind::File)).add_path(temp),
            Instant::now(),
        ),
    ]);

    f.assert_no_rescan(&delta, "an atomic editor save");
    assert!(f.is_indexed("src/main.rs"), "target must stay indexed");
}

#[test]
fn creating_and_deleting_files_stays_incremental() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    f.write("src/added.rs", "pub fn added() {}");
    let created = f.feed([create(f.path("src/added.rs"))]);
    f.assert_no_rescan(&created, "creating a file");
    assert!(f.is_indexed("src/added.rs"));

    f.remove("src/added.rs");
    let removed = f.feed([remove_file(f.path("src/added.rs"))]);
    f.assert_no_rescan(&removed, "deleting a file");
    assert!(!f.is_indexed("src/added.rs"));
}

#[test]
fn deleting_a_directory_stays_incremental() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.write("src/nested/a.rs", "");
    f.write("src/nested/b.rs", "");
    f.index();

    std::fs::remove_dir_all(f.path("src/nested")).unwrap();
    let delta = f.feed([DebouncedEvent::new(
        Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(f.path("src/nested")),
        Instant::now(),
    )]);

    f.assert_no_rescan(&delta, "deleting a directory");
    assert!(!f.is_indexed("src/nested/a.rs"));
    assert!(f.is_indexed("src/main.rs"));
}

#[test]
fn read_only_access_events_are_ignored() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    // fff's own preview + grep reads generate these; reacting to them would
    // make the picker rescan whenever the user scrolls the result list.
    let path = f.path("src/main.rs");
    let delta = f.feed([
        DebouncedEvent::new(
            Event::new(EventKind::Access(AccessKind::Read)).add_path(path.clone()),
            Instant::now(),
        ),
        DebouncedEvent::new(
            Event::new(EventKind::Access(AccessKind::Open(AccessMode::Read)))
                .add_path(path.clone()),
            Instant::now(),
        ),
        DebouncedEvent::new(
            Event::new(EventKind::Access(AccessKind::Close(AccessMode::Read))).add_path(path),
            Instant::now(),
        ),
    ]);

    f.assert_no_rescan(&delta, "read-only access events");
}

#[test]
fn recreating_the_same_paths_does_not_consume_overflow_capacity() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    // Recreated paths must reuse their overflow slots.
    for _ in 0..8 {
        for i in 0..200 {
            let rel = format!("gen/out{i}.rs");
            f.write(&rel, "generated");
            f.feed([create(f.path(&rel))]);
        }
        for i in 0..200 {
            let rel = format!("gen/out{i}.rs");
            f.remove(&rel);
            f.feed([remove_file(f.path(&rel))]);
        }
    }

    let delta = f.all_rescans();
    f.assert_no_rescan(&delta, "1600 create/delete cycles over 200 stable paths");
    assert!(
        f.overflow_len() <= 200,
        "each path must claim one overflow slot at most, got {}",
        f.overflow_len()
    );
}

#[test]
fn writes_inside_a_gitignored_directory_stay_incremental() {
    let f = Fixture::with_git();
    f.write(".gitignore", "target/\nnode_modules/\n");
    f.write("src/main.rs", "fn main() {}");
    f.index();

    let mut events = Vec::new();
    for i in 0..64 {
        let rel = format!("target/debug/artifact{i}.o");
        f.write(&rel, "binary");
        events.push(create(f.path(&rel)));
    }

    let delta = f.feed(events);
    f.assert_no_rescan(&delta, "build output written into an ignored directory");
}

#[test]
fn ignored_event_batch_above_index_capacity_stays_incremental() {
    let f = Fixture::with_git();
    f.write(".gitignore", "node_modules/\n");
    f.write("src/main.rs", "fn main() {}");
    f.index();

    let events = (0..MAX_OVERFLOW_FILES + 1)
        .map(|i| {
            let rel = format!("node_modules/pkg/file{i}.js");
            f.write(&rel, "");
            create(f.path(&rel))
        })
        .collect::<Vec<_>>();

    let delta = f.feed(events);
    f.assert_no_rescan(&delta, "ignored events above the index capacity");
    assert_eq!(f.overflow_len(), 0);
}

#[test]
fn repeated_edits_above_index_capacity_stay_incremental() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    let path = f.path("src/main.rs");
    let events = (0..MAX_OVERFLOW_FILES + 1)
        .map(|_| modify(path.clone()))
        .collect::<Vec<_>>();

    let delta = f.feed(events);
    f.assert_no_rescan(&delta, "repeated edits above the index capacity");
    assert_eq!(f.overflow_len(), 0);
}

#[test]
fn ignore_file_inside_an_ignored_directory_stays_incremental() {
    let f = Fixture::with_git();
    f.write(".gitignore", "node_modules/\n");
    f.write("src/main.rs", "fn main() {}");
    f.index();

    let ignore_files =
        ["left-pad", "lodash", "typescript"].map(|pkg| format!("node_modules/{pkg}/.gitignore"));

    for rel in &ignore_files {
        f.write(rel, "dist\n");
    }
    let delta = f.feed(ignore_files.iter().map(|rel| create(f.path(rel))));
    f.assert_no_rescan(&delta, "creating ignored .gitignore files");

    for rel in &ignore_files {
        f.write(rel, "build\n");
    }
    let delta = f.feed(ignore_files.iter().map(|rel| modify(f.path(rel))));
    f.assert_no_rescan(&delta, "modifying ignored .gitignore files");

    for rel in &ignore_files {
        f.remove(rel);
    }
    let delta = f.feed(ignore_files.iter().map(|rel| remove_file(f.path(rel))));
    f.assert_no_rescan(&delta, "removing ignored .gitignore files");
}

#[test]
fn ignore_file_inside_an_indexed_directory_triggers_a_rescan() {
    let f = Fixture::with_git();
    f.write("src/.gitignore", ".gitignore\ngenerated/\n");
    f.write("src/main.rs", "fn main() {}");
    f.index();

    f.write("src/.gitignore", ".gitignore\ngenerated/\nbuild/\n");
    let delta = f.feed([modify(f.path("src/.gitignore"))]);

    assert_eq!(delta.count(RescanReason::IgnoreFileChanged), 1);
}

#[test]
fn git_internal_churn_stays_incremental() {
    let f = Fixture::with_git();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    let git_dir = f.path(".git");
    let delta = f.feed([
        create(git_dir.join("index.lock")),
        modify(git_dir.join("index")),
        remove_file(git_dir.join("index.lock")),
        modify(git_dir.join("HEAD")),
        modify(git_dir.join("logs/HEAD")),
        modify(git_dir.join("COMMIT_EDITMSG")),
        modify(git_dir.join("refs/heads/main")),
    ]);

    f.assert_no_rescan(&delta, "git writing its own metadata");
}

#[test]
fn changing_the_root_ignore_file_triggers_a_rescan() {
    let f = Fixture::with_git();
    f.write(".gitignore", "target/\n");
    f.write("src/main.rs", "fn main() {}");
    f.index();

    f.write(".gitignore", "target/\nsrc/\n");
    let delta = f.feed([modify(f.path(".gitignore"))]);

    assert_eq!(
        delta.count(RescanReason::IgnoreFileChanged),
        1,
        "the indexed set depends on the root ignore rules, got {delta}"
    );
}

#[test]
fn kernel_event_loss_on_a_directory_triggers_a_rescan() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    let delta = f.feed([DebouncedEvent::new(
        Event::new(EventKind::Modify(ModifyKind::Any))
            .add_path(f.path("src"))
            .set_flag(Flag::Rescan),
        Instant::now(),
    )]);

    assert_eq!(
        delta.count(RescanReason::KernelEventLoss),
        1,
        "a dropped-events flag over a directory means unknown subtree state, got {delta}"
    );
}

#[test]
fn new_files_above_index_capacity_trigger_a_rescan() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    let events = (0..MAX_OVERFLOW_FILES + 1)
        .map(|i| {
            let rel = format!("src/bulk{i}.rs");
            f.write(&rel, "");
            create(f.path(&rel))
        })
        .collect::<Vec<_>>();

    let delta = f.feed(events);
    assert_eq!(
        delta.count(RescanReason::IndexUpdateRejected),
        1,
        "new files above the overflow region cannot be applied incrementally, got {delta}"
    );
}

#[test]
fn batch_at_the_overflow_boundary_stays_incremental() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    let events = (0..MAX_OVERFLOW_FILES)
        .map(|i| {
            let rel = format!("src/bulk{i}.rs");
            f.write(&rel, "");
            create(f.path(&rel))
        })
        .collect::<Vec<_>>();

    let delta = f.feed(events);
    f.assert_no_rescan(&delta, "a batch exactly at the overflow limit");
}

#[test]
fn event_batch_at_four_times_index_capacity_stays_incremental() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    let path = f.path("src/main.rs");
    let events = (0..MAX_OVERFLOW_FILES * 4)
        .map(|_| modify(path.clone()))
        .collect::<Vec<_>>();

    let delta = f.feed(events);
    f.assert_no_rescan(&delta, "an event batch exactly at the event limit");
}

#[test]
fn event_batch_above_four_times_index_capacity_triggers_a_rescan() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    let path = f.path("src/main.rs");
    let events = (0..MAX_OVERFLOW_FILES * 4 + 1)
        .map(|_| modify(path.clone()))
        .collect::<Vec<_>>();

    let delta = f.feed(events);
    assert_eq!(
        delta.count(RescanReason::EventBatchOverflow),
        1,
        "an event batch above four times the index capacity must rescan, got {delta}"
    );
}

#[test]
fn repeated_triggers_inside_the_cooldown_collapse_to_one_rescan() {
    let f = Fixture::with_git();
    f.write(".gitignore", "target/\n");
    f.write("src/main.rs", "fn main() {}");
    f.index();

    // Repeated batches during the cooldown must share one walk.
    for round in 0..50 {
        f.write(".gitignore", &format!("target/\n# round {round}\n"));
        f.feed([modify(f.path(".gitignore"))]);
    }

    let stats = f.all_rescans();
    assert_eq!(
        stats.total, 1,
        "50 triggers inside the cooldown must collapse to a single walk, got {stats}"
    );
    assert_eq!(
        stats.throttled, 49,
        "every suppressed request must be accounted for, got {stats}"
    );
}

#[test]
fn an_explicit_request_is_never_throttled() {
    let f = Fixture::with_git();
    f.write(".gitignore", "target/\n");
    f.write("src/main.rs", "fn main() {}");
    f.index();

    // Burn the cooldown with a watcher trigger, then confirm a user-initiated
    // refresh still goes through.
    f.write(".gitignore", "target/\nsrc/\n");
    f.feed([modify(f.path(".gitignore"))]);

    for _ in 0..3 {
        f.picker.trigger_full_rescan_async(&f.frecency).unwrap();
    }

    let stats = f.all_rescans();
    assert_eq!(
        stats.count(RescanReason::Explicit),
        3,
        "explicit refreshes must bypass the throttle, got {stats}"
    );
    assert_eq!(stats.count_throttled(RescanReason::Explicit), 0);
}

#[test]
fn events_after_a_suppressed_kernel_rescan_are_still_applied() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    f.write("src/added.rs", "pub fn added() {}");
    let delta = f.feed([
        DebouncedEvent::new(
            Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
                .add_path(f.path("src/main.rs"))
                .set_flag(Flag::Rescan),
            Instant::now(),
        ),
        create(f.path("src/added.rs")),
    ]);

    f.assert_no_rescan(&delta, "a dropped-events flag over a single tracked file");
    assert!(
        f.is_indexed("src/added.rs"),
        "suppressing the rescan must not drop the rest of the batch"
    );
}

#[test]
fn a_throttled_ignore_file_event_is_still_applied_incrementally() {
    let f = Fixture::with_git();
    f.write(".gitignore", "target/\n");
    f.write("src/main.rs", "fn main() {}");
    f.index();

    // Burn the cooldown: deleting .gitignore admits a full rescan.
    f.remove(".gitignore");
    let delta = f.feed([remove_file(f.path(".gitignore"))]);
    assert_eq!(delta.count(RescanReason::IgnoreFileChanged), 1);
    f.picker.wait_for_indexing_complete(Duration::from_secs(10));

    // Recreating it inside the cooldown throttles the rescan, but the file
    // itself must re-enter the index via the incremental fallback.
    f.write(".gitignore", "target/\n__ignored_x/\n");
    let delta = f.feed([create(f.path(".gitignore"))]);
    assert_eq!(delta.total, 0, "the rescan must be throttled, got {delta}");
    assert_eq!(delta.count_throttled(RescanReason::IgnoreFileChanged), 1);
    assert!(
        f.is_indexed(".gitignore"),
        "a throttled ignore-file event must still index the file itself"
    );
}

#[test]
fn renaming_a_file_carries_frecency_and_keeps_the_old_entry() {
    let f = Fixture::with_frecency();
    f.write("src/old.rs", "fn old() {}");
    f.index();

    for _ in 0..3 {
        f.track_access("src/old.rs");
    }
    let before = f.access_score("src/old.rs");
    assert!(before > 0, "precondition: source must be ranked");

    // A real rename so size and mtime are genuinely preserved.
    std::fs::rename(f.path("src/old.rs"), f.path("src/new.rs")).unwrap();
    let delta = f.feed([
        remove_file(f.path("src/old.rs")),
        create(f.path("src/new.rs")),
    ]);

    f.assert_no_rescan(&delta, "a rename");
    assert!(f.is_indexed("src/new.rs"), "destination must be indexed");
    assert_eq!(
        f.access_score("src/new.rs"),
        before,
        "destination inherits the history"
    );
    assert_eq!(
        f.access_score("src/old.rs"),
        before,
        "source keeps its history so old revisions still rank"
    );
    assert!(
        f.indexed_access_score("src/new.rs") > 0,
        "in-memory score must be refreshed, not left at zero until the next git pass"
    );
}

#[test]
fn renaming_a_file_emits_a_single_renamed_event() {
    let f = Fixture::new();
    f.write("src/old.rs", "fn old() {}");
    f.index();
    let events = f.subscribe("**");

    std::fs::rename(f.path("src/old.rs"), f.path("src/new.rs")).unwrap();
    f.feed([
        remove_file(f.path("src/old.rs")),
        create(f.path("src/new.rs")),
    ]);

    let received = events.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(received.len(), 1, "expected one event, got {received:?}");
    assert_eq!(received[0].kind, WatchEventKind::Renamed);
    assert_eq!(received[0].path, f.path("src/new.rs"));
    assert_eq!(
        received[0].from.as_deref(),
        Some(f.path("src/old.rs").as_path())
    );
}

#[test]
fn paired_rename_events_are_trusted_over_the_heuristic() {
    let f = Fixture::new();
    f.write("src/old.rs", "fn old() {}");
    f.index();
    let events = f.subscribe("**");

    std::fs::rename(f.path("src/old.rs"), f.path("src/new.rs")).unwrap();
    // Linux/inotify pairs the halves for us and hands over both paths at once.
    f.feed([DebouncedEvent::new(
        Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(f.path("src/old.rs"))
            .add_path(f.path("src/new.rs")),
        Instant::now(),
    )]);

    let received = events.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(received.len(), 1, "expected one event, got {received:?}");
    assert_eq!(received[0].kind, WatchEventKind::Renamed);
    assert_eq!(received[0].path, f.path("src/new.rs"));
    assert_eq!(
        received[0].from.as_deref(),
        Some(f.path("src/old.rs").as_path())
    );
}

#[test]
fn editing_an_indexed_file_is_never_mistaken_for_a_rename() {
    let f = Fixture::new();
    f.write("src/keep.rs", "same size!!");
    f.write("src/gone.rs", "same size!!");
    f.index();
    let events = f.subscribe("**");

    // Both files share a size; copying the mtime across makes their identities
    // collide. `keep.rs` is still indexed, so it is an edit, not a destination.
    let mtime = std::fs::metadata(f.path("src/gone.rs"))
        .unwrap()
        .modified()
        .unwrap();
    std::fs::remove_file(f.path("src/gone.rs")).unwrap();
    std::fs::File::open(f.path("src/keep.rs"))
        .unwrap()
        .set_modified(mtime)
        .unwrap();

    f.feed([
        remove_file(f.path("src/gone.rs")),
        modify(f.path("src/keep.rs")),
    ]);

    let received = events.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        received.iter().all(|e| e.kind != WatchEventKind::Renamed),
        "an edit to a live file must not be reported as a rename: {received:?}"
    );
}

#[test]
fn unrelated_delete_and_create_never_share_history() {
    let f = Fixture::with_frecency();
    f.write("src/tracked.rs", "fn tracked() {}");
    f.index();

    for _ in 0..3 {
        f.track_access("src/tracked.rs");
    }
    assert!(f.access_score("src/tracked.rs") > 0);

    f.remove("src/tracked.rs");
    f.write(
        "src/unrelated.rs",
        "fn unrelated() { /* different size */ }",
    );
    f.feed([
        remove_file(f.path("src/tracked.rs")),
        create(f.path("src/unrelated.rs")),
    ]);

    assert_eq!(
        f.access_score("src/unrelated.rs"),
        0,
        "an unrelated file must not inherit anyone's history"
    );
}

#[test]
fn ambiguous_batch_falls_back_to_remove_and_create() {
    let f = Fixture::new();
    f.write("src/a.rs", "same");
    f.write("src/b.rs", "same");
    // Identical mtimes make the pair ambiguous even if the writes straddle a second.
    let mtime = std::fs::metadata(f.path("src/a.rs"))
        .unwrap()
        .modified()
        .unwrap();
    std::fs::File::open(f.path("src/b.rs"))
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    f.index();
    let events = f.subscribe("**");

    // Two identical files renamed at once: nothing can be paired safely.
    std::fs::rename(f.path("src/a.rs"), f.path("src/x.rs")).unwrap();
    std::fs::rename(f.path("src/b.rs"), f.path("src/y.rs")).unwrap();
    f.feed([
        remove_file(f.path("src/a.rs")),
        remove_file(f.path("src/b.rs")),
        create(f.path("src/x.rs")),
        create(f.path("src/y.rs")),
    ]);

    let received = events.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        received.iter().all(|e| e.kind != WatchEventKind::Renamed),
        "ambiguous identities must not be guessed: {received:?}"
    );
    assert_eq!(received.len(), 4, "{received:?}");
}

// FSEvents reports a replaced root as one late batch of these kinds for the root itself.
#[test]
fn a_replaced_base_directory_keeps_every_file_indexed() {
    let f = Fixture::new();
    f.write("kept.rs", "fn kept() {}");
    f.write("nested/deep.rs", "fn deep() {}");
    f.write("dropped.rs", "fn dropped() {}");
    f.index();
    let events = f.subscribe("");

    let old = f.base.with_extension("old");
    std::fs::rename(&f.base, &old).unwrap();
    f.write("kept.rs", "fn kept() {}");
    f.write("nested/deep.rs", "fn deep() {}");
    f.write("added.rs", "fn added() {}");
    for rel in ["kept.rs", "nested/deep.rs"] {
        f.copy_mtime(&old.join(rel), rel);
    }
    std::fs::remove_dir_all(&old).unwrap();

    let (delta, new_dirs) = f.feed_for_new_dirs([
        event(EventKind::Remove(RemoveKind::Other), f.base.clone()),
        event(EventKind::Create(CreateKind::Folder), f.base.clone()),
        event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            f.base.clone(),
        ),
        event(
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Extended)),
            f.base.clone(),
        ),
    ]);

    for rel in ["kept.rs", "nested/deep.rs", "added.rs"] {
        assert!(f.is_indexed(rel), "{rel} must stay searchable");
    }
    // Nothing is left to a later walk: the batch's own update is the final state.
    assert!(new_dirs.is_empty(), "{new_dirs:?}");
    assert!(!f.is_indexed("dropped.rs"));
    assert!(f.indexed_dirs().contains(&"nested/".to_owned()));
    f.assert_no_rescan(&delta, "replacing the base directory");

    let mut received = events.recv_timeout(Duration::from_secs(1)).unwrap();
    received.sort_by(|a, b| a.path.cmp(&b.path));
    let received: Vec<_> = received.into_iter().map(|e| (e.path, e.kind)).collect();
    assert_eq!(
        received,
        vec![
            (f.path("added.rs"), WatchEventKind::Created),
            (f.path("dropped.rs"), WatchEventKind::Removed),
        ]
    );
}

// inotify reports the old files and the directory as removed, then the new directory.
#[test]
fn a_replaced_subdirectory_keeps_every_file_indexed() {
    let f = Fixture::new();
    f.write("outside.rs", "fn outside() {}");
    f.write("sub/kept.rs", "fn kept() {}");
    f.write("sub/inner/deep.rs", "fn deep() {}");
    f.write("sub/dropped.rs", "fn dropped() {}");
    f.index();

    std::fs::remove_dir_all(f.path("sub")).unwrap();
    f.write("sub/kept.rs", "fn kept() {}");
    f.write("sub/inner/deep.rs", "fn deep() {}");
    f.write("sub/added.rs", "fn added() {}");

    let (delta, new_dirs) = f.feed_for_new_dirs([
        remove_file(f.path("sub/kept.rs")),
        remove_file(f.path("sub/dropped.rs")),
        event(EventKind::Remove(RemoveKind::Folder), f.path("sub/inner")),
        event(EventKind::Remove(RemoveKind::Folder), f.path("sub")),
        event(EventKind::Create(CreateKind::Folder), f.path("sub")),
    ]);

    for rel in [
        "outside.rs",
        "sub/kept.rs",
        "sub/inner/deep.rs",
        "sub/added.rs",
    ] {
        assert!(f.is_indexed(rel), "{rel} must stay searchable");
    }
    assert!(new_dirs.is_empty(), "{new_dirs:?}");
    assert!(!f.is_indexed("sub/dropped.rs"));
    assert!(f.indexed_dirs().contains(&"sub/inner/".to_owned()));
    f.assert_no_rescan(&delta, "replacing a subdirectory");
}

// `mv d d.bak && mv new d` as FSEvents reports it: rename flags only, no removal.
#[test]
fn a_directory_renamed_over_a_known_one_replaces_its_entries() {
    let name = |path| event(EventKind::Modify(ModifyKind::Name(RenameMode::Any)), path);
    rename_over_a_known_directory(|f| {
        vec![
            name(f.path("d")),
            name(f.path("d.bak")),
            name(f.path("new")),
        ]
    });
}

// The same as inotify reports it: each move as its two halves and the paired event.
#[test]
fn a_directory_moved_over_a_known_one_replaces_its_entries() {
    let half = |mode, path| event(EventKind::Modify(ModifyKind::Name(mode)), path);
    let both = |from, to| {
        DebouncedEvent::new(
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(from)
                .add_path(to),
            Instant::now(),
        )
    };
    rename_over_a_known_directory(|f| {
        vec![
            half(RenameMode::From, f.path("d")),
            half(RenameMode::To, f.path("d.bak")),
            both(f.path("d"), f.path("d.bak")),
            half(RenameMode::From, f.path("new")),
            half(RenameMode::To, f.path("d")),
            both(f.path("new"), f.path("d")),
        ]
    });
}

fn rename_over_a_known_directory(batch: impl Fn(&Fixture) -> Vec<DebouncedEvent>) {
    let f = Fixture::new();
    f.write("d/kept.rs", "fn kept() {}");
    f.write("d/inner/deep.rs", "fn deep() {}");
    f.write("d/old_only.rs", "fn old_only() {}");
    f.index();

    f.write("new/kept.rs", "fn kept() {}");
    f.write("new/inner/deep.rs", "fn deep() {}");
    f.write("new/new_only.rs", "fn new_only() {}");
    std::fs::rename(f.path("d"), f.path("d.bak")).unwrap();
    std::fs::rename(f.path("new"), f.path("d")).unwrap();

    let (delta, new_dirs) = f.feed_for_new_dirs(batch(&f));

    for rel in ["d/kept.rs", "d/inner/deep.rs", "d/new_only.rs"] {
        assert!(f.is_indexed(rel), "{rel} must stay searchable");
    }
    assert!(
        !f.is_indexed("d/old_only.rs"),
        "a file the new d lacks lingers"
    );
    // Only the directory the index never saw is left to the walk of new directories.
    assert_eq!(new_dirs, vec![f.path("d.bak")]);
    f.index_new_dirs(&new_dirs);
    for rel in ["d.bak/kept.rs", "d.bak/inner/deep.rs", "d.bak/old_only.rs"] {
        assert!(f.is_indexed(rel), "{rel} must be indexed");
    }
    assert!(!f.indexed_dirs().iter().any(|dir| dir.starts_with("new/")));
    f.assert_no_rescan(&delta, "renaming a directory over a known one");
}

#[test]
fn a_removed_directory_leaves_the_index_whatever_the_removal_kind() {
    for kind in [RemoveKind::Other, RemoveKind::Any, RemoveKind::Folder] {
        let f = Fixture::new();
        f.write("outside.rs", "fn outside() {}");
        f.write("sub/inner/deep.rs", "fn deep() {}");
        f.index();

        std::fs::remove_dir_all(f.path("sub")).unwrap();
        f.feed([event(EventKind::Remove(kind), f.path("sub"))]);

        assert!(!f.is_indexed("sub/inner/deep.rs"), "{kind:?}");
        assert!(f.is_indexed("outside.rs"), "{kind:?}");
        assert!(
            !f.indexed_dirs().iter().any(|dir| dir.starts_with("sub/")),
            "{kind:?}"
        );
    }
}

// `rm f && mkdir f`: the file's entry leaves with the shapes both backends report.
#[test]
fn a_file_replaced_by_a_directory_leaves_the_index() {
    for first in [RemoveKind::File, RemoveKind::Other] {
        let f = Fixture::new();
        f.write("f", "a file");
        f.index();

        f.remove("f");
        f.write("f/inner.rs", "fn inner() {}");
        let (delta, new_dirs) = f.feed_for_new_dirs([
            event(EventKind::Remove(first), f.path("f")),
            event(EventKind::Create(CreateKind::Folder), f.path("f")),
        ]);

        assert!(!f.is_indexed("f"), "{first:?}: the replaced file lingers");
        assert_eq!(new_dirs, vec![f.path("f")], "{first:?}");
        f.index_new_dirs(&new_dirs);
        assert!(f.is_indexed("f/inner.rs"), "{first:?}");
        f.assert_no_rescan(&delta, "replacing a file by a directory");
    }
}

#[test]
fn sibling_and_nested_replacements_apply_in_one_batch() {
    let f = Fixture::new();
    for rel in [
        "a/kept.rs",
        "a/x/deep.rs",
        "a/x/gone.rs",
        "b/kept.rs",
        "b/gone.rs",
        "c/untouched.rs",
    ] {
        f.write(rel, "fn f() {}");
    }
    f.index();

    for dir in ["a", "b"] {
        std::fs::remove_dir_all(f.path(dir)).unwrap();
    }
    for rel in ["a/kept.rs", "a/x/deep.rs", "a/x/added.rs", "b/kept.rs"] {
        f.write(rel, "fn f() {}");
    }
    let replaced = |rel| {
        [
            event(EventKind::Remove(RemoveKind::Folder), f.path(rel)),
            event(EventKind::Create(CreateKind::Folder), f.path(rel)),
        ]
    };
    let (delta, new_dirs) = f.feed_for_new_dirs(
        [
            replaced("a/x"),
            replaced("a"),
            replaced("b"),
            replaced("a/x"),
        ]
        .into_iter()
        .flatten(),
    );

    for rel in [
        "a/kept.rs",
        "a/x/deep.rs",
        "a/x/added.rs",
        "b/kept.rs",
        "c/untouched.rs",
    ] {
        assert!(f.is_indexed(rel), "{rel} must be indexed");
    }
    for rel in ["a/x/gone.rs", "b/gone.rs"] {
        assert!(!f.is_indexed(rel), "{rel} lingers");
    }
    assert!(new_dirs.is_empty(), "{new_dirs:?}");
    f.assert_no_rescan(&delta, "replacing several directories");
}

// What a scan of the same tree holds: nothing below a directory the rules now ignore.
#[test]
fn a_replaced_directory_that_is_now_ignored_leaves_the_index() {
    let f = Fixture::with_git();
    f.write("kept.rs", "fn kept() {}");
    f.write("build/out.rs", "fn out() {}");
    f.index();

    f.write(".gitignore", "build/\n");
    std::fs::remove_dir_all(f.path("build")).unwrap();
    f.write("build/out.rs", "fn out() {}");
    f.feed([
        event(EventKind::Remove(RemoveKind::Folder), f.path("build")),
        event(EventKind::Create(CreateKind::Folder), f.path("build")),
    ]);

    assert!(!f.is_indexed("build/out.rs"));
    assert!(!f.indexed_dirs().contains(&"build/".to_owned()));
    assert!(f.is_indexed("kept.rs"));
}

#[test]
fn a_symlinked_directory_is_reconciled_only_when_the_index_follows_links() {
    for follow in [false, true] {
        let f = Fixture::new();
        // Outside the index, which zlob would otherwise walk once through the real path.
        let targets = tempfile::tempdir().unwrap();
        let (real, other) = (targets.path().join("real"), targets.path().join("other"));
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(real.join("a.rs"), "fn a() {}").unwrap();
        std::fs::write(other.join("b.rs"), "fn b() {}").unwrap();
        f.write("kept.rs", "fn kept() {}");
        std::os::unix::fs::symlink(&real, f.path("link")).unwrap();
        f.index_with_options(None, follow);
        assert_eq!(f.is_indexed("link/a.rs"), follow);

        // `ln -sfn other link`
        std::fs::remove_file(f.path("link")).unwrap();
        std::os::unix::fs::symlink(&other, f.path("link")).unwrap();
        let (_, new_dirs) = f.feed_for_new_dirs([
            event(EventKind::Remove(RemoveKind::Other), f.path("link")),
            event(EventKind::Create(CreateKind::Other), f.path("link")),
        ]);
        f.index_new_dirs(&new_dirs);

        assert!(!f.is_indexed("link/a.rs"), "follow={follow}");
        assert_eq!(f.is_indexed("link/b.rs"), follow, "follow={follow}");
        assert!(f.is_indexed("kept.rs"));
    }
}

#[test]
fn a_replacement_beyond_index_capacity_keeps_the_kept_files_and_rescans() {
    let f = Fixture::new();
    f.write("d/kept.rs", "fn kept() {}");
    f.index();

    std::fs::remove_dir_all(f.path("d")).unwrap();
    f.write("d/kept.rs", "fn kept() {}");
    for i in 0..=MAX_OVERFLOW_FILES {
        f.write(&format!("d/new{i}.rs"), "");
    }
    let delta = f.feed([
        event(EventKind::Remove(RemoveKind::Folder), f.path("d")),
        event(EventKind::Create(CreateKind::Folder), f.path("d")),
    ]);

    assert_eq!(delta.count(RescanReason::IndexUpdateRejected), 1, "{delta}");
    assert!(f.picker.wait_for_indexing_complete(Duration::from_secs(10)));
    assert!(f.is_indexed("d/kept.rs"));
    assert!(f.is_indexed(&format!("d/new{MAX_OVERFLOW_FILES}.rs")));
}

#[test]
fn a_replaced_subdirectory_reports_what_changed() {
    let f = Fixture::new();
    f.write("sub/kept.rs", "fn kept() {}");
    f.write("sub/dropped.rs", "fn dropped() {}");
    f.write("sub/edited.rs", "fn edited() {}");
    f.index();
    let events = f.subscribe("");

    let old = f.path("sub.old");
    std::fs::rename(f.path("sub"), &old).unwrap();
    f.write("sub/kept.rs", "fn kept() {}");
    f.write("sub/edited.rs", "fn edited_and_longer() {}");
    f.write("sub/added.rs", "fn added() {}");
    f.copy_mtime(&old.join("kept.rs"), "sub/kept.rs");
    std::fs::remove_dir_all(&old).unwrap();
    f.feed([
        event(EventKind::Remove(RemoveKind::Folder), f.path("sub")),
        event(EventKind::Create(CreateKind::Folder), f.path("sub")),
    ]);

    let mut received: Vec<_> = events
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .into_iter()
        .map(|e| (e.path, e.kind))
        .collect();
    received.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        received,
        vec![
            (f.path("sub/added.rs"), WatchEventKind::Created),
            (f.path("sub/dropped.rs"), WatchEventKind::Removed),
            (f.path("sub/edited.rs"), WatchEventKind::Modified),
        ]
    );
}

#[test]
fn a_new_directory_repeated_in_a_batch_is_walked_once() {
    let f = Fixture::new();
    f.index();
    f.write("new/a.rs", "fn a() {}");
    let created = || event(EventKind::Create(CreateKind::Folder), f.path("new"));
    let renamed = || {
        event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            f.path("new"),
        )
    };
    let (_, new_dirs) = f.feed_for_new_dirs([created(), renamed(), created()]);
    assert_eq!(new_dirs, vec![f.path("new")]);
}

// A rescan that commits while the batch walks took its snapshot after the batch's
// events; files it found are not removed by the older walk.
#[test]
fn a_walk_older_than_the_index_does_not_remove_its_files() {
    let f = Fixture::new();
    f.write("d/kept.rs", "fn kept() {}");
    f.index();

    let replaced =
        super::dir_walk::ReplacedDirs::walk(vec![f.path("d")], &f.base, false, false, &f.picker);
    f.write("d/later.rs", "fn later() {}");
    f.picker
        .write()
        .unwrap()
        .as_mut()
        .unwrap()
        .collect_files()
        .unwrap();
    assert!(f.is_indexed("d/later.rs"));

    let mut guard = f.picker.write().unwrap();
    replaced
        .apply(
            guard.as_mut().unwrap(),
            &f.base,
            |_| {},
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap();
    drop(guard);
    assert!(f.is_indexed("d/later.rs"));
}

// A directory without read permission is what a scan skips too: the rest of the replaced
// directory is reconciled, the unreadable part leaves as a scan would drop it, and nothing
// is rescanned. Granting access again is an attribute change that walks it again.
#[test]
fn an_unreadable_directory_inside_a_replaced_one_is_left_out_as_a_scan_would() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new();
    f.write("d/kept.rs", "fn kept() {}");
    f.write("d/dropped.rs", "fn dropped() {}");
    f.write("d/locked/inner.rs", "fn inner() {}");
    f.index();

    f.remove("d/dropped.rs");
    std::fs::set_permissions(f.path("d/locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }
    let restore = Restore(f.path("d/locked"));
    if std::fs::read_dir(f.path("d/locked")).is_ok() {
        return; // root ignores the mode
    }

    let delta = f.feed([event(
        EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
        f.path("d"),
    )]);

    assert!(f.is_indexed("d/kept.rs"));
    assert!(!f.is_indexed("d/dropped.rs"));
    assert!(!f.is_indexed("d/locked/inner.rs"));
    f.assert_no_rescan(&delta, "an unreadable directory");

    drop(restore);
    let (_, new_dirs) = f.feed_for_new_dirs([event(
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::Permissions)),
        f.path("d/locked"),
    )]);
    f.index_new_dirs(&new_dirs);
    assert!(f.is_indexed("d/locked/inner.rs"));
}

// `git checkout`, `rm -rf` and build tools remove directories while they are walked.
#[test]
fn a_directory_removed_while_it_is_walked_is_gone_not_a_failure() {
    let f = Fixture::new();
    f.write("d/kept.rs", "fn kept() {}");
    f.index();
    f.write("d/gone/x.rs", "fn x() {}");
    f.write("d/gone/deeper/y.rs", "fn y() {}");

    let gone = f.path("d/gone");
    let hook: crate::walk::DirHook = Arc::new(move |dir: &std::path::Path| {
        if dir == gone {
            let _ = std::fs::remove_dir_all(dir);
        }
    });
    let walk = super::dir_walk::DirWalk::new(&f.path("d"), &f.base, false, false, Some(&hook))
        .expect("a vanished directory does not fail the walk");

    let mut guard = f.picker.write().unwrap();
    walk.apply(
        guard.as_mut().unwrap(),
        &f.base,
        &mut Vec::new(),
        &mut Vec::new(),
    )
    .unwrap();
    drop(guard);
    assert!(f.is_indexed("d/kept.rs"));
    assert!(!f.is_indexed("d/gone/x.rs"));
    assert!(!f.is_indexed("d/gone/deeper/y.rs"));
}

// One dangling link anywhere in the tree must not stop the watcher from indexing.
#[test]
fn a_dangling_symlink_does_not_fail_a_walk_that_follows_links() {
    let f = Fixture::new();
    f.write("kept.rs", "fn kept() {}");
    std::os::unix::fs::symlink(f.path("missing"), f.path("dangling")).unwrap();
    f.index_with_options(None, true);

    f.write("new/a.rs", "fn a() {}");
    let before = f.all_rescans();
    f.index_new_dirs(&[f.path("new")]);

    assert!(f.is_indexed("new/a.rs"));
    f.assert_no_rescan(&f.all_rescans().since(&before), "a dangling link");
}

// `rm -r d && echo > d`, with the removal FSEvents and Windows report for it.
#[test]
fn a_directory_replaced_by_a_file_leaves_the_index() {
    for removal in [RemoveKind::Other, RemoveKind::Any] {
        let f = Fixture::new();
        f.write("d/inner.rs", "fn inner() {}");
        f.write("d/sub/deep.rs", "fn deep() {}");
        f.index();

        std::fs::remove_dir_all(f.path("d")).unwrap();
        f.write("d", "a file now");
        f.feed([
            event(EventKind::Remove(removal), f.path("d")),
            create(f.path("d")),
        ]);

        assert!(!f.is_indexed("d/inner.rs"), "{removal:?}");
        assert!(!f.is_indexed("d/sub/deep.rs"), "{removal:?}");
        assert!(f.is_indexed("d"), "{removal:?}");
    }
}

// A replaced directory brings new inodes: a file there with the size and modification time
// of the one it replaced is still read again, here found to be binary.
#[test]
fn a_replaced_file_with_the_same_size_and_time_is_read_again() {
    let f = Fixture::new();
    f.write("d/file.dat", "text");
    f.index();

    let old = f.path("d.old");
    std::fs::rename(f.path("d"), &old).unwrap();
    f.write("d/file.dat", "t\0xt");
    f.copy_mtime(&old.join("file.dat"), "d/file.dat");
    f.feed([event(
        EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
        f.path("d"),
    )]);

    let guard = f.picker.read().unwrap();
    let file = guard
        .as_ref()
        .unwrap()
        .get_file_by_path(f.path("d/file.dat"))
        .unwrap();
    assert!(file.is_binary());
}

// `mv d/a.rs moved.rs` in the batch that replaces `d`.
#[test]
fn a_file_moved_out_of_a_replaced_directory_is_a_rename() {
    let f = Fixture::with_frecency();
    f.write("d/a.rs", "fn a() { /* unique */ }");
    f.write("d/kept.rs", "fn kept() {}");
    f.index();
    f.track_access("d/a.rs");
    let events = f.subscribe("");

    std::fs::rename(f.path("d/a.rs"), f.path("moved.rs")).unwrap();
    std::fs::remove_dir_all(f.path("d")).unwrap();
    f.write("d/kept.rs", "fn kept() {}");
    let name = |rel| {
        event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            f.path(rel),
        )
    };
    f.feed([
        name("d/a.rs"),
        name("moved.rs"),
        event(EventKind::Remove(RemoveKind::Folder), f.path("d")),
        event(EventKind::Create(CreateKind::Folder), f.path("d")),
    ]);

    let received = events.recv_timeout(Duration::from_secs(1)).unwrap();
    let renamed: Vec<_> = received
        .iter()
        .filter(|e| e.kind == WatchEventKind::Renamed)
        .map(|e| (e.path.clone(), e.from.clone()))
        .collect();
    assert_eq!(
        renamed,
        vec![(f.path("moved.rs"), Some(f.path("d/a.rs")))],
        "{received:?}"
    );
    assert!(
        received.iter().all(|e| e.path != f.path("d/a.rs")),
        "{received:?}"
    );
    assert!(f.access_score("moved.rs") > 0);
}

// `mv backup d` where the backup's copy is older but the same size.
#[test]
fn a_file_replaced_by_an_older_copy_is_reported_modified() {
    let f = Fixture::new();
    f.write("d/file.rs", "fn new() {}");
    f.index();
    let events = f.subscribe("");

    std::fs::remove_dir_all(f.path("d")).unwrap();
    f.write("d/file.rs", "fn old() {}");
    std::fs::File::options()
        .write(true)
        .open(f.path("d/file.rs"))
        .unwrap()
        .set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000))
        .unwrap();
    f.feed([
        event(EventKind::Remove(RemoveKind::Folder), f.path("d")),
        event(EventKind::Create(CreateKind::Folder), f.path("d")),
    ]);

    let received = events.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(received.len(), 1, "{received:?}");
    assert_eq!(received[0].path, f.path("d/file.rs"));
    assert_eq!(received[0].kind, WatchEventKind::Modified);
}

struct Fixture {
    base: PathBuf,
    picker: SharedFilePicker,
    frecency: SharedFrecency,
    git_workdir: Option<PathBuf>,
    git_worker: Arc<GitStatusWorker>,
    // Dropped last so background work started by a triggered rescan still
    // sees the tree it was asked to walk.
    _tmp: TempDir,
    _frecency_tmp: Option<TempDir>,
}

impl Fixture {
    fn new() -> Self {
        Self::build(false, false)
    }

    fn with_git() -> Self {
        Self::build(true, false)
    }

    // `SharedFrecency::noop()` silently drops `init`, so any test asserting on
    // scores needs a real LMDB-backed tracker instead.
    fn with_frecency() -> Self {
        Self::build(false, true)
    }

    fn build(git: bool, frecency: bool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let git_workdir = git.then(|| {
            let status = Command::new("git")
                .args(["init", "-b", "main"])
                .current_dir(&base)
                .output()
                .expect("git init");
            assert!(status.status.success(), "git init failed");
            base.clone()
        });

        let (shared_frecency, frecency_tmp) = if frecency {
            let db = tempfile::tempdir().unwrap();
            let shared = SharedFrecency::default();
            shared
                .init(FrecencyTracker::open(db.path().join("frecency.mdb")).expect("open frecency"))
                .expect("init frecency");
            (shared, Some(db))
        } else {
            (SharedFrecency::noop(), None)
        };

        Self {
            base,
            picker: SharedFilePicker::default(),
            frecency: shared_frecency,
            git_workdir,
            git_worker: GitStatusWorker::new(),
            _tmp: tmp,
            _frecency_tmp: frecency_tmp,
        }
    }

    fn track_access(&self, rel: &str) {
        let guard = self.frecency.read().unwrap();
        guard
            .as_ref()
            .expect("fixture built without frecency")
            .track_access(&self.path(rel))
            .expect("track access");
    }

    fn access_score(&self, rel: &str) -> i64 {
        let guard = self.frecency.read().unwrap();
        guard
            .as_ref()
            .expect("fixture built without frecency")
            .get_access_score(&self.path(rel), FFFMode::Neovim)
    }

    fn indexed_access_score(&self, rel: &str) -> i16 {
        let guard = self.picker.read().unwrap();
        guard
            .as_ref()
            .and_then(|p| p.get_file_by_path(self.path(rel)))
            .map(|file| file.access_frecency_score)
            .unwrap_or(0)
    }

    fn subscribe(&self, pattern: &str) -> mpsc::Receiver<Vec<WatchEvent>> {
        let (sender, receiver) = mpsc::channel();
        self.picker
            .watch_registry()
            .subscribe(
                &self.base,
                pattern,
                WatchOptions::default(),
                Box::new(move |_, events| {
                    let _ = sender.send(events.to_vec());
                }),
            )
            .expect("subscribe");
        receiver
    }

    fn index(&self) {
        self.index_with(None);
    }

    fn index_with_rescan_cooldown(&self, cooldown: Duration) {
        self.index_with(Some(cooldown));
    }

    fn index_with(&self, rescan_cooldown: Option<Duration>) {
        self.index_with_options(rescan_cooldown, false);
    }

    fn index_with_options(&self, rescan_cooldown: Option<Duration>, follow_symlinks: bool) {
        let mut picker = FilePicker::new(FilePickerOptions {
            base_path: self.base.to_string_lossy().into_owned(),
            watch: false,
            rescan_cooldown,
            follow_symlinks,
            ..Default::default()
        })
        .unwrap();
        picker.collect_files().unwrap();
        self.picker.rebase_watches(&self.base);
        *self.picker.write().unwrap() = Some(picker);
    }

    fn feed(&self, events: impl IntoIterator<Item = DebouncedEvent>) -> RescanStats {
        let before = self.picker.rescan_stats();
        handle_debounced_events(
            FFFMode::Neovim,
            events.into_iter().collect(),
            &self.base,
            &self.git_workdir,
            &self.picker,
            &self.frecency,
            &self.git_worker,
        );

        self.picker.rescan_stats().since(&before)
    }

    fn feed_for_new_dirs(
        &self,
        events: impl IntoIterator<Item = DebouncedEvent>,
    ) -> (RescanStats, Vec<PathBuf>) {
        let before = self.picker.rescan_stats();
        let new_dirs = handle_debounced_events(
            FFFMode::Neovim,
            events.into_iter().collect(),
            &self.base,
            &self.git_workdir,
            &self.picker,
            &self.frecency,
            &self.git_worker,
        );
        (self.picker.rescan_stats().since(&before), new_dirs)
    }

    fn index_new_dirs(&self, dirs: &[PathBuf]) {
        for dir in dirs {
            super::background_watcher::index_new_directory(
                dir,
                &self.picker,
                &self.frecency,
                &self.git_workdir,
                &self.git_worker,
                None,
            );
        }
    }

    fn copy_mtime(&self, from: &std::path::Path, rel: &str) {
        let mtime = std::fs::metadata(from).unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(self.path(rel))
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    fn assert_no_rescan(&self, delta: &RescanStats, what: &str) {
        assert_eq!(delta.total, 0, "{what} must not trigger a rescan: {delta}");
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.base.join(rel)
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.path(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn remove(&self, rel: &str) {
        std::fs::remove_file(self.path(rel)).unwrap();
    }

    fn is_indexed(&self, rel: &str) -> bool {
        let guard = self.picker.read().unwrap();
        guard
            .as_ref()
            .and_then(|p| p.get_file_by_path(self.path(rel)))
            .is_some_and(|file| !file.is_deleted())
    }

    fn indexed_dirs(&self) -> Vec<String> {
        let guard = self.picker.read().unwrap();
        let picker = guard.as_ref().unwrap();
        let mut dirs: Vec<_> = picker
            .get_dirs()
            .iter()
            .filter(|dir| !dir.is_deleted())
            .map(|dir| dir.relative_path(picker).to_owned())
            .collect();
        dirs.sort();
        dirs
    }

    fn all_rescans(&self) -> RescanStats {
        self.picker.rescan_stats()
    }

    fn overflow_len(&self) -> usize {
        let guard = self.picker.read().unwrap();
        guard
            .as_ref()
            .map(|p| p.get_overflow_files().len())
            .unwrap_or(0)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A test that intentionally triggers a rescan leaves a walk running on
        // the background pool; let it finish before the tree disappears.
        self.picker
            .wait_for_indexing_complete(Duration::from_secs(10));
    }
}

fn event(kind: EventKind, path: PathBuf) -> DebouncedEvent {
    DebouncedEvent::new(Event::new(kind).add_path(path), Instant::now())
}

fn create(path: PathBuf) -> DebouncedEvent {
    event(EventKind::Create(CreateKind::File), path)
}

fn modify(path: PathBuf) -> DebouncedEvent {
    event(
        EventKind::Modify(ModifyKind::Data(DataChange::Content)),
        path,
    )
}

fn remove_file(path: PathBuf) -> DebouncedEvent {
    event(EventKind::Remove(RemoveKind::File), path)
}
