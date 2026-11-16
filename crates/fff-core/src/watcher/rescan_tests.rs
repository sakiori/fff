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

use crate::constants::MAX_OVERFLOW_FILES;
use crate::file_picker::{FFFMode, FilePicker, FilePickerOptions};
use crate::frecency::FrecencyTracker;
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

    f.feed([event(
        EventKind::Create(CreateKind::Folder),
        f.path("new").to_path_buf(),
    )]);

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
        f.feed([event(
            EventKind::Create(CreateKind::Folder),
            f.path(rel).to_path_buf(),
        )])
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

    // A copy that kept size and time is another inode: Git's stat data tells it apart too.
    let mut received = events.recv_timeout(Duration::from_secs(1)).unwrap();
    received.sort_by(|a, b| a.path.cmp(&b.path));
    let received: Vec<_> = received.into_iter().map(|e| (e.path, e.kind)).collect();
    let mut expected = vec![(f.path("d/changed.rs"), WatchEventKind::Modified)];
    if cfg!(unix) {
        expected.push((f.path("d/same.rs"), WatchEventKind::Modified));
    }
    assert_eq!(received, expected);
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
        f.feed([event(
            EventKind::Create(CreateKind::Folder),
            f.path("empty").to_path_buf(),
        )]);
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
    f.feed([event(
        EventKind::Create(CreateKind::Folder),
        f.path("new").to_path_buf(),
    )]);
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
        f.feed([event(
            EventKind::Create(CreateKind::Folder),
            f.path(path).parent().unwrap().to_path_buf(),
        )]);
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
    f.feed([event(
        EventKind::Create(CreateKind::Folder),
        f.path("new").to_path_buf(),
    )]);
    assert_eq!(f.all_rescans().count(RescanReason::IndexUpdateRejected), 1);
    assert!(f.picker.wait_for_indexing_complete(Duration::from_secs(10)));
    assert_eq!(f.indexed_dirs().len(), MAX_OVERFLOW_FILES + 2);
}

#[test]
fn a_directory_past_the_capacity_waits_whole_for_the_pending_rescan() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    // An interval no test outlasts, so the rescan below waits whatever the timing.
    let cooldown = Duration::from_secs(24 * 60 * 60);
    f.index_with_rescan_cooldown(cooldown);
    // The interval counts from the end of this scan.
    f.picker.trigger_full_rescan_async(&f.frecency).unwrap();
    assert!(f.picker.wait_for_indexing_complete(Duration::from_secs(10)));
    assert_eq!(
        f.picker.read().unwrap().as_ref().unwrap().rescan_cooldown(),
        cooldown
    );
    let events = f.subscribe("**");
    let before = f.all_rescans();

    // More files than the overflow region takes: all of them wait for a rescan, unread.
    let files: Vec<String> = (0..=MAX_OVERFLOW_FILES)
        .map(|i| format!("new/file{i}.rs"))
        .collect();
    for rel in &files {
        f.write(rel, "");
    }
    f.feed([event(
        EventKind::Create(CreateKind::Folder),
        f.path("new").to_path_buf(),
    )]);

    let delta = f.all_rescans().since(&before);
    assert_eq!(
        delta.total, 0,
        "the rescan must wait for the interval, got {delta}"
    );
    // It is the one pending, so it joined none.
    assert_eq!(delta.throttled, 0, "{delta}");
    assert!(
        (f.picker.next_rescan_due()).is_some_and(|due| due > Instant::now()),
        "the rescan must stay pending until the interval ends"
    );

    assert!(files.iter().all(|rel| !f.is_indexed(rel)));
    f.picker.watch_registry().wait_for_dispatch();
    let reported: Vec<WatchEvent> = events.try_iter().flatten().collect();
    assert!(reported.is_empty(), "{reported:?}");

    // The pending rescan starts once the interval ends, under the reason it was requested for.
    f.picker.end_rescan_cooldown();
    let delta = f.all_rescans().since(&before);
    assert_eq!(delta.count(RescanReason::OverflowCapacity), 1, "{delta}");
    assert!(f.picker.wait_for_indexing_complete(Duration::from_secs(10)));
    assert!(files.iter().all(|rel| f.is_indexed(rel)));
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
fn a_nested_ignore_file_change_updates_its_rules_and_reconciles_only_its_subtree() {
    let f = Fixture::with_git();
    f.write("src/.gitignore", "generated/\n");
    f.write("src/main.rs", "fn main() {}");
    f.write("src/build/out.rs", "fn out() {}");
    f.write("other/kept.rs", "fn kept() {}");
    f.index();
    assert!(f.is_indexed("src/build/out.rs"));

    // Changes the watcher never reported, one inside the subtree and one outside it.
    f.write("src/unreported.rs", "fn unreported() {}");
    f.write("other/unreported.rs", "fn unreported() {}");
    f.write("src/.gitignore", "generated/\nbuild/\n");
    let delta = f.feed([modify(f.path("src/.gitignore"))]);

    f.assert_no_rescan(&delta, "a nested ignore file change");
    assert!(!f.is_indexed("src/build/out.rs"), "the new rule applies");
    assert!(
        f.is_indexed("src/unreported.rs"),
        "its subtree is reconciled"
    );
    assert!(
        !f.is_indexed("other/unreported.rs"),
        "only its subtree is reconciled"
    );
    // Later changes are filtered with the new rules.
    f.write("src/build/later.rs", "fn later() {}");
    let delta = f.feed([create(f.path("src/build/later.rs"))]);
    f.assert_no_rescan(&delta, "a file created where the new rule applies");
    assert!(!f.is_indexed("src/build/later.rs"));
}

// Touches `rel` without changing it, as a checkout or an editor's save may.
fn touch(f: &Fixture, rel: &str) -> RescanStats {
    let path = f.path(rel);
    let later = std::time::SystemTime::now() + Duration::from_secs(5);
    let file = std::fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(later).unwrap();
    let kind = EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime));
    f.feed([event(kind, path)])
}

fn touching_leaves_its_subtree_alone(f: Fixture, rel: &str) {
    f.write("src/unreported.rs", "fn unreported() {}");
    let delta = touch(&f, rel);
    f.assert_no_rescan(&delta, "touching an ignore file");
    assert!(
        !f.is_indexed("src/unreported.rs"),
        "a touch of {rel} walked its subtree"
    );
}

#[test]
fn touching_an_ignore_file_without_changing_it_does_nothing() {
    let f = Fixture::with_git();
    f.write("src/.gitignore", "generated/\n");
    f.write("src/main.rs", "fn main() {}");
    f.index();
    touching_leaves_its_subtree_alone(f, "src/.gitignore");
}

#[test]
fn touching_an_ignore_file_that_ignores_itself_does_nothing() {
    let f = Fixture::with_git();
    f.write("src/.gitignore", ".gitignore\ngenerated/\n");
    f.write("src/main.rs", "fn main() {}");
    f.index();
    assert!(!f.is_indexed("src/.gitignore"));
    touching_leaves_its_subtree_alone(f, "src/.gitignore");
}

#[test]
fn touching_an_ignore_file_in_a_root_outside_git_does_nothing() {
    for name in [".ignore", ".gitignore"] {
        let f = Fixture::new();
        let rel = format!("src/{name}");
        f.write(&rel, "generated/\n");
        f.write("src/main.rs", "fn main() {}");
        f.index();
        touching_leaves_its_subtree_alone(f, &rel);
    }
}

