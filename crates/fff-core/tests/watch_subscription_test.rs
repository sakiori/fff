use fff_search::file_picker::{FFFMode, FilePicker};
use fff_search::{
    FilePickerOptions, SharedFilePicker, SharedFrecency, WatchEvent, WatchEventKind, WatchOptions,
};
use parking_lot::Mutex;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn make_watched_picker(base: &Path) -> (SharedFilePicker, SharedFrecency) {
    let shared_picker = SharedFilePicker::default();
    let shared_frecency = SharedFrecency::noop();

    FilePicker::new_with_shared_state(
        shared_picker.clone(),
        shared_frecency.clone(),
        FilePickerOptions {
            base_path: base.to_string_lossy().into_owned(),
            enable_mmap_cache: false,
            enable_content_indexing: false,
            mode: FFFMode::Neovim,
            watch: true,
            ..Default::default()
        },
    )
    .expect("FilePicker::new_with_shared_state");

    assert!(
        shared_picker.wait_for_scan(Duration::from_secs(30)),
        "initial scan did not complete"
    );
    assert!(
        shared_picker.wait_for_watcher(Duration::from_secs(30)),
        "watcher did not install"
    );
    // macOS FSEvents streams need a beat before they deliver reliably
    std::thread::sleep(Duration::from_millis(300));

    (shared_picker, shared_frecency)
}

fn wait_for<F: Fn() -> bool>(cond: F, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    cond()
}

fn seed(base: &Path) {
    fs::create_dir_all(base.join("src")).unwrap();
    fs::write(base.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(base.join("README.md"), "# seed\n").unwrap();
}

type Collected = Arc<Mutex<Vec<WatchEvent>>>;

/// Subscribe with a collector callback; returns the shared event sink.
fn watch_collect(picker: &SharedFilePicker, pattern: &str, options: WatchOptions) -> Collected {
    let collected: Collected = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&collected);
    picker
        .watch(pattern, options, move |_id, events| {
            sink.lock().extend_from_slice(events)
        })
        .expect("watch subscription failed");
    collected
}

#[test]
fn glob_subscription_receives_created_and_removed_events() {
    let tmp = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    seed(&base);
    let (picker, _frecency) = make_watched_picker(&base);

    let events: Arc<Mutex<Vec<WatchEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));

    let events_cb = Arc::clone(&events);
    let calls_cb = Arc::clone(&calls);
    let id = picker
        .watch("**/*.rs", WatchOptions::default(), move |_id, batch| {
            calls_cb.fetch_add(1, Ordering::SeqCst);
            events_cb.lock().extend_from_slice(batch);
        })
        .expect("subscribe glob");

    let rs_file = base.join("src/new_module.rs");
    let ts_file = base.join("src/ignored_by_glob.ts");
    fs::write(&rs_file, "pub fn hi() {}\n").unwrap();
    fs::write(&ts_file, "export {};\n").unwrap();

    assert!(
        wait_for(
            || events.lock().iter().any(|e| e.path == rs_file),
            Duration::from_secs(10)
        ),
        "did not receive event for created .rs file, got: {:?}",
        events.lock()
    );
    assert!(
        !events.lock().iter().any(|e| e.path == ts_file),
        ".ts file must not match the *.rs glob"
    );

    fs::remove_file(&rs_file).unwrap();
    assert!(
        wait_for(
            || events
                .lock()
                .iter()
                .any(|e| e.path == rs_file && e.kind == WatchEventKind::Removed),
            Duration::from_secs(10)
        ),
        "did not receive Removed event, got: {:?}",
        events.lock()
    );

    // batching: each debounce window is one callback invocation, so the call
    // count must be well below the delivered event count + noise ceiling
    assert!(calls.load(Ordering::SeqCst) <= events.lock().len() + 2);

    assert!(picker.unwatch(id));
    let count_after = events.lock().len();
    fs::write(base.join("src/after_unsub.rs"), "\n").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        events.lock().len(),
        count_after,
        "no events after unsubscribe"
    );
}

#[test]
fn watch_events_reflect_applied_file_transitions() {
    let tmp = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    let removed_path = base.join("removed.txt");
    let created_path = base.join("created.txt");
    let replaced_path = base.join("replaced.txt");
    fs::write(&removed_path, "remove me").unwrap();
    fs::write(&replaced_path, "before").unwrap();

    let (picker, _frecency) = make_watched_picker(&base);
    let removed = watch_collect(
        &picker,
        removed_path.to_str().unwrap(),
        WatchOptions::default(),
    );
    let created = watch_collect(
        &picker,
        created_path.to_str().unwrap(),
        WatchOptions::default(),
    );
    let replaced = watch_collect(
        &picker,
        replaced_path.to_str().unwrap(),
        WatchOptions::default(),
    );

    let barrier_path = base.join("barrier.txt");
    let barrier = watch_collect(
        &picker,
        barrier_path.to_str().unwrap(),
        WatchOptions::default(),
    );

    // The watcher may split one change across batches; each event must still be
    // a transition of the index, and the events must end in the on-disk state.
    fs::remove_file(&removed_path).unwrap();
    assert!(
        wait_for(
            || !presence_after(true, &removed.lock()),
            Duration::from_secs(10)
        ),
        "remove event was not delivered"
    );

    fs::write(&created_path, "created").unwrap();
    assert!(
        wait_for(
            || presence_after(false, &created.lock()),
            Duration::from_secs(10)
        ),
        "create event was not delivered"
    );

    fs::remove_file(&replaced_path).unwrap();
    fs::write(&replaced_path, "after").unwrap();
    assert!(
        wait_for(
            || {
                let replaced = replaced.lock();
                !replaced.is_empty() && presence_after(true, &replaced)
            },
            Duration::from_secs(10)
        ),
        "replacement event was not delivered"
    );

    // Events are delivered in order, so the barrier's event follows every earlier one.
    fs::write(&barrier_path, "barrier").unwrap();
    assert!(
        wait_for(|| !barrier.lock().is_empty(), Duration::from_secs(10)),
        "barrier event was not delivered"
    );

    for (path, events, initial, present) in [
        (&removed_path, &removed, true, false),
        (&created_path, &created, false, true),
        (&replaced_path, &replaced, true, true),
    ] {
        let events = events.lock();
        assert!(events.iter().all(|event| event.path == *path), "{events:?}");
        assert_eq!(presence_after(initial, &events), present, "{events:?}");
    }
}

#[test]
fn removed_directory_delivers_removed_event_per_file() {
    let tmp = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    seed(&base);
    let dir = base.join("doomed");
    fs::create_dir_all(dir.join("nested")).unwrap();
    let files = [dir.join("a.rs"), dir.join("b.txt"), dir.join("nested/c.rs")];
    for f in &files {
        fs::write(f, "content\n").unwrap();
    }

    let (picker, _frecency) = make_watched_picker(&base);
    let events = watch_collect(&picker, "", WatchOptions::default());

    fs::remove_dir_all(&dir).unwrap();

    assert!(
        wait_for(
            || {
                let got = events.lock();
                files.iter().all(|f| {
                    got.iter()
                        .any(|e| e.path == *f && e.kind == WatchEventKind::Removed)
                })
            },
            Duration::from_secs(10)
        ),
        "expected Removed for every file in the removed dir, got: {:?}",
        events.lock()
    );
}

#[test]
fn moved_out_directory_delivers_removed_event_per_file() {
    let tmp = TempDir::new().unwrap();
    let trash = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    seed(&base);
    let dir = base.join("doomed");
    fs::create_dir_all(dir.join("nested")).unwrap();
    let files = [dir.join("a.rs"), dir.join("b.txt"), dir.join("nested/c.rs")];
    for f in &files {
        fs::write(f, "content\n").unwrap();
    }

    let (picker, _frecency) = make_watched_picker(&base);
    let events = watch_collect(&picker, "", WatchOptions::default());

    // mimics `mv dir elsewhere` / Finder trash: one rename event on the dir,
    // no per-file remove events from the OS
    fs::rename(&dir, trash.path().join("doomed")).unwrap();

    assert!(
        wait_for(
            || {
                let got = events.lock();
                files.iter().all(|f| {
                    got.iter()
                        .any(|e| e.path == *f && e.kind == WatchEventKind::Removed)
                })
            },
            Duration::from_secs(10)
        ),
        "expected Removed for every file in the moved-out dir, got: {:?}",
        events.lock()
    );
}

#[test]
fn renaming_a_file_delivers_one_renamed_event_with_both_paths() {
    let tmp = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    seed(&base);
    let old = base.join("src/renamed_from.rs");
    fs::write(&old, "pub fn hi() {}\n").unwrap();

    let (picker, _frecency) = make_watched_picker(&base);
    let events = watch_collect(&picker, "", WatchOptions::default());

    let new = base.join("src/renamed_to.rs");
    fs::rename(&old, &new).unwrap();

    assert!(
        wait_for(
            || events
                .lock()
                .iter()
                .any(|e| e.kind == WatchEventKind::Renamed && e.path == new),
            Duration::from_secs(10)
        ),
        "expected a Renamed event for {}, got: {:?}",
        new.display(),
        events.lock()
    );

    let got = events.lock();
    let renamed = got
        .iter()
        .find(|e| e.kind == WatchEventKind::Renamed)
        .unwrap();
    assert_eq!(renamed.from.as_deref(), Some(old.as_path()));
    assert!(
        !got.iter()
            .any(|e| e.path == old && e.kind == WatchEventKind::Removed),
        "a recognised rename must not also report the source as Removed: {got:?}"
    );
}

#[test]
fn renaming_out_of_a_watched_subtree_still_notifies() {
    let tmp = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    seed(&base);
    fs::create_dir_all(base.join("vendor")).unwrap();
    let inside = base.join("src/leaving.rs");
    fs::write(&inside, "pub fn leaving() {}\n").unwrap();

    let (picker, _frecency) = make_watched_picker(&base);
    // Only `src/` is watched; the destination is outside it.
    let events = watch_collect(&picker, "src", WatchOptions::default());

    let outside = base.join("vendor/leaving.rs");
    fs::rename(&inside, &outside).unwrap();

    assert!(
        wait_for(
            || events
                .lock()
                .iter()
                .any(|e| e.kind == WatchEventKind::Renamed
                    && e.from.as_deref() == Some(inside.as_path())),
            Duration::from_secs(10)
        ),
        "a subscriber on src/ must hear that a file moved out of it, got: {:?}",
        events.lock()
    );
}

#[test]
fn renaming_into_a_watched_subtree_still_notifies() {
    let tmp = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    seed(&base);
    fs::create_dir_all(base.join("vendor")).unwrap();
    let outside = base.join("vendor/arriving.rs");
    fs::write(&outside, "pub fn arriving() {}\n").unwrap();

    let (picker, _frecency) = make_watched_picker(&base);
    let events = watch_collect(&picker, "src", WatchOptions::default());

    let inside = base.join("src/arriving.rs");
    fs::rename(&outside, &inside).unwrap();

    assert!(
        wait_for(
            || events
                .lock()
                .iter()
                .any(|e| e.kind == WatchEventKind::Renamed && e.path == inside),
            Duration::from_secs(10)
        ),
        "a subscriber on src/ must hear that a file moved into it, got: {:?}",
        events.lock()
    );
}

#[test]
fn renaming_across_the_base_boundary_degrades_to_removed() {
    let tmp = TempDir::new().unwrap();
    let outside_root = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    seed(&base);
    let inside = base.join("src/exiting.rs");
    fs::write(&inside, "pub fn exiting() {}\n").unwrap();

    let (picker, _frecency) = make_watched_picker(&base);
    let events = watch_collect(&picker, "", WatchOptions::default());

    // The destination is outside the indexed tree, so there is no pair to
    // report — this stays a plain removal.
    fs::rename(&inside, outside_root.path().join("exiting.rs")).unwrap();

    assert!(
        wait_for(
            || events
                .lock()
                .iter()
                .any(|e| e.path == inside && e.kind == WatchEventKind::Removed),
            Duration::from_secs(10)
        ),
        "expected Removed for a file moved out of the base, got: {:?}",
        events.lock()
    );
    assert!(
        events
            .lock()
            .iter()
            .all(|e| e.kind != WatchEventKind::Renamed),
        "no Renamed may escape with a path outside the base: {:?}",
        events.lock()
    );
}

#[test]
fn empty_pattern_watches_the_whole_tree() {
    let tmp = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    seed(&base);
    let (picker, _frecency) = make_watched_picker(&base);

    let events = watch_collect(
        &picker,
        "",
        WatchOptions {
            ignore: vec!["*.log".to_string()],
        },
    );

    let rs_file = base.join("src/anywhere.rs");
    let txt_file = base.join("notes.txt");
    let log_file = base.join("noise.log");
    fs::write(&rs_file, "\n").unwrap();
    fs::write(&txt_file, "\n").unwrap();
    fs::write(&log_file, "\n").unwrap();

    assert!(
        wait_for(
            || {
                let got = events.lock();
                got.iter().any(|e| e.path == rs_file) && got.iter().any(|e| e.path == txt_file)
            },
            Duration::from_secs(10)
        ),
        "watch-all did not receive events for both files, got: {:?}",
        events.lock()
    );
    // the ignore option still filters within a watch-all subscription
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !events.lock().iter().any(|e| e.path == log_file),
        "*.log must be filtered by the ignore option"
    );
}