// A file system with 2 s timestamps may date a write made as the walk began a tick earlier.
#[test]
fn an_ignore_file_written_a_timestamp_tick_before_the_walk_counts_as_changed() {
    let f = Fixture::with_git();
    f.write("src/.gitignore", "generated/\n");
    f.write("src/main.rs", "fn main() {}");
    let tick_before = std::time::SystemTime::now() - Duration::from_secs(1);
    f.set_mtime("src/.gitignore", tick_before);
    f.index();
    f.write("src/unreported.rs", "fn unreported() {}");
    let delta = f.feed([modify(f.path("src/.gitignore"))]);
    f.assert_no_rescan(&delta, "a racily clean ignore file");
    assert!(
        f.is_indexed("src/unreported.rs"),
        "its subtree is walked again"
    );
}

// The walk may have read the rules between a write and one that put the content back.
#[test]
fn an_ignore_file_written_back_while_the_walk_read_it_counts_as_changed() {
    let f = Fixture::with_git();
    f.write("src/.gitignore", "generated/\n");
    f.write("src/main.rs", "fn main() {}");
    f.set_mtime(
        "src/.gitignore",
        std::time::SystemTime::now() - Duration::from_secs(60),
    );
    let walk_began = std::time::SystemTime::now();
    f.index();
    f.write("src/unreported.rs", "fn unreported() {}");
    f.write("src/.gitignore", "generated/\n");
    f.set_mtime("src/.gitignore", walk_began);
    let delta = f.feed([modify(f.path("src/.gitignore"))]);
    f.assert_no_rescan(&delta, "an ignore file written back");
    assert!(
        f.is_indexed("src/unreported.rs"),
        "its subtree is walked again"
    );
}

// Only the directory's creation is reported: later events below it follow its own rules.
fn a_new_directory_s_ignore_file_filters_later_events_in_it(f: Fixture, name: &str) {
    f.write(&format!("pkg/{name}"), "gen/\n");
    f.write("pkg/main.rs", "fn main() {}");
    f.write("pkg/gen/a.rs", "fn a() {}");
    let created = event(EventKind::Create(CreateKind::Folder), f.path("pkg"));
    let delta = f.feed([created]);
    f.assert_no_rescan(&delta, "a new directory");
    assert!(f.is_indexed("pkg/main.rs"));
    assert!(!f.is_indexed("pkg/gen/a.rs"), "the walk honours pkg/{name}");
    f.write("pkg/gen/b.rs", "fn b() {}");
    f.feed([create(f.path("pkg/gen/b.rs"))]);
    assert!(
        !f.is_indexed("pkg/gen/b.rs"),
        "a later event below pkg ignores pkg/{name}"
    );
}

#[test]
fn a_new_directory_s_ignore_file_filters_later_events_in_it_in_a_repository() {
    let f = Fixture::with_git();
    f.write(".gitignore", "target/\n");
    f.write("src/main.rs", "fn main() {}");
    f.index();
    a_new_directory_s_ignore_file_filters_later_events_in_it(f, ".gitignore");
}

#[test]
fn a_new_directory_s_ignore_file_filters_later_events_in_it_outside_git() {
    let f = Fixture::new();
    f.write(".ignore", "target/\n");
    f.write("src/main.rs", "fn main() {}");
    f.index();
    a_new_directory_s_ignore_file_filters_later_events_in_it(f, ".ignore");
}

// A directory moved over a known one brings its own ignore file: later events follow it.
fn moved_over_with_other_rules(old_rules: &str, new_rules: &str) -> Fixture {
    let f = Fixture::with_git();
    f.write("pkg/.gitignore", old_rules);
    f.write("pkg/main.rs", "fn main() {}");
    f.index();
    f.write("pkg.new/.gitignore", new_rules);
    f.write("pkg.new/main.rs", "fn main() {}");
    std::fs::rename(f.path("pkg"), f.path("pkg.bak")).unwrap();
    std::fs::rename(f.path("pkg.new"), f.path("pkg")).unwrap();
    let name = |path| event(EventKind::Modify(ModifyKind::Name(RenameMode::Any)), path);
    let delta = f.feed([name(f.path("pkg")), name(f.path("pkg.new"))]);
    f.assert_no_rescan(&delta, "a directory moved over a known one");
    f.write("pkg/gen/later.rs", "fn later() {}");
    f.feed([create(f.path("pkg/gen/later.rs"))]);
    f
}

#[test]
fn a_directory_moved_over_a_known_one_brings_its_ignore_rules() {
    let f = moved_over_with_other_rules("other/\n", "gen/\n");
    assert!(!f.is_indexed("pkg/gen/later.rs"), "pkg's new rules apply");
}

#[test]
fn a_directory_moved_over_a_known_one_takes_the_old_ignore_rules_away() {
    let f = moved_over_with_other_rules("gen/\n", "other/\n");
    assert!(
        f.is_indexed("pkg/gen/later.rs"),
        "pkg's old rules no longer apply"
    );
}

// Other indexes' walks or post-scans may hold every thread of the shared pool.
#[test]
fn a_rule_change_reconcile_never_waits_for_the_shared_pool() {
    let f = Fixture::with_git();
    f.write("src/.gitignore", "generated/\n");
    f.write("src/main.rs", "fn main() {}");
    f.write("src/build/out.rs", "fn out() {}");
    f.index();
    let pool = crate::parallelism::indexing_pool();
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let busy = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for _ in 0..pool.current_num_threads() {
        let (release, busy) = (Arc::clone(&release), Arc::clone(&busy));
        pool.spawn(move || {
            busy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            while !release.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        });
    }
    let threads = pool.current_num_threads();
    let deadline = Instant::now() + Duration::from_secs(10);
    while busy.load(std::sync::atomic::Ordering::SeqCst) < threads && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    f.write("src/.gitignore", "generated/\nbuild/\n");
    let applied = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            f.feed([modify(f.path("src/.gitignore"))]);
            applied.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while !applied.load(std::sync::atomic::Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let waited = !applied.load(std::sync::atomic::Ordering::SeqCst);
        release.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(!waited, "the rule change waited for the shared pool");
    });
    assert!(!f.is_indexed("src/build/out.rs"));
}

// A walk of an index that is replaced meanwhile stops: its writer serves the new index.
#[test]
fn a_new_directory_walk_stops_once_its_index_is_cancelled() {
    let f = Fixture::new();
    f.write("existing.rs", "fn existing() {}");
    f.index();
    for i in 0..200 {
        f.write(&format!("new/d{i}/f.rs"), "fn f() {}");
    }
    let walked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (seen, picker) = (Arc::clone(&walked), f.picker.clone());
    let hook: crate::walk::DirHook = Arc::new(move |_: &std::path::Path| {
        if seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            picker.cancel();
        }
        std::thread::sleep(Duration::from_millis(2));
    });
    let created = event(EventKind::Create(CreateKind::Folder), f.path("new"));
    super::apply_events(&f.picker, &f.options(), None, Some(hook), vec![created]);
    let walked = walked.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        walked < 100,
        "the walk went on through {walked} directories"
    );
    assert!(!f.indexed_dirs().iter().any(|dir| dir.starts_with("new/")));
}