#[test]
fn exact_out_of_tree_paths_are_rejected() {
    let tmp = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    seed(&base);

    let outside = TempDir::new().unwrap();
    let outside_file = fff_search::path_utils::canonicalize(outside.path())
        .unwrap()
        .join("config.txt");
    fs::write(&outside_file, "v1").unwrap();

    let (picker, _frecency) = make_watched_picker(&base);

    assert!(
        picker
            .watch(
                outside_file.to_str().unwrap(),
                WatchOptions::default(),
                |_, _| {}
            )
            .is_err(),
        "exact paths outside the indexed tree must be rejected"
    );
}

#[test]
fn gitignored_files_are_never_delivered() {
    let tmp = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    seed(&base);
    fs::create_dir_all(base.join("dist")).unwrap();
    fs::write(base.join(".gitignore"), "dist/\n*.log\n").unwrap();
    git2::Repository::init(&base).unwrap();
    let (picker, _frecency) = make_watched_picker(&base);

    let events = watch_collect(&picker, "", WatchOptions::default());

    fs::write(base.join("dist/bundle.js"), "js").unwrap();
    fs::write(base.join("noise.log"), "log").unwrap();
    fs::write(base.join("visible.txt"), "txt").unwrap();

    assert!(
        wait_for(
            || events
                .lock()
                .iter()
                .any(|e| e.path == base.join("visible.txt")),
            Duration::from_secs(10)
        ),
        "non-ignored file must be delivered, got {:?}",
        events.lock()
    );

    std::thread::sleep(Duration::from_millis(500));
    let collected = events.lock();
    assert!(
        !collected
            .iter()
            .any(|e| e.path == base.join("dist/bundle.js")),
        "gitignored directory content must not be delivered: {:?}",
        collected
    );
    assert!(
        !collected.iter().any(|e| e.path == base.join("noise.log")),
        "gitignored file must not be delivered: {:?}",
        collected
    );
}

#[test]
fn dir_subscription_with_ignore_option() {
    let tmp = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    seed(&base);
    fs::create_dir_all(base.join("src/vendor")).unwrap();
    let (picker, _frecency) = make_watched_picker(&base);

    // parcel-style: subscribe to a directory subtree with excludes
    let got = watch_collect(
        &picker,
        "src",
        WatchOptions {
            ignore: vec!["*.map".to_string(), "src/vendor".to_string()],
        },
    );

    fs::write(base.join("src/feature.rs"), "pub fn f() {}\n").unwrap();
    fs::write(base.join("src/feature.js.map"), "{}\n").unwrap();
    fs::write(base.join("src/vendor/lib.js"), "x\n").unwrap();
    fs::write(base.join("outside_dir.txt"), "not in src\n").unwrap();

    assert!(
        wait_for(
            || got
                .lock()
                .iter()
                .any(|e| e.path == base.join("src/feature.rs")),
            Duration::from_secs(10)
        ),
        "dir subscriber must see files in its subtree, got {:?}",
        got.lock()
    );
    let got = got.lock();
    assert!(
        !got.iter()
            .any(|e| e.path == base.join("src/feature.js.map")),
        "ignore glob leaked: {got:?}"
    );
    assert!(
        !got.iter().any(|e| e.path == base.join("src/vendor/lib.js")),
        "ignore prefix leaked: {got:?}"
    );
    assert!(
        !got.iter().any(|e| e.path == base.join("outside_dir.txt")),
        "event outside the subscribed dir leaked: {got:?}"
    );
}

#[test]
fn shutdown_watches_stops_future_deliveries() {
    let tmp = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    seed(&base);
    let (picker, _frecency) = make_watched_picker(&base);

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_cb = Arc::clone(&calls);
    picker
        .watch("**/*.txt", WatchOptions::default(), move |_, _| {
            calls_cb.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();

    fs::write(base.join("one.txt"), "1\n").unwrap();
    assert!(
        wait_for(|| calls.load(Ordering::SeqCst) > 0, Duration::from_secs(10)),
        "callback never fired before shutdown"
    );

    picker.shutdown_watches();
    let after = calls.load(Ordering::SeqCst);

    fs::write(base.join("two.txt"), "2\n").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        after,
        "callback fired after shutdown_watches returned"
    );
}

#[test]
fn non_canonical_dir_pattern_resolves_into_the_tree() {
    let tmp = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    seed(&base);
    let (picker, _) = make_watched_picker(&base);

    // tmp.path() is the non-canonical spelling (e.g. /var/... symlinked to
    // /private/var/... on macOS, 8.3 short names on Windows); the watch must
    // canonicalize instead of rejecting it
    let events = watch_collect(
        &picker,
        tmp.path().to_str().unwrap(),
        WatchOptions::default(),
    );

    fs::write(base.join("via-alias.txt"), "x\n").unwrap();
    assert!(
        wait_for(
            || events
                .lock()
                .iter()
                .any(|e| e.path == base.join("via-alias.txt")),
            Duration::from_secs(10)
        ),
        "non-canonical base-dir pattern must receive events, got {:?}",
        events.lock()
    );
}

#[test]
fn invalid_patterns_are_rejected() {
    let tmp = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    seed(&base);
    let (picker, _frecency) = make_watched_picker(&base);

    assert!(
        picker
            .watch(
                "/somewhere/else/**/*.rs",
                WatchOptions::default(),
                |_, _| {}
            )
            .is_err(),
        "absolute glob outside base must be rejected"
    );

    // relative exact path resolves against base
    let got = watch_collect(&picker, "README.md", WatchOptions::default());
    fs::write(base.join("README.md"), "# updated\n").unwrap();
    assert!(
        wait_for(
            || got.lock().iter().any(|e| e.path == base.join("README.md")),
            Duration::from_secs(10)
        ),
        "got {:?}",
        got.lock()
    );
}

#[test]
fn ref_listeners_hear_branches_tags_heads_packing_and_fetches() {
    let tmp = TempDir::new().unwrap();
    let root = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    let upstream = root.join("upstream");
    let base = root.join("clone");
    committed_repository(&upstream);
    git(&root, &["clone", "-q", "upstream", "clone"]);
    hears_every_ref_step(&upstream, &base, true);
}

#[test]
fn ref_listeners_of_a_reftable_repository_hear_its_tables() {
    let tmp = TempDir::new().unwrap();
    let root = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    let upstream = root.join("upstream");
    committed_repository(&upstream);
    if let Err(reason) = try_git(
        &root,
        &["clone", "-q", "--ref-format=reftable", "upstream", "clone"],
    ) {
        eprintln!("skipped: the installed git cannot create a reftable repository: {reason}");
        return;
    }
    // Reftable compacts its tables as it writes, so packing may change nothing.
    hears_every_ref_step(&upstream, &root.join("clone"), false);
}

// Every step changes the refs of `base`, a clone of `upstream`, as a terminal would.
fn hears_every_ref_step(upstream: &Path, base: &Path, packs: bool) {
    let (picker, _frecency) = make_ref_picker(base);
    let refs = RefChanges::subscribe(&picker, base, base);

    for step in [
        &["branch", "feature"][..],
        &["commit", "-q", "--allow-empty", "-m", "second"],
        &["branch", "-f", "feature", "HEAD"],
        &["branch", "-m", "feature", "renamed"],
        &["branch", "-D", "renamed"],
        // A namespace is a directory of its own below refs/heads.
        &["branch", "team/topic"],
        &["branch", "-D", "team/topic"],
        &["tag", "v1"],
        &["tag", "-d", "v1"],
        // HEAD moves without any branch changing.
        &["branch", "existing"],
        &["switch", "-q", "existing"],
        &["switch", "-q", "--detach"],
        &["commit", "-q", "--allow-empty", "-m", "detached"],
        &["switch", "-q", "main"],
        &["branch", "packed"],
        &["pack-refs", "--all"],
        // Only `packed-refs` holds this branch now.
        &["branch", "-D", "packed"],
    ] {
        if step[0] == "pack-refs" && !packs {
            continue;
        }
        refs.after(base, || git(base, step), &format!("git {}", step.join(" ")));
    }
    git(
        upstream,
        &["commit", "-q", "--allow-empty", "-m", "upstream"],
    );
    refs.after(base, || git(base, &["fetch", "-q", "origin"]), "a fetch");
}

#[test]
fn ref_listeners_hear_upstream_and_remote_config_once() {
    let tmp = TempDir::new().unwrap();
    let root = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    let upstream = root.join("upstream");
    let base = root.join("clone");
    committed_repository(&upstream);
    git(&root, &["clone", "-q", "upstream", "clone"]);
    let (picker, _frecency) = make_ref_picker(&base);
    let refs = RefChanges::subscribe(&picker, &base, &base);
    let moved = root.join("moved");
    let moved = moved.to_string_lossy();

    // How many times each command writes the config.
    for (writes, step) in [
        (2, &["branch", "--unset-upstream"][..]),
        (2, &["branch", "-u", "origin/main"]),
        (1, &["remote", "set-url", "origin", &moved]),
        // Any key of the shared config is announced: consumers read what they need.
        (1, &["config", "fff.unrelated", "true"]),
    ] {
        let what = format!("git {}", step.join(" "));
        refs.at_most(writes, &base, || git(&base, step), &what);
    }
}

// Linked worktrees below the main checkout, which ignores them, and one beside it. How often
// one command is heard is timing: git writes the admin dir in place around a checkout.
#[test]
fn ref_listeners_hear_linked_worktrees_added_moved_and_removed() {
    hears_the_set_of_worktrees(make_ref_picker);
}

#[cfg(target_os = "macos")]
#[test]
fn ref_listeners_hear_linked_worktrees_with_the_native_watcher() {
    hears_the_set_of_worktrees(make_watched_picker);
}

fn hears_the_set_of_worktrees(make_picker: fn(&Path) -> (SharedFilePicker, SharedFrecency)) {
    let tmp = TempDir::new().unwrap();
    let root = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    let main = root.join("main");
    seed(&main);
    fs::write(main.join(".gitignore"), ".worktrees/\n").unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "init"]);
    let (picker, _frecency) = make_picker(&main);
    let (state_sender, state) = std::sync::mpsc::channel();
    let _state = picker.subscribe_repository_state(move |root| {
        let _ = state_sender.send(root.to_path_buf());
    });
    let refs = RefChanges::subscribe(&picker, &main, &main);

    for step in [
        &["worktree", "add", "-q", "--detach", ".worktrees/inner"][..],
        &["worktree", "add", "-q", "--detach", "../beside"],
        &["worktree", "lock", ".worktrees/inner"],
        &["worktree", "unlock", ".worktrees/inner"],
        &["worktree", "move", ".worktrees/inner", ".worktrees/moved"],
        &["worktree", "remove", ".worktrees/moved"],
    ] {
        refs.only(
            &main,
            || git(&main, step),
            &format!("git {}", step.join(" ")),
        );
    }
    // Work in another worktree is its own: its admin dir changes, its set does not, and its
    // operations in progress are not the main checkout's. Only its HEAD is ref state.
    let beside = root.join("beside");
    refs.quiet(
        || git(&beside, &["status", "--short"]),
        "a status in another worktree",
    );
    refs.only(
        &main,
        || git(&beside, &["commit", "-q", "--allow-empty", "-m", "beside"]),
        "a commit on another worktree's detached HEAD",
    );
    refs.after(
        &main,
        || {
            git(&beside, &["switch", "-q", "-c", "side"]);
            fs::write(beside.join("README.md"), "side\n").unwrap();
            git(&beside, &["commit", "-q", "-am", "side"]);
        },
        "a branch of another worktree",
    );
    git(&beside, &["switch", "-q", "--detach", "main"]);
    fs::write(beside.join("README.md"), "other\n").unwrap();
    git(&beside, &["commit", "-q", "-am", "other"]);
    refs.fence();
    state.try_iter().for_each(drop);
    for (operation, abort) in [
        ("merge", "--abort"),
        ("cherry-pick", "--abort"),
        ("revert", "--abort"),
    ] {
        refs.quiet(
            || {
                // Each stops at its conflict, with its `*_HEAD` in the worktree's admin dir.
                let _ = try_git(&beside, &[operation, "side"]);
                git(&beside, &[operation, abort]);
            },
            &format!("a {operation} in another worktree"),
        );
        let repository: Vec<_> = state.try_iter().collect();
        assert!(
            !repository.contains(&main),
            "a {operation} in another worktree: {repository:?}"
        );
    }
    fs::remove_dir_all(root.join("beside")).unwrap();
    refs.after(
        &main,
        || git(&main, &["worktree", "prune"]),
        "git worktree prune",
    );
}

#[test]
fn ref_listeners_of_a_linked_worktree_hear_shared_and_own_config() {
    let tmp = TempDir::new().unwrap();
    let root = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    let main = root.join("main");
    let linked = root.join("linked");
    committed_repository(&main);
    git(
        &main,
        &["worktree", "add", "-q", "-b", "linked", "../linked"],
    );
    let (picker, _frecency) = make_ref_picker(&linked);
    let refs = RefChanges::subscribe(&picker, &linked, &linked);

    refs.at_most(
        1,
        &linked,
        || git(&main, &["config", "branch.linked.merge", "refs/heads/main"]),
        "the main repository's branch tracking",
    );
    refs.at_most(
        1,
        &linked,
        || git(&main, &["config", "extensions.worktreeConfig", "true"]),
        "the shared config",
    );
    refs.at_most(
        1,
        &linked,
        || git(&linked, &["config", "--worktree", "fff.own", "true"]),
        "the worktree's own config",
    );
}