// Git's racy-git case: rewritten within the second its entry was recorded, with the same size
// and inode, the file matches its entry's identity though its content changed.
#[test]
fn a_reconcile_reads_again_a_file_rewritten_in_the_second_it_was_recorded() {
    let f = Fixture::new();
    f.write("d/a.rs", "const zebra_alpha: u8 = 0;\n");
    f.write("d/b.rs", "fn b() {}");
    // Dated after the recording, as a write in the same second may be.
    let recorded = std::time::SystemTime::now() + Duration::from_secs(5);
    f.set_mtime("d/a.rs", recorded);
    f.index();
    let events = f.subscribe("");
    std::fs::write(f.path("d/a.rs"), "const zebra_gamma: u8 = 0;\n").unwrap();
    f.set_mtime("d/a.rs", recorded);
    // An event for the known directory itself has its subtree walked again.
    let renamed = EventKind::Modify(ModifyKind::Name(RenameMode::Any));
    let delta = f.feed([event(renamed, f.path("d"))]);
    f.assert_no_rescan(&delta, "a reconcile of the file's directory");
    f.picker.watch_registry().wait_for_dispatch();
    let modified: Vec<PathBuf> = (events.try_iter().flatten())
        .filter(|event| event.kind == WatchEventKind::Modified)
        .map(|event| event.path)
        .collect();
    assert_eq!(modified, [f.path("d/a.rs")]);
    assert_eq!(f.files_containing("zebra_gamma"), ["d/a.rs"]);
}

// A FIFO, socket or device is never indexed, so its event never opens it.
#[cfg(unix)]
#[test]
fn a_fifo_in_the_tree_is_never_opened_or_indexed() {
    let f = Fixture::new();
    f.write("kept.rs", "fn kept() {}");
    f.index();
    let fifo = f.path("pipe");
    let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(made.success());
    let (done, applied) = mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            f.feed([create(fifo.clone()), modify(fifo.clone())]);
            let _ = done.send(());
        });
        let finished = applied.recv_timeout(Duration::from_secs(2)).is_ok();
        // Unblocks a reader that opened it, so the test ends either way.
        use std::os::unix::fs::OpenOptionsExt;
        let mut writer = std::fs::OpenOptions::new();
        drop(
            writer
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo),
        );
        assert!(finished, "applying the FIFO's events blocked");
    });
    assert!(!f.is_indexed("pipe"));
    assert!(f.is_indexed("kept.rs"));
}

// Not racy when recorded, rewritten later within the same second with the same size: only the
// sub-second time tells.
#[test]
fn a_reconcile_finds_a_same_size_rewrite_later_in_the_same_second() {
    let f = Fixture::new();
    f.write("d/a.rs", "const zebra_alpha: u8 = 0;\n");
    let second = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    f.set_mtime("d/a.rs", second + Duration::from_millis(100));
    f.index();
    let events = f.subscribe("");
    std::fs::write(f.path("d/a.rs"), "const zebra_gamma: u8 = 0;\n").unwrap();
    f.set_mtime("d/a.rs", second + Duration::from_millis(600));
    let renamed = EventKind::Modify(ModifyKind::Name(RenameMode::Any));
    let delta = f.feed([event(renamed, f.path("d"))]);
    f.assert_no_rescan(&delta, "a reconcile of the file's directory");
    f.picker.watch_registry().wait_for_dispatch();
    let modified: Vec<PathBuf> = (events.try_iter().flatten())
        .filter(|event| event.kind == WatchEventKind::Modified)
        .map(|event| event.path)
        .collect();
    assert_eq!(modified, [f.path("d/a.rs")]);
}

// Git compares the exclude file by content too: a touch or chmod changes no rule.
#[test]
fn touching_the_exclude_file_does_nothing() {
    let f = Fixture::with_git();
    f.write(".git/info/exclude", "docs/\n");
    f.write("src/main.rs", "fn main() {}");
    f.index();
    f.write("src/unreported.rs", "fn unreported() {}");
    let exclude = f.path(".git/info/exclude");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&exclude, permissions).unwrap();
    }
    let delta = touch(&f, ".git/info/exclude");
    f.assert_no_rescan(&delta, "touching the exclude file");
    assert!(
        !f.is_indexed("src/unreported.rs"),
        "a touch walked the tree"
    );
    f.write(".git/info/exclude", "src/\n");
    f.feed([modify(exclude)]);
    assert!(!f.is_indexed("src/main.rs"), "a content change applies");
}

// A directory moved in brings its own ignore file; a build writes below it meanwhile.
fn moved_in_with_rules(f: &Fixture) -> tempfile::TempDir {
    f.write("src/main.rs", "fn main() {}");
    f.index();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(outside.path().join("pkg/gen")).unwrap();
    std::fs::write(outside.path().join("pkg/.gitignore"), "gen/\n").unwrap();
    std::fs::write(outside.path().join("pkg/.ignore"), "gen/\n").unwrap();
    std::fs::write(outside.path().join("pkg/main.rs"), "fn main() {}").unwrap();
    std::fs::rename(outside.path().join("pkg"), f.path("pkg")).unwrap();
    outside
}

fn a_moved_in_directory_s_rules_apply_to_child_events_of_its_batch(f: Fixture) {
    let _outside = moved_in_with_rules(&f);
    f.write("pkg/gen/b.rs", "fn b() {}");
    let moved = EventKind::Modify(ModifyKind::Name(RenameMode::To));
    f.feed([event(moved, f.path("pkg")), create(f.path("pkg/gen/b.rs"))]);
    assert!(f.is_indexed("pkg/main.rs"));
    assert!(!f.is_indexed("pkg/gen/b.rs"), "pkg's rules leave it out");
}

#[test]
fn a_moved_in_directory_s_rules_apply_to_child_events_of_its_batch_in_a_repository() {
    a_moved_in_directory_s_rules_apply_to_child_events_of_its_batch(Fixture::with_git());
}

#[test]
fn a_moved_in_directory_s_rules_apply_to_child_events_of_its_batch_outside_git() {
    a_moved_in_directory_s_rules_apply_to_child_events_of_its_batch(Fixture::new());
}

// The next batch, below the moved-in directory, follows the rules its walk read.
fn a_moved_in_directory_s_walk_undoes_an_earlier_child_event(f: Fixture) {
    let _outside = moved_in_with_rules(&f);
    let moved = EventKind::Modify(ModifyKind::Name(RenameMode::To));
    f.feed([event(moved, f.path("pkg"))]);
    f.write("pkg/gen/b.rs", "fn b() {}");
    f.feed([create(f.path("pkg/gen/b.rs"))]);
    assert!(f.is_indexed("pkg/main.rs"));
    assert!(
        !f.is_indexed("pkg/gen/b.rs"),
        "the walk keeps only what it lists"
    );
}

#[test]
fn a_moved_in_directory_s_walk_undoes_an_earlier_child_event_in_a_repository() {
    a_moved_in_directory_s_walk_undoes_an_earlier_child_event(Fixture::with_git());
}