#[test]
fn ref_listeners_of_a_linked_worktree_hear_its_main_repository_refs() {
    let tmp = TempDir::new().unwrap();
    let root = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    let main = root.join("main");
    let linked = root.join("linked");
    committed_repository(&main);
    git(
        &main,
        &["worktree", "add", "-q", "-b", "linked", "../linked"],
    );
    let (picker, _frecency) = make_ref_picker(&linked);
    let refs = RefChanges::subscribe(&picker, &linked, &linked);

    refs.after(&linked, || git(&main, &["branch", "other"]), "a branch");
    refs.after(&linked, || git(&main, &["tag", "v1"]), "a tag");
    refs.after(
        &linked,
        || git(&linked, &["commit", "-q", "--allow-empty", "-m", "moved"]),
        "a commit in the worktree",
    );
    refs.after(
        &linked,
        || git(&linked, &["switch", "-q", "--detach"]),
        "the worktree's own HEAD",
    );
    refs.quiet(
        || git(&main, &["switch", "-q", "--detach"]),
        "the main checkout's HEAD",
    );
    refs.after(&linked, || git(&main, &["pack-refs", "--all"]), "packing");
    refs.after(
        &linked,
        || git(&main, &["branch", "-D", "other"]),
        "a packed branch deleted",
    );

    // The watches go with the last listener and come back with the next.
    drop(refs);
    let refs = RefChanges::subscribe(&picker, &linked, &linked);
    refs.after(
        &linked,
        || git(&main, &["branch", "again"]),
        "a branch after resubscribing",
    );
}

#[test]
fn ref_listeners_of_a_reftable_linked_worktree_hear_both_table_lists() {
    let tmp = TempDir::new().unwrap();
    let root = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    let main = root.join("main");
    let linked = root.join("linked");
    seed(&main);
    if let Err(reason) = try_git(
        &main,
        &["init", "-q", "--ref-format=reftable", "-b", "main"],
    ) {
        eprintln!("skipped: the installed git cannot create a reftable repository: {reason}");
        return;
    }
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "init"]);
    git(
        &main,
        &["worktree", "add", "-q", "-b", "linked", "../linked"],
    );
    git(&main, &["branch", "side"]);
    let beside = root.join("beside");
    git(&main, &["worktree", "add", "-q", "--detach", "../beside"]);
    let (picker, _frecency) = make_ref_picker(&linked);
    let refs = RefChanges::subscribe(&picker, &linked, &linked);

    refs.after(&linked, || git(&main, &["branch", "other"]), "a branch");
    refs.after(
        &linked,
        || git(&linked, &["switch", "-q", "--detach"]),
        "the worktree's own HEAD",
    );
    refs.after(
        &linked,
        || git(&main, &["branch", "-D", "other"]),
        "a branch deleted",
    );
    // The HEAD of a worktree the index does not follow moves its tables, not its HEAD file.
    refs.only(
        &linked,
        || git(&beside, &["switch", "-q", "side"]),
        "the HEAD of another worktree",
    );
    refs.quiet(
        || {
            fs::write(beside.join("staged.txt"), "staged\n").unwrap();
            git(&beside, &["add", "staged.txt"]);
        },
        "the index of another worktree",
    );
}

// A switch in a worktree the index does not follow moves its HEAD alone, in its admin dir, which
// names the index's work tree. Other files there stay unheard, and it is no repository here.
#[test]
fn ref_listeners_hear_the_head_of_a_worktree_the_index_does_not_follow() {
    let tmp = TempDir::new().unwrap();
    let root = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    let (main, linked, beside) = (root.join("main"), root.join("linked"), root.join("beside"));
    committed_repository(&main);
    for branch in ["side", "other"] {
        git(&main, &["branch", branch]);
    }
    git(
        &main,
        &["worktree", "add", "-q", "-b", "linked", "../linked"],
    );
    git(&main, &["worktree", "add", "-q", "--detach", "../beside"]);
    let (picker, _frecency) = make_ref_picker(&linked);
    let (state_sender, state) = std::sync::mpsc::channel();
    let _state = picker.subscribe_repository_state(move |root| {
        let _ = state_sender.send(root.to_path_buf());
    });
    let refs = RefChanges::subscribe(&picker, &linked, &linked);

    refs.only(
        &linked,
        || git(&beside, &["switch", "-q", "side"]),
        "a switch in another worktree",
    );
    refs.quiet(
        || {
            fs::write(beside.join("staged.txt"), "staged\n").unwrap();
            git(&beside, &["add", "staged.txt"]);
            git(&beside, &["status", "--short"]);
        },
        "the index of another worktree",
    );
    let repository: Vec<_> = state.try_iter().collect();
    assert!(repository.is_empty(), "no git state here: {repository:?}");

    // Each worktree added after the watch began is followed once the watcher saw it. The first
    // switch may come before that; the event after its watch stands for it, once.
    let late = root.join("late");
    refs.after(
        &linked,
        || git(&main, &["worktree", "add", "-q", "--detach", "../late"]),
        "a worktree added",
    );
    let switches_in_late = |what: &str| {
        for target in ["other", "--detach", "other"] {
            refs.only(
                &linked,
                || git(&late, &["switch", "-q", target]),
                &format!("{what}: switch {target}"),
            );
        }
    };
    switches_in_late("a worktree added after the watch began");

    // Its admin dir gone and back, faster than the watcher looks, costs the old watch.
    refs.after(
        &linked,
        || {
            git(&main, &["worktree", "remove", "--force", "../late"]);
            git(&main, &["worktree", "add", "-q", "--detach", "../late"]);
        },
        "a worktree added again",
    );
    switches_in_late("a worktree added again");
    assert_eq!(picker.rescan_stats().total, 0);
}

// `git worktree remove` takes `worktrees` along with the last worktree, and inotify's debouncer
// reports only that removal, also when the worktree is back by then.
#[test]
fn ref_listeners_hear_the_head_of_the_last_worktree_added_again() {
    let tmp = TempDir::new().unwrap();
    let root = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    let (main, late) = (root.join("main"), root.join("late"));
    tagged_repository(&main);
    git(&main, &["worktree", "add", "-q", "--detach", "../late"]);
    let (picker, _frecency) = make_ref_picker(&main);
    let refs = RefChanges::subscribe(&picker, &main, &main);
    refs.only(&main, || switch_to(&late, "one"), "a switch at first");

    refs.after(
        &main,
        || {
            git(&main, &["worktree", "remove", "--force", "../late"]);
            git(&main, &["worktree", "add", "-q", "--detach", "../late"]);
        },
        "the last worktree added again",
    );
    for tag in ["one", "two", "one"] {
        let what = format!("a switch to {tag} after it was added again");
        refs.only(&main, || switch_to(&late, tag), &what);
    }
}

// What a view of a main checkout relies on: a switch in any of its worktrees reaches its
// subscribers, ignored below it, beside it or open below it, followed by another index or not.
#[test]
fn ref_listeners_of_a_main_checkout_hear_the_heads_of_all_its_worktrees() {
    hears_the_heads_of_all_worktrees(make_ref_picker);
}

#[cfg(target_os = "macos")]
#[test]
fn ref_listeners_of_a_main_checkout_hear_the_heads_of_all_its_worktrees_natively() {
    hears_the_heads_of_all_worktrees(make_watched_picker);
}

fn hears_the_heads_of_all_worktrees(make_picker: fn(&Path) -> (SharedFilePicker, SharedFrecency)) {
    let tmp = TempDir::new().unwrap();
    let root = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    let main = root.join("main");
    seed(&main);
    fs::write(main.join(".gitignore"), ".worktrees/\n").unwrap();
    tagged_repository(&main);
    let (ignored, beside, open) = (
        main.join(".worktrees/ignored"),
        root.join("beside"),
        main.join("open"),
    );
    for path in [".worktrees/ignored", "../beside", "open"] {
        git(&main, &["worktree", "add", "-q", "--detach", path]);
    }
    let (picker, _frecency) = make_picker(&main);
    let broad = RefChanges::subscribe(&picker, &main, &main);
    let narrow = RefChanges::subscribe(&picker, &main.join("src"), &main);
    // A worktree open below the checkout is named to whoever's scope holds it.
    let layouts = [
        (&ignored, &main, "ignored"),
        (&beside, &main, "beside"),
        (&open, &open, "open"),
    ];
    let switches = |worktree: &Path, named: &Path, what: &str| {
        for tag in ["one", "two"] {
            broad.after(named, || switch_to(worktree, tag), what);
            let heard = narrow.fence();
            assert!(heard.contains(&main), "{what}, narrowly: heard {heard:?}");
        }
    };
    for (worktree, named, what) in layouts {
        switches(worktree, named, what);
    }

    // The index meets an ignored worktree through its git state, and follows it from then on.
    broad.quiet(
        || {
            fs::write(ignored.join("staged.txt"), "staged\n").unwrap();
            git(&ignored, &["add", "staged.txt"]);
        },
        "the index of an ignored worktree",
    );
    broad.after(
        &main,
        || git(&main, &["worktree", "add", "-q", "--detach", "../late"]),
        "another worktree, whose set makes the watches follow",
    );
    narrow.fence();
    switches(&ignored, &main, "ignored, once met");

    for (worktree, named, what) in layouts {
        let (other, _frecency) = make_picker(worktree);
        let _own = RefChanges::subscribe(&other, worktree, worktree);
        switches(
            worktree,
            named,
            &format!("{what}, with an index of its own"),
        );
    }

    // Nothing depends on another listener holding the worktree in its scope.
    drop(broad);
    narrow.fence();
    narrow.after(&main, || switch_to(&open, "one"), "open, narrowly alone");
}

#[test]
fn ref_listeners_hear_only_the_repositories_of_their_scope() {
    let tmp = TempDir::new().unwrap();
    let root = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    committed_repository(&root.join("a"));
    committed_repository(&root.join("b"));
    let (picker, _frecency) = make_ref_picker(&root);
    let refs = RefChanges::subscribe(&picker, &root.join("a"), &root);

    refs.quiet(
        || git(&root.join("b"), &["branch", "other"]),
        "another repository",
    );
    refs.after(
        &root.join("a"),
        || git(&root.join("a"), &["branch", "other"]),
        "the scope's repository",
    );
}

#[test]
fn ref_listeners_hear_a_repository_whose_refs_appear_after_it() {
    let tmp = TempDir::new().unwrap();
    let root = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    let upstream = root.join("upstream");
    committed_repository(&upstream);
    let base = root.join("index");
    let repo = base.join("repo");
    // A clone caught after it made its git dir and before its refs.
    fs::create_dir_all(repo.join(".git/objects")).unwrap();
    fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    seed(&base);
    let (picker, _frecency) = make_ref_picker(&base);
    let refs = RefChanges::subscribe(&picker, &base, &base);

    let commit = git_output(&upstream, &["rev-parse", "HEAD"]);
    refs.after(
        &repo,
        || {
            fs::create_dir_all(repo.join(".git/refs/heads")).unwrap();
            fs::write(repo.join(".git/refs/heads/main"), commit).unwrap();
        },
        "the refs created",
    );
    refs.after(
        &repo,
        || {
            fs::write(
                repo.join(".git/refs/heads/second"),
                git_output(&upstream, &["rev-parse", "HEAD"]),
            )
            .unwrap()
        },
        "a branch in them",
    );
}

#[test]
fn subscribers_without_ref_listeners_hear_no_ref_changes() {
    let tmp = TempDir::new().unwrap();
    let base = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    committed_repository(&base);
    let (picker, _frecency) = make_ref_picker(&base);
    let files = watch_collect(&picker, "", WatchOptions::default());

    for step in [
        &["branch", "quiet"][..],
        &["tag", "quiet-tag"],
        &["pack-refs", "--all"],
        &["branch", "-D", "quiet"],
    ] {
        git(&base, step);
    }
    // A listener elsewhere turns ref watching on without reaching the others.
    let refs = RefChanges::subscribe(&picker, &base, &base);
    refs.after(
        &base,
        || git(&base, &["branch", "heard"]),
        "the opted-in listener",
    );
    // Its sentinels follow every change before them.
    let files = files.lock();
    assert!(
        (files.iter()).any(|e| e.path.to_string_lossy().contains("ref-sentinel-")),
        "the sentinels reached every subscriber: {files:?}"
    );
    assert!(
        files.iter().all(|e| !e.path.starts_with(base.join(".git"))),
        "file subscribers never hear git dir changes: {files:?}"
    );
}