#[test]
fn a_moved_in_directory_s_walk_undoes_an_earlier_child_event_outside_git() {
    a_moved_in_directory_s_walk_undoes_an_earlier_child_event(Fixture::new());
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
fn changing_the_root_ignore_file_reconciles_the_root_without_a_rescan() {
    let f = Fixture::with_git();
    f.write(".gitignore", "target/\n");
    f.write("src/main.rs", "fn main() {}");
    f.write("docs/guide.md", "guide");
    f.index();

    f.write(".gitignore", "target/\nsrc/\n");
    let delta = f.feed([modify(f.path(".gitignore"))]);

    f.assert_no_rescan(&delta, "a root ignore file change");
    assert!(!f.is_indexed("src/main.rs"), "the new root rule applies");
    assert!(f.is_indexed("docs/guide.md"));
}

#[test]
fn changing_the_exclude_file_reconciles_the_root_without_a_rescan() {
    let f = Fixture::with_git();
    f.write("src/main.rs", "fn main() {}");
    f.write("docs/guide.md", "guide");
    f.index();

    f.write(".git/info/exclude", "docs/\n");
    let delta = f.feed([modify(f.path(".git/info/exclude"))]);

    f.assert_no_rescan(&delta, "an exclude file change");
    assert!(!f.is_indexed("docs/guide.md"), "the exclude rule applies");
    assert!(f.is_indexed("src/main.rs"));
}

#[test]
fn lost_events_for_a_subdirectory_reconcile_only_its_subtree() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.write("docs/guide.md", "guide");
    f.index();

    // Changes whose events were lost, inside the named subtree and outside it.
    f.write("src/unreported.rs", "fn unreported() {}");
    f.write("docs/unreported.md", "unreported");
    f.remove("src/main.rs");
    let delta = f.feed([lost(Some(f.path("src")))]);

    f.assert_no_rescan(&delta, "lost events for a subdirectory");
    assert!(f.is_indexed("src/unreported.rs"));
    assert!(!f.is_indexed("src/main.rs"));
    assert!(
        !f.is_indexed("docs/unreported.md"),
        "only the named subtree is reconciled"
    );
}

#[test]
fn lost_events_without_a_path_reconcile_the_root() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.write("docs/guide.md", "guide");
    f.index();

    f.write("src/unreported.rs", "fn unreported() {}");
    f.write("docs/unreported.md", "unreported");
    f.remove("docs/guide.md");
    let delta = f.feed([lost(None)]);

    f.assert_no_rescan(&delta, "a queue overflow");
    assert!(f.is_indexed("src/unreported.rs"));
    assert!(f.is_indexed("docs/unreported.md"));
    assert!(!f.is_indexed("docs/guide.md"));
}

#[test]
fn lost_events_for_an_ancestor_of_the_root_reconcile_the_root() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    f.write("docs/unreported.md", "unreported");
    let delta = f.feed([lost(f.base.parent().map(PathBuf::from))]);

    f.assert_no_rescan(&delta, "lost events above the root");
    assert!(f.is_indexed("docs/unreported.md"));
}

#[test]
fn lost_events_inside_an_ignored_directory_are_dropped() {
    let f = Fixture::with_git();
    f.write(".gitignore", "node_modules/\n");
    f.write("src/main.rs", "fn main() {}");
    f.write("node_modules/pkg/index.js", "");
    f.index();

    f.write("src/unreported.rs", "fn unreported() {}");
    let delta = f.feed([lost(Some(f.path("node_modules/pkg")))]);

    f.assert_no_rescan(&delta, "lost events in an ignored directory");
    assert!(!f.is_indexed("src/unreported.rs"), "nothing is walked");
    assert!(!f.is_indexed("node_modules/pkg/index.js"));
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
        delta.count(RescanReason::OverflowCapacity),
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
fn event_batch_above_four_times_index_capacity_stays_incremental_too() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    let path = f.path("src/main.rs");
    f.write("src/main.rs", "fn main() { edited(); }");
    let events = (0..MAX_OVERFLOW_FILES * 4 + 1)
        .map(|_| modify(path.clone()))
        .collect::<Vec<_>>();

    let delta = f.feed(events);
    f.assert_no_rescan(&delta, "an event batch above the event limit");
    assert_eq!(f.indexed_size("src/main.rs"), Some(23));
}

#[test]
fn a_batch_of_5000_paths_aggregates_into_their_directories_without_a_rescan() {
    let f = Fixture::new();
    let rel = |i: usize| format!("pkg{}/lib/m{}.rs", i / 100, i % 100);
    for i in 0..5000 {
        f.write(&rel(i), "");
    }
    f.index();
    let events = f.subscribe("**");

    // Every file is rewritten, as a formatter or a checkout does.
    for i in 0..5000 {
        f.write(&rel(i), "pub fn edited() {}");
    }
    let delta = f.feed((0..5000).map(|i| modify(f.path(&rel(i)))));

    f.assert_no_rescan(&delta, "a batch of 5000 paths");
    for i in [0, 2499, 4999] {
        assert_eq!(f.indexed_size(&rel(i)), Some(18), "{}", rel(i));
    }
    // Subscribers hear of each change, also where it was checked by walking its directory.
    f.picker.watch_registry().wait_for_dispatch();
    let mut modified: Vec<PathBuf> = (events.try_iter().flatten())
        .filter(|event| event.kind == WatchEventKind::Modified)
        .map(|event| event.path)
        .collect();
    modified.sort();
    modified.dedup();
    assert_eq!(modified.len(), 5000);
}

#[test]
fn repeated_requests_collapse_into_one_pending_rescan() {
    let f = Fixture::with_git();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    // The first request starts a rescan; the rest join the one pending behind it.
    for _ in 0..50 {
        let reason = RescanReason::OverflowCapacity;
        let _ = f
            .picker
            .trigger_full_rescan_with_reason(&f.frecency, reason);
    }

    let stats = f.all_rescans();
    assert_eq!(stats.total, 1, "one rescan starts at once, got {stats}");
    // The second request makes the pending rescan; the 48 after it join that one.
    assert_eq!(
        stats.throttled, 48,
        "every request that joined the pending one is accounted for, got {stats}"
    );
    assert!(f.picker.wait_for_indexing_complete(Duration::from_secs(10)));
    // The loop takes the end of the running rescan's post-scan phase as a message.
    let deadline = Instant::now() + Duration::from_secs(10);
    while f.picker.next_rescan_due().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        f.picker.next_rescan_due().is_some(),
        "one rescan stays pending for the interval"
    );
}

#[test]
fn explicit_requests_during_a_scan_run_once_right_after_it() {
    let f = Fixture::with_git();
    f.write(".gitignore", "target/\n");
    f.write("src/main.rs", "fn main() {}");
    f.index_with_rescan_cooldown(Duration::from_secs(24 * 60 * 60));

    // A watcher's rescan runs; explicit requests made meanwhile wait for no interval.
    let hold = || std::thread::sleep(Duration::from_millis(300));
    crate::scan::tests::on(&f.base, crate::scan::tests::Stage::Walking, hold);
    let reason = RescanReason::OverflowCapacity;
    let _ = f
        .picker
        .trigger_full_rescan_with_reason(&f.frecency, reason);
    for _ in 0..3 {
        f.picker.trigger_full_rescan_async(&f.frecency).unwrap();
    }
    let status = f
        .picker
        .read()
        .unwrap()
        .as_ref()
        .unwrap()
        .background_status();
    let deadline = Instant::now() + Duration::from_secs(10);
    while f.all_rescans().count(RescanReason::Explicit) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(f.picker.wait_for_indexing_complete(Duration::from_secs(10)));
    while status.is_scan_pending() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    let stats = f.all_rescans();
    assert_eq!(
        stats.count(RescanReason::Explicit),
        1,
        "explicit requests join one rescan right after the running one, got {stats}"
    );
    assert_eq!(stats.count_throttled(RescanReason::Explicit), 2);
}