// With FSEvents, an index of a linked worktree whose main repository lies
// outside it. A second repository's worktree nested in the index serves as a fence: its git
// dir lies outside too, so its refs come through the same stream, in order.
#[cfg(target_os = "macos")]
#[test]
fn a_linked_worktree_hears_its_refs_with_the_native_watcher() {
    let tmp = TempDir::new().unwrap();
    let root = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    let (main, other) = (root.join("main"), root.join("other"));
    let (linked, nested) = (root.join("linked"), root.join("linked/nested"));
    committed_repository(&main);
    committed_repository(&other);
    git(
        &main,
        &["worktree", "add", "-q", "-b", "linked", "../linked"],
    );
    git(
        &other,
        &["worktree", "add", "-q", "-b", "nested", "../linked/nested"],
    );
    git(&main, &["worktree", "add", "-q", "--detach", "../beside"]);
    let beside = root.join("beside");
    git(&main, &["branch", "side"]);
    git(&main, &["branch", "later"]);
    let (picker, _frecency) = make_watched_picker(&linked);
    assert_eq!(
        picker.rescan_stats().total,
        0,
        "watching a git dir outside the index during the walk rescans nothing"
    );
    let (state_sender, state) = std::sync::mpsc::channel();
    let _state = picker.subscribe_repository_state(move |root| {
        let _ = state_sender.send(root.to_path_buf());
    });
    let (sender, heard) = std::sync::mpsc::channel();
    let _refs = picker.subscribe_ref_changes(&linked, move |root| {
        let _ = sender.send(root.to_path_buf());
    });
    let fences = std::cell::Cell::new(0);
    // Returns what was heard before the fence.
    let fence = || {
        fences.set(fences.get() + 1);
        git(&other, &["branch", &format!("fence-{}", fences.get())]);
        until_heard(&heard, &nested, "the fence")
    };
    fence();
    let step = |change: &dyn Fn(), what: &str| {
        change();
        until_heard(&heard, &linked, what);
        fence();
    };

    step(
        &|| git(&main, &["branch", "other"]),
        "a branch of the main repository",
    );
    step(
        &|| git(&linked, &["switch", "-q", "--detach"]),
        "the worktree's own HEAD",
    );
    git(&main, &["switch", "-q", "--detach"]);
    let before = fence();
    assert!(
        !before.contains(&linked),
        "the main checkout's HEAD is its own: {before:?}"
    );
    // A worktree the index does not follow: its HEAD alone, through the recursive watch of
    // `worktrees`.
    step(
        &|| git(&beside, &["switch", "-q", "side"]),
        "the HEAD of another worktree",
    );
    fs::write(beside.join("staged.txt"), "staged\n").unwrap();
    git(&beside, &["add", "staged.txt"]);
    git(&beside, &["status", "--short"]);
    let before = fence();
    assert!(
        !before.contains(&linked),
        "the index of another worktree is its own: {before:?}"
    );
    // Upstreams and remotes in the main repository's config, heard at least once and at
    // most once per write of the config.
    // Worktrees added from the main checkout restart no stream: the linked worktree hears refs
    // events, spelled one way, and no repository event, which a restart's recovery sends.
    state.try_iter().for_each(drop);
    for n in 0..3 {
        let what = format!("git worktree add {n}");
        let path = format!("../added-{n}");
        git(&main, &["worktree", "add", "-q", "--detach", &path]);
        until_heard(&heard, &linked, &what);
        let later = fence();
        // At most once per write of its admin dir's ref state, whatever a loaded machine
        // spreads over batches: the dir, `locked` and its removal, `gitdir`, and HEAD twice.
        let times = 1 + later.iter().filter(|root| **root == linked).count();
        assert!(times <= 6, "{what}: heard {times} times, {later:?}");
        let spelled = |root: &std::path::PathBuf| root.as_os_str().to_string_lossy().ends_with('/');
        assert!(!later.iter().any(spelled), "{what}: {later:?}");
        let repository: Vec<_> = state.try_iter().collect();
        assert!(
            !repository.contains(&linked),
            "{what} restarted a stream: {repository:?}"
        );
    }
    // The recursive watch has covered a worktree added since from its first file on.
    step(
        &|| git(&root.join("added-0"), &["switch", "-q", "later"]),
        "the HEAD of a worktree added after the watch began",
    );
    for (writes, args) in [
        (1, &["config", "branch.linked.merge", "refs/heads/main"][..]),
        (2, &["remote", "add", "origin", "../other"]),
        (1, &["remote", "set-url", "origin", "../main"]),
    ] {
        let what = format!("git {}", args.join(" "));
        git(&main, args);
        until_heard(&heard, &linked, &what);
        let later = fence();
        let times = 1 + later.iter().filter(|root| **root == linked).count();
        assert!(times <= writes, "{what}: heard {times} times, {later:?}");
    }
    assert_eq!(picker.rescan_stats().total, 0);
}

// With FSEvents the own repository's refs are watched from the start and stay, so neither the
// first listener nor the last one leaving restarts a stream.
#[cfg(target_os = "macos")]
#[test]
fn refs_listeners_of_a_linked_worktree_restart_no_stream() {
    let tmp = TempDir::new().unwrap();
    let root = fff_search::path_utils::canonicalize(tmp.path()).unwrap();
    let (main, linked) = (root.join("main"), root.join("linked"));
    committed_repository(&main);
    git(
        &main,
        &["worktree", "add", "-q", "-b", "linked", "../linked"],
    );
    let (picker, _frecency) = make_watched_picker(&linked);
    let (state_sender, state) = std::sync::mpsc::channel();
    let _state = picker.subscribe_repository_state(move |root| {
        let _ = state_sender.send(root.to_path_buf());
    });

    // A restarted stream announces its repositories at once, from the change that restarts it.
    for round in 0..2 {
        let (sender, heard) = std::sync::mpsc::channel();
        let refs = picker.subscribe_ref_changes(&linked, move |root| {
            let _ = sender.send(root.to_path_buf());
        });
        assert!(state.try_recv().is_err(), "subscribing announced a restart");
        git(&main, &["branch", &format!("heard-{round}")]);
        until_heard(&heard, &linked, "a branch of the main repository");
        drop(refs);
        assert!(
            state.try_recv().is_err(),
            "unsubscribing announced a restart"
        );
    }
    assert_eq!(picker.rescan_stats().total, 0);
}

// Waits until `heard` names `work_tree` and returns what it named before.
#[cfg(target_os = "macos")]
fn until_heard(
    heard: &std::sync::mpsc::Receiver<std::path::PathBuf>,
    work_tree: &Path,
    what: &str,
) -> Vec<std::path::PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut before = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match heard.recv_timeout(left) {
            Ok(root) if root == work_tree => return before,
            Ok(root) => before.push(root),
            Err(_) => break,
        }
    }
    panic!(
        "{what}: no ref change of {}, heard {before:?}",
        work_tree.display()
    );
}