#[test]
fn every_full_rescan_tells_subscribers_once_its_index_is_in_place() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();
    let events = f.subscribe("**");
    // A scan runs, an explicit rescan waits behind it, and the watcher asks for one too
    // because it could not take changes in.
    f.picker.trigger_full_rescan_async(&f.frecency).unwrap();
    f.picker.trigger_full_rescan_async(&f.frecency).unwrap();
    let reason = RescanReason::OverflowCapacity;
    let started = f
        .picker
        .trigger_full_rescan_with_reason(&f.frecency, reason);
    assert!(!started.unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    while f.all_rescans().total < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(f.picker.wait_for_indexing_complete(Duration::from_secs(10)));
    f.picker.watch_registry().wait_for_dispatch();
    let stats = f.all_rescans();
    assert_eq!(stats.total, 2, "{stats}");
    // The second rescan counts for both reasons that asked for it.
    assert_eq!(stats.count(RescanReason::Explicit), 2, "{stats}");
    assert_eq!(stats.count(RescanReason::OverflowCapacity), 1, "{stats}");
    assert_eq!(stats.watcher_triggered(), 1, "{stats}");
    // Explicit or not, each rescan is heard once, with its index published.
    let rescans = (events.try_iter().flatten())
        .filter(|event| event.kind == WatchEventKind::Rescan)
        .count();
    assert_eq!(rescans, 2, "stats {stats}");
}

#[test]
fn lost_events_for_a_file_stat_it_again_and_keep_the_rest_of_the_batch() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index();

    f.write("src/main.rs", "fn main() { edited(); }");
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

    f.assert_no_rescan(&delta, "lost events for a single tracked file");
    assert_eq!(f.indexed_size("src/main.rs"), Some(23));
    assert!(
        f.is_indexed("src/added.rs"),
        "the rest of the batch is applied"
    );
}