// Folds events into index presence, rejecting any the index could not have applied.
fn presence_after(initial: bool, events: &[WatchEvent]) -> bool {
    events.iter().fold(initial, |present, event| {
        let valid = match event.kind {
            WatchEventKind::Created => !present,
            WatchEventKind::Modified | WatchEventKind::Removed => present,
            _ => false,
        };
        assert!(
            valid,
            "{:?} is not a transition from {present}: {events:?}",
            event.kind
        );
        event.kind != WatchEventKind::Removed
    })
}

// FSEvents restarts its stream whenever the watched paths change and may coalesce what a
// step writes, so macOS polls; inotify delivers every step as it happens.
fn make_ref_picker(base: &Path) -> (SharedFilePicker, SharedFrecency) {
    if cfg!(target_os = "macos") {
        fff_search::watch::poll_watcher_for(base);
    }
    make_watched_picker(base)
}

// A repository with two tags, to switch a worktree's HEAD between without moving a ref.
fn tagged_repository(dir: &Path) {
    committed_repository(dir);
    git(dir, &["tag", "one"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "two"]);
    git(dir, &["tag", "two"]);
}

fn switch_to(worktree: &Path, tag: &str) {
    git(worktree, &["switch", "-q", "--detach", tag]);
}

// A repository on `main` with one commit.
fn committed_repository(dir: &Path) {
    seed(dir);
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn git(dir: &Path, args: &[&str]) {
    if let Err(error) = try_git(dir, args) {
        panic!("git {args:?}: {error}");
    }
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    let output = git_command(dir, args).output().unwrap();
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn try_git(dir: &Path, args: &[&str]) -> Result<(), String> {
    fs::create_dir_all(dir).unwrap();
    let output = git_command(dir, args).output().map_err(|e| e.to_string())?;
    match output.status.success() {
        true => Ok(()),
        false => Err(String::from_utf8_lossy(&output.stderr).trim().to_string()),
    }
}

fn git_command(dir: &Path, args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com");
    command
}

// The work trees a ref listener heard, beside a file subscription whose sentinel tells when
// the watcher has handled every change made before it; dropping it unsubscribes both.
struct RefChanges {
    picker: SharedFilePicker,
    heard: std::sync::mpsc::Receiver<std::path::PathBuf>,
    files: std::sync::mpsc::Receiver<WatchEvent>,
    files_id: fff_search::WatchId,
    index: std::path::PathBuf,
    writes: std::cell::Cell<usize>,
    _subscription: fff_search::ListenerSubscription,
}

impl RefChanges {
    // Listens to the refs of `scope`'s repositories; the sentinel is written in `index`.
    fn subscribe(picker: &SharedFilePicker, scope: &Path, index: &Path) -> Self {
        let (sender, heard) = std::sync::mpsc::channel();
        let subscription = picker.subscribe_ref_changes(scope, move |root| {
            let _ = sender.send(root.to_path_buf());
        });
        let (sender, files) = std::sync::mpsc::channel();
        let files_id = picker
            .watch("", WatchOptions::default(), move |_, events| {
                for event in events {
                    let _ = sender.send(event.clone());
                }
            })
            .unwrap();
        let refs = Self {
            picker: picker.clone(),
            heard,
            files,
            files_id,
            index: index.to_path_buf(),
            writes: std::cell::Cell::new(0),
            _subscription: subscription,
        };
        refs.fence();
        refs
    }

    // Runs `change` and waits until the listener names `work_tree`, then lets the watcher
    // handle the rest of the change, so the next step starts from nothing heard.
    fn after(&self, work_tree: &Path, change: impl FnOnce(), what: &str) {
        self.heard_after(work_tree, change, what);
    }

    // As `after`, and the listener names no other work tree for the change.
    fn only(&self, work_tree: &Path, change: impl FnOnce(), what: &str) {
        let others: Vec<_> = (self.heard_after(work_tree, change, what).into_iter())
            .filter(|root| root != work_tree)
            .collect();
        assert!(others.is_empty(), "{what}: heard {others:?}");
    }

    // What the listener named besides `work_tree` until it named it, then until the fence.
    fn heard_after(
        &self,
        work_tree: &Path,
        change: impl FnOnce(),
        what: &str,
    ) -> Vec<std::path::PathBuf> {
        change();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut heard = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.heard.recv_timeout(left) {
                Ok(root) if root == work_tree => {
                    heard.extend(self.fence());
                    return heard;
                }
                Ok(root) => heard.push(root),
                Err(_) => break,
            }
        }
        panic!(
            "{what}: no ref change of {}, heard {heard:?}",
            work_tree.display()
        );
    }

    // As `after`, and the listener names `work_tree` at most `most` times for the change:
    // events of one path coalesce within the debounce window, so a command writing a file
    // twice may still be heard twice.
    fn at_most(&self, most: usize, work_tree: &Path, change: impl FnOnce(), what: &str) {
        change();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut heard = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.heard.recv_timeout(left) {
                Ok(root) if root == work_tree => {
                    let later = self.fence();
                    let times = 1 + later.iter().filter(|root| *root == work_tree).count();
                    assert!(times <= most, "{what}: heard {times} times, {later:?}");
                    return;
                }
                Ok(root) => heard.push(root),
                Err(_) => break,
            }
        }
        panic!(
            "{what}: no ref change of {}, heard {heard:?}",
            work_tree.display()
        );
    }

    // Runs `change` and asserts the listener hears nothing of it.
    fn quiet(&self, change: impl FnOnce(), what: &str) {
        change();
        let heard = self.fence();
        assert!(heard.is_empty(), "{what}: heard {heard:?}");
    }

    // Events come in order, and a batch's file events go out before its ref changes, so two
    // sentinels follow each other: once the second is heard, the watcher has handled every
    // change before the first. Each is a new file, as one write may be heard twice. Returns
    // what the listener heard until then.
    fn fence(&self) -> Vec<std::path::PathBuf> {
        for _ in 0..2 {
            let sentinel = self.sentinel();
            fs::write(&sentinel, "sentinel\n").unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let left = deadline
                    .checked_duration_since(Instant::now())
                    .unwrap_or_else(|| panic!("no event of {}", sentinel.display()));
                match self.files.recv_timeout(left) {
                    Ok(event) if event.path == sentinel => break,
                    Ok(_) => {}
                    Err(error) => panic!("no event of {}: {error}", sentinel.display()),
                }
            }
        }
        self.heard.try_iter().collect()
    }

    fn sentinel(&self) -> std::path::PathBuf {
        let writes = self.writes.get() + 1;
        self.writes.set(writes);
        self.index.join(format!("ref-sentinel-{writes}.txt"))
    }
}

impl Drop for RefChanges {
    fn drop(&mut self) {
        self.picker.unwatch(self.files_id);
    }
}