#[test]
fn a_recreated_ignore_file_is_indexed_and_its_rules_apply() {
    let f = Fixture::with_git();
    f.write(".gitignore", "target/\n");
    f.write("src/main.rs", "fn main() {}");
    f.index();

    f.remove(".gitignore");
    let delta = f.feed([remove_file(f.path(".gitignore"))]);
    f.assert_no_rescan(&delta, "removing the root ignore file");

    f.write(".gitignore", "target/\nsrc/\n");
    let delta = f.feed([create(f.path(".gitignore"))]);
    f.assert_no_rescan(&delta, "creating the root ignore file");
    assert!(f.is_indexed(".gitignore"));
    assert!(!f.is_indexed("src/main.rs"));
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
fn identical_files_renamed_at_once_pair_by_their_inodes() {
    let f = Fixture::new();
    f.write("src/a.rs", "same");
    f.write("src/b.rs", "same");
    // Identical mtimes make size and time ambiguous even if the writes straddle a second.
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

    std::fs::rename(f.path("src/a.rs"), f.path("src/x.rs")).unwrap();
    std::fs::rename(f.path("src/b.rs"), f.path("src/y.rs")).unwrap();
    f.feed([
        remove_file(f.path("src/a.rs")),
        remove_file(f.path("src/b.rs")),
        create(f.path("src/x.rs")),
        create(f.path("src/y.rs")),
    ]);

    let mut received = events.recv_timeout(Duration::from_secs(1)).unwrap();
    received.sort_by(|a, b| a.path.cmp(&b.path));
    if cfg!(unix) {
        let renamed: Vec<_> = (received.iter())
            .map(|e| (e.kind, e.path.clone(), e.from.clone()))
            .collect();
        assert_eq!(
            renamed,
            [
                (
                    WatchEventKind::Renamed,
                    f.path("src/x.rs"),
                    Some(f.path("src/a.rs"))
                ),
                (
                    WatchEventKind::Renamed,
                    f.path("src/y.rs"),
                    Some(f.path("src/b.rs"))
                ),
            ]
        );
    } else {
        // Without inodes nothing can be paired safely.
        assert!(received.iter().all(|e| e.kind != WatchEventKind::Renamed));
        assert_eq!(received.len(), 4, "{received:?}");
    }
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

    let delta = f.feed([
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
    assert!(!f.is_indexed("dropped.rs"));
    assert!(f.indexed_dirs().contains(&"nested/".to_owned()));
    f.assert_no_rescan(&delta, "replacing the base directory");

    let mut received = events.recv_timeout(Duration::from_secs(1)).unwrap();
    received.sort_by(|a, b| a.path.cmp(&b.path));
    let received: Vec<_> = received.into_iter().map(|e| (e.path, e.kind)).collect();
    // The kept files are copies, other inodes of the same size and time.
    let mut expected = vec![
        (f.path("added.rs"), WatchEventKind::Created),
        (f.path("dropped.rs"), WatchEventKind::Removed),
    ];
    if cfg!(unix) {
        expected.push((f.path("kept.rs"), WatchEventKind::Modified));
        expected.push((f.path("nested/deep.rs"), WatchEventKind::Modified));
    }
    assert_eq!(received, expected);
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

    let delta = f.feed([
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

    let delta = f.feed(batch(&f));

    for rel in ["d/kept.rs", "d/inner/deep.rs", "d/new_only.rs"] {
        assert!(f.is_indexed(rel), "{rel} must stay searchable");
    }
    assert!(
        !f.is_indexed("d/old_only.rs"),
        "a file the new d lacks lingers"
    );
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
        let delta = f.feed([
            event(EventKind::Remove(first), f.path("f")),
            event(EventKind::Create(CreateKind::Folder), f.path("f")),
        ]);

        assert!(!f.is_indexed("f"), "{first:?}: the replaced file lingers");
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
    let delta = f.feed(
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
        f.feed([
            event(EventKind::Remove(RemoveKind::Other), f.path("link")),
            event(EventKind::Create(CreateKind::Other), f.path("link")),
        ]);

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

    assert_eq!(delta.count(RescanReason::OverflowCapacity), 1, "{delta}");
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
    let mut expected = vec![
        (f.path("sub/added.rs"), WatchEventKind::Created),
        (f.path("sub/dropped.rs"), WatchEventKind::Removed),
        (f.path("sub/edited.rs"), WatchEventKind::Modified),
    ];
    // A copy that kept size and time is another inode.
    if cfg!(unix) {
        expected.push((f.path("sub/kept.rs"), WatchEventKind::Modified));
    }
    assert_eq!(received, expected);
}

// A scan that already read a recreated directory leaves its walk nothing to change; the
// files the batch's own events name are still changes subscribers hear of.
#[test]
fn a_file_named_in_a_replaced_directory_is_reported_although_the_index_has_it() {
    let f = Fixture::new();
    f.write("sub/named.rs", "fn named() {}");
    f.write("sub/unnamed.rs", "fn unnamed() {}");
    f.index();
    let events = f.subscribe("");

    f.feed([
        event(EventKind::Create(CreateKind::Folder), f.path("sub")),
        create(f.path("sub/named.rs")),
    ]);

    let received: Vec<_> = events
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .into_iter()
        .map(|e| (e.path, e.kind))
        .collect();
    assert_eq!(
        received,
        vec![(f.path("sub/named.rs"), WatchEventKind::Modified)]
    );
}

#[test]
fn a_new_directory_repeated_in_a_batch_is_walked_once() {
    let f = Fixture::new();
    f.index();
    f.write("new/a.rs", "fn a() {}");
    let visits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&visits);
    let new = f.path("new");
    let hook: crate::walk::DirHook = Arc::new(move |dir: &std::path::Path| {
        if dir == new {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });
    let walk = |events: Vec<DebouncedEvent>| {
        visits.store(0, std::sync::atomic::Ordering::SeqCst);
        let options = f.options();
        super::apply_events(&f.picker, &options, None, Some(Arc::clone(&hook)), events);
        visits.load(std::sync::atomic::Ordering::SeqCst)
    };
    let created = || event(EventKind::Create(CreateKind::Folder), f.path("new"));
    let renamed = || {
        event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            f.path("new"),
        )
    };
    let once = walk(vec![created()]);
    assert!(once > 0);
    assert_eq!(walk(vec![created(), renamed(), created()]), once);
    assert!(f.is_indexed("new/a.rs"));
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
    f.feed([event(
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::Permissions)),
        f.path("d/locked"),
    )]);
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
    let renamed = event(
        EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
        f.path("d"),
    );
    let rescan = super::apply_events(&f.picker, &f.options(), None, Some(hook), vec![renamed]);
    assert_eq!(rescan, None, "a vanished directory does not fail the walk");
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
    f.feed([event(EventKind::Create(CreateKind::Folder), f.path("new"))]);

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

// More paths than one batch takes, so the batch goes through the changed-path set.
fn big_batch(f: &Fixture, mut events: Vec<DebouncedEvent>) -> RescanStats {
    let rel = |i: usize| format!("pkg{}/m{}.rs", i / 100, i % 100);
    events.extend((0..=MAX_OVERFLOW_FILES * 4).map(|i| modify(f.path(&rel(i)))));
    f.feed(events)
}

fn indexed_pkgs(f: &Fixture) {
    for i in 0..=MAX_OVERFLOW_FILES * 4 {
        f.write(&format!("pkg{}/m{}.rs", i / 100, i % 100), "");
    }
}

#[test]
fn a_file_replaced_by_a_directory_in_a_big_batch_leaves_the_index() {
    let f = Fixture::new();
    f.write("foo", "x");
    indexed_pkgs(&f);
    f.index();
    f.remove("foo");
    f.write("foo/a.rs", "fn a() {}");
    let delta = big_batch(
        &f,
        vec![
            event(EventKind::Remove(RemoveKind::File), f.path("foo")),
            event(EventKind::Create(CreateKind::Folder), f.path("foo")),
            create(f.path("foo/a.rs")),
        ],
    );
    f.assert_no_rescan(&delta, "a big batch");
    assert!(f.is_indexed("foo/a.rs"));
    assert!(!f.is_indexed("foo"), "the replaced file stays indexed");
}

#[test]
fn a_directory_replaced_by_a_file_in_a_big_batch_leaves_the_index() {
    let f = Fixture::new();
    f.write("foo/a.rs", "fn a() {}");
    f.write("foo/inner/b.rs", "fn b() {}");
    indexed_pkgs(&f);
    f.index();
    std::fs::remove_dir_all(f.path("foo")).unwrap();
    f.write("foo", "now a file");
    let delta = big_batch(
        &f,
        vec![
            event(EventKind::Remove(RemoveKind::Folder), f.path("foo")),
            create(f.path("foo")),
        ],
    );
    f.assert_no_rescan(&delta, "a big batch");
    assert!(f.is_indexed("foo"));
    assert!(!f.is_indexed("foo/a.rs"));
    assert!(!f.is_indexed("foo/inner/b.rs"));
}

#[test]
fn a_file_replaced_by_a_directory_while_a_set_records_it_leaves_the_index() {
    let f = Fixture::new();
    f.write("foo", "x");
    f.index();
    f.remove("foo");
    f.write("foo/a.rs", "fn a() {}");
    let mut changed = super::changed_paths::ChangedPaths::new(&f.base);
    changed.add([super::changed_paths::Change::Path {
        path: f.path("foo"),
        recursive: false,
    }]);
    super::reconcile::reconcile(&f.picker, &f.frecency, changed, true, None);
    assert!(f.is_indexed("foo/a.rs"));
    assert!(!f.is_indexed("foo"), "the replaced file stays indexed");
}

struct Fixture {
    base: PathBuf,
    picker: SharedFilePicker,
    frecency: SharedFrecency,
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
        if git {
            let status = Command::new("git")
                .args(["init", "-b", "main"])
                .current_dir(&base)
                .output()
                .expect("git init");
            assert!(status.status.success(), "git init failed");
        }

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

    // Hands a batch to the index's loop as its watcher would, and waits until it is applied.
    fn feed(&self, events: impl IntoIterator<Item = DebouncedEvent>) -> RescanStats {
        let before = self.picker.rescan_stats();
        let events = events.into_iter().collect();
        assert!(self.picker.feed(Some(self.frecency.clone()), events));
        self.picker.rescan_stats().since(&before)
    }

    fn options(&self) -> super::WatcherOptions {
        super::WatcherOptions::of(self.picker.read().unwrap().as_ref().unwrap())
    }

    fn files_containing(&self, text: &str) -> Vec<String> {
        let guard = self.picker.read().unwrap();
        let picker = guard.as_ref().unwrap();
        let query = crate::grep::parse_grep_query(text);
        let result = picker.grep(&query, &crate::grep::GrepSearchOptions::default());
        (result.files.iter())
            .map(|file| file.relative_path(picker))
            .collect()
    }

    fn set_mtime(&self, rel: &str, mtime: std::time::SystemTime) {
        let file = std::fs::File::options().write(true).open(self.path(rel));
        file.unwrap().set_modified(mtime).unwrap();
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

    fn indexed_size(&self, rel: &str) -> Option<u64> {
        let guard = self.picker.read().unwrap();
        guard
            .as_ref()
            .and_then(|p| p.get_file_by_path(self.path(rel)))
            .filter(|file| !file.is_deleted())
            .map(|file| file.size)
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

// What a watcher reports when it lost events, for a path or for none.
fn lost(path: Option<PathBuf>) -> DebouncedEvent {
    let event = Event::new(EventKind::Other).set_flag(Flag::Rescan);
    DebouncedEvent::new(
        path.into_iter()
            .fold(event, |event, path| event.add_path(path)),
        Instant::now(),
    )
}

fn remove_file(path: PathBuf) -> DebouncedEvent {
    event(EventKind::Remove(RemoveKind::File), path)
}

// A directory new to the index with no ignore file of its own: the rules above decide alike
// for its walk and for later events in it.
fn a_new_directory_without_rules_of_its_own_follows_the_rules_above(f: &Fixture, name: &str) {
    f.write(name, "gen/\n/pkg2/out/\n");
    f.write(&format!("a/{name}"), "*.log\n");
    f.write("a/main.rs", "fn main() {}");
    f.write("src/main.rs", "fn main() {}");
    f.index();
    for (dir, file) in [("pkg2", "pkg2/main.rs"), ("a/new", "a/new/main.rs")] {
        f.write(file, "fn main() {}");
        f.feed([event(EventKind::Create(CreateKind::Folder), f.path(dir))]);
        assert!(f.is_indexed(file), "{file}");
    }
    for rel in [
        "pkg2/gen/x.rs",
        "pkg2/out/y.rs",
        "a/new/z.log",
        "a/new/gen/w.rs",
    ] {
        f.write(rel, "x");
        f.feed([create(f.path(rel))]);
        assert!(!f.is_indexed(rel), "{rel} is ignored by the rules above");
    }
}

#[test]
fn a_new_directory_without_rules_of_its_own_follows_the_rules_above_in_a_repository() {
    a_new_directory_without_rules_of_its_own_follows_the_rules_above(
        &Fixture::with_git(),
        ".gitignore",
    );
}

#[test]
fn a_new_directory_without_rules_of_its_own_follows_the_rules_above_outside_git() {
    a_new_directory_without_rules_of_its_own_follows_the_rules_above(&Fixture::new(), ".ignore");
}

// `mkdir pkg && mv src/old.rs pkg/` in one batch: the new directory's walk and the removal
// are one delta, so the move is one rename whether the backend paired it or not.
fn moving_a_file_into_a_new_directory_is_one_rename(paired: bool) {
    let f = Fixture::new();
    f.write("src/old.rs", "fn old() {}");
    f.write("src/main.rs", "fn main() {}");
    f.index();
    let events = f.subscribe("**");
    std::fs::create_dir(f.path("pkg")).unwrap();
    std::fs::rename(f.path("src/old.rs"), f.path("pkg/old.rs")).unwrap();
    let mut batch = vec![event(EventKind::Create(CreateKind::Folder), f.path("pkg"))];
    if paired {
        batch.push(DebouncedEvent::new(
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(f.path("src/old.rs"))
                .add_path(f.path("pkg/old.rs")),
            Instant::now(),
        ));
    } else {
        batch.push(remove_file(f.path("src/old.rs")));
        batch.push(create(f.path("pkg/old.rs")));
    }
    f.feed(batch);
    f.picker.watch_registry().wait_for_dispatch();
    let received: Vec<WatchEvent> = events.try_iter().flatten().collect();
    assert!(f.is_indexed("pkg/old.rs"));
    let renamed: Vec<_> = (received.iter())
        .map(|e| (e.kind, e.path.clone(), e.from.clone()))
        .collect();
    assert_eq!(
        renamed,
        [(
            WatchEventKind::Renamed,
            f.path("pkg/old.rs"),
            Some(f.path("src/old.rs"))
        )]
    );
}

#[test]
fn moving_a_file_into_a_new_directory_is_one_rename_by_identity() {
    moving_a_file_into_a_new_directory_is_one_rename(false);
}

#[test]
fn moving_a_file_into_a_new_directory_is_one_rename_as_the_backend_paired_it() {
    moving_a_file_into_a_new_directory_is_one_rename(true);
}

// A polling pass saw `pkg` before `mv src/old.rs pkg/`, and the next one names the move: the
// first batch's walk of the directory creates the file, and the second removes its source.
#[test]
fn a_move_split_across_batches_creates_the_file_before_its_source_is_removed() {
    let f = Fixture::new();
    f.write("src/old.rs", "fn old() {}");
    f.write("src/main.rs", "fn main() {}");
    f.index();
    let events = f.subscribe("**");
    std::fs::create_dir(f.path("pkg")).unwrap();
    std::fs::rename(f.path("src/old.rs"), f.path("pkg/old.rs")).unwrap();
    f.feed([event(EventKind::Create(CreateKind::Folder), f.path("pkg"))]);
    f.feed([
        remove_file(f.path("src/old.rs")),
        create(f.path("pkg/old.rs")),
    ]);
    f.picker.watch_registry().wait_for_dispatch();
    let received: Vec<_> = (events.try_iter().enumerate())
        .flat_map(|(batch, events)| events.into_iter().map(move |e| (batch, e.kind, e.path)))
        .collect();
    assert!(f.is_indexed("pkg/old.rs"));
    assert_eq!(
        received,
        [
            (0, WatchEventKind::Created, f.path("pkg/old.rs")),
            (1, WatchEventKind::Removed, f.path("src/old.rs")),
            // A named file is a change whatever its stat says.
            (1, WatchEventKind::Modified, f.path("pkg/old.rs")),
        ]
    );
}

// A tool keeping mtimes (rsync -a, cp -p) replaces a file with same-size content, its event lost:
// a reconcile finds it by inode and ctime, as Git's stat data does.
#[cfg(unix)]
#[test]
fn a_reconcile_reports_a_same_size_rewrite_that_kept_its_mtime() {
    let f = Fixture::new();
    f.write("src/a.rs", "fn alpha() {}");
    f.write("src/main.rs", "fn main() {}");
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    f.set_mtime("src/a.rs", old);
    f.index();
    let events = f.subscribe("**");
    f.write("tmp.rs", "fn gamma() {}");
    f.set_mtime("tmp.rs", old);
    std::fs::rename(f.path("tmp.rs"), f.path("src/a.rs")).unwrap();
    f.feed([lost(Some(f.path("src")))]);
    assert_eq!(
        f.files_containing("gamma"),
        ["src/a.rs"],
        "the index read it again"
    );
    f.picker.watch_registry().wait_for_dispatch();
    let received: Vec<WatchEvent> = events.try_iter().flatten().collect();
    assert!(
        (received.iter())
            .any(|e| e.path == f.path("src/a.rs") && e.kind == WatchEventKind::Modified),
        "subscribers hear of the new content: {received:?}"
    );
}

// Rules that leave a directory in only as a directory: a directory moved in or created, which
// an event may name as a file, keeps its files.
fn a_directory_the_rules_keep_only_as_a_directory_is_indexed(
    f: &Fixture,
    rules: &str,
    kind: EventKind,
) {
    f.write(rules, "*\n!*/\n!*.rs\n!.gitignore\n!.ignore\n");
    f.write("src/main.rs", "fn main() {}");
    f.index();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(outside.path().join("pkg/inner")).unwrap();
    std::fs::write(outside.path().join("pkg/main.rs"), "fn main() {}").unwrap();
    std::fs::write(outside.path().join("pkg/inner/lib.rs"), "fn lib() {}").unwrap();
    std::fs::rename(outside.path().join("pkg"), f.path("pkg")).unwrap();
    f.feed([event(kind, f.path("pkg"))]);
    assert!(f.is_indexed("pkg/main.rs"), "{kind:?}");
    assert!(f.is_indexed("pkg/inner/lib.rs"), "{kind:?}");
}

#[test]
fn a_directory_moved_in_as_fsevents_names_it_is_indexed() {
    let renamed = EventKind::Modify(ModifyKind::Name(RenameMode::Any));
    a_directory_the_rules_keep_only_as_a_directory_is_indexed(&Fixture::new(), ".ignore", renamed);
    a_directory_the_rules_keep_only_as_a_directory_is_indexed(
        &Fixture::with_git(),
        ".gitignore",
        renamed,
    );
}

#[test]
fn a_directory_moved_in_as_inotify_names_it_is_indexed() {
    let renamed = EventKind::Modify(ModifyKind::Name(RenameMode::To));
    a_directory_the_rules_keep_only_as_a_directory_is_indexed(&Fixture::new(), ".ignore", renamed);
    a_directory_the_rules_keep_only_as_a_directory_is_indexed(
        &Fixture::with_git(),
        ".gitignore",
        renamed,
    );
}

#[test]
fn a_directory_created_as_polling_names_it_is_indexed() {
    let created = EventKind::Create(CreateKind::Any);
    a_directory_the_rules_keep_only_as_a_directory_is_indexed(&Fixture::new(), ".ignore", created);
    a_directory_the_rules_keep_only_as_a_directory_is_indexed(
        &Fixture::with_git(),
        ".gitignore",
        created,
    );
}

// The paths intake keeps of a batch, before any reconcile.
fn kept_paths(f: &Fixture, events: &[DebouncedEvent]) -> Vec<PathBuf> {
    use super::changed_paths::Change;
    let workdir = (f.picker.read().unwrap().as_ref().unwrap().git_root()).map(PathBuf::from);
    let repo = workdir.and_then(|workdir| git2::Repository::open(workdir).ok());
    (super::intake::intake(&f.picker, events, repo.as_ref()).into_iter())
        .flat_map(|change| match change {
            Change::Path { path, .. } | Change::Lost(path) | Change::IgnoreFile(path) => vec![path],
            Change::Renamed(from, to) => vec![from, to],
            Change::Root | Change::Git(_) | Change::LostGit(_) => Vec::new(),
        })
        .collect()
}

// Intake keeps nothing strictly below a directory the rules ignore, however its paths are
// named; the directory itself, and what lies outside it, the reconcile judges.
fn intake_keeps_nothing_below_an_ignored_directory(f: &Fixture, rules: &str) {
    f.write(rules, "target/\n");
    f.write("src/main.rs", "fn main() {}");
    f.write("target/debug/a.o", "x");
    f.index();
    f.write("target/debug/new.o", "x");
    f.write("target/x/y/z.o", "x");
    f.write("src/new.rs", "fn new() {}");
    let events = [
        create(f.path("target/debug/new.o")),
        modify(f.path("target/debug/a.o")),
        remove_file(f.path("target/debug/gone.o")),
        event(EventKind::Create(CreateKind::Folder), f.path("target/x")),
        create(f.path("target/x/y/z.o")),
        event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            f.path("target/x/y"),
        ),
        lost(Some(f.path("target/debug"))),
        event(EventKind::Create(CreateKind::Folder), f.path("target")),
        create(f.path("src/new.rs")),
    ];
    let kept = kept_paths(f, &events);
    let target = f.path("target");
    let below: Vec<_> = (kept.iter())
        .filter(|path| path.starts_with(&target) && **path != target)
        .collect();
    assert!(
        below.is_empty(),
        "kept below an ignored directory: {below:?}"
    );
    assert!(kept.contains(&f.path("src/new.rs")), "{kept:?}");
}

#[test]
fn intake_keeps_nothing_below_an_ignored_directory_git() {
    intake_keeps_nothing_below_an_ignored_directory(&Fixture::with_git(), ".gitignore");
}

#[test]
fn intake_keeps_nothing_below_an_ignored_directory_non_git() {
    intake_keeps_nothing_below_an_ignored_directory(&Fixture::new(), ".ignore");
}

// Lost events naming a directory the rules ignore are dropped as any change there: nothing is
// walked and no recrawl is logged, and the directory's rules stay the scan's.
fn lost_events_in_an_ignored_directory_are_dropped(f: &Fixture, rules: &str) {
    f.write(rules, "node_modules/\n");
    f.write("src/main.rs", "fn main() {}");
    f.write("node_modules/pkg/index.js", "x");
    f.index();
    f.write("node_modules/pkg/new.js", "x");
    f.write("src/unreported.rs", "fn unreported() {}");
    let delta = f.feed([lost(Some(f.path("node_modules")))]);
    f.assert_no_rescan(&delta, "lost events naming an ignored directory");
    assert_eq!(super::intake::recrawls_logged(&f.path("node_modules")), 0);
    assert!(
        !f.is_indexed("src/unreported.rs"),
        "nothing outside is walked"
    );
    assert!(!f.is_indexed("node_modules/pkg/new.js"));
    f.write("node_modules/pkg2/a/b.js", "x");
    f.feed([
        event(
            EventKind::Create(CreateKind::Folder),
            f.path("node_modules/pkg2"),
        ),
        lost(Some(f.path("node_modules/pkg"))),
    ]);
    assert!(!f.is_indexed("node_modules/pkg2/a/b.js"));
}

#[test]
fn lost_events_in_an_ignored_directory_are_dropped_git() {
    lost_events_in_an_ignored_directory_are_dropped(&Fixture::with_git(), ".gitignore");
}

#[test]
fn lost_events_in_an_ignored_directory_are_dropped_non_git() {
    lost_events_in_an_ignored_directory_are_dropped(&Fixture::new(), ".ignore");
}

// Past the capacity an edit of a file the index holds, in the same batch, is still applied and
// reported: only the new files wait for the rescan.
#[test]
fn an_edit_beside_new_files_past_the_capacity_is_applied() {
    let f = Fixture::new();
    f.write("src/main.rs", "fn main() {}");
    f.index_with_rescan_cooldown(Duration::from_secs(24 * 60 * 60));
    f.picker.trigger_full_rescan_async(&f.frecency).unwrap();
    assert!(f.picker.wait_for_indexing_complete(Duration::from_secs(10)));
    let events = f.subscribe("src/**");
    let edited = "fn main_changed_and_longer() {}";
    f.write("src/main.rs", edited);
    let mut batch = vec![modify(f.path("src/main.rs"))];
    for i in 0..=MAX_OVERFLOW_FILES {
        f.write(&format!("new/file{i}.rs"), "");
    }
    batch.push(event(EventKind::Create(CreateKind::Folder), f.path("new")));
    let delta = f.feed(batch);
    assert_eq!(delta.total, 0, "the rescan waits for its interval: {delta}");
    assert_eq!(f.indexed_size("src/main.rs"), Some(edited.len() as u64));
    assert!(!f.is_indexed("new/file0.rs"));
    f.picker.watch_registry().wait_for_dispatch();
    let reported: Vec<WatchEvent> = events.try_iter().flatten().collect();
    assert!(
        (reported.iter())
            .any(|e| e.kind == WatchEventKind::Modified && e.path == f.path("src/main.rs")),
        "{reported:?}"
    );
}

// A file swapped with a FIFO between its stat and its read never blocks the loop: files are
// opened first and read only when the open handle is a regular file.
#[cfg(unix)]
#[test]
fn a_fifo_swapped_in_after_a_stat_never_blocks_the_reader() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("swap.rs");
    let (fifo, file) = (dir.path().join("fifo-tmp"), dir.path().join("file-tmp"));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let swapper = {
        let (stop, target, fifo, file) = (stop.clone(), target.clone(), fifo.clone(), file.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                std::fs::write(&file, "fn swap() {}\n").unwrap();
                let _ = std::fs::rename(&file, &target);
                let _ = Command::new("mkfifo").arg(&fifo).status();
                let _ = std::fs::rename(&fifo, &target);
            }
        })
    };
    let (sender, read) = mpsc::channel();
    let reader_target = target.clone();
    std::thread::spawn(move || {
        for i in 0..2000 {
            let _ = std::fs::metadata(&reader_target);
            let _ = crate::stat::read_regular(&reader_target);
            let _ = sender.send(i);
        }
    });
    let mut stuck = false;
    loop {
        match read.recv_timeout(Duration::from_secs(2)) {
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                stuck = true;
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    swapper.join().unwrap();
    if stuck {
        use std::os::unix::fs::OpenOptionsExt;
        let mut writer = std::fs::OpenOptions::new();
        drop(
            writer
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&target),
        );
    }
    assert!(
        !stuck,
        "a FIFO swapped in after the stat was opened for reading"
    );
}
