use std::time::{Duration, Instant};

use fff_search::file_picker::{FFFMode, FilePicker};
use fff_search::{FilePickerOptions, SharedFilePicker, SharedFrecency, WatcherStatus};

fn picker(base: &std::path::Path, watch: bool) -> SharedFilePicker {
    std::fs::write(base.join("a.txt"), "a").unwrap();
    let shared = SharedFilePicker::default();
    FilePicker::new_with_shared_state(
        shared.clone(),
        SharedFrecency::default(),
        FilePickerOptions {
            base_path: base.to_string_lossy().into_owned(),
            watch,
            mode: FFFMode::Ai,
            ..Default::default()
        },
    )
    .unwrap();
    shared
}

fn watcher_status(shared: &SharedFilePicker) -> WatcherStatus {
    shared
        .read()
        .unwrap()
        .as_ref()
        .unwrap()
        .background_status()
        .watcher()
}

#[test]
fn watcher_status_reports_ready_and_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let shared = picker(dir.path(), true);
    assert!(shared.wait_for_watcher(Duration::from_secs(30)));
    assert_eq!(watcher_status(&shared), WatcherStatus::Ready);
    {
        let mut guard = shared.write().unwrap();
        let picker = guard.as_mut().unwrap();
        picker.cancel();
        picker.stop_background_monitor();
    }
    assert_eq!(watcher_status(&shared), WatcherStatus::Stopped);
}

#[test]
fn unwatched_picker_does_not_wait_for_a_watcher() {
    let dir = tempfile::tempdir().unwrap();
    let shared = picker(dir.path(), false);
    let start = Instant::now();
    assert!(!shared.wait_for_watcher(Duration::from_secs(30)));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(watcher_status(&shared), WatcherStatus::Disabled);
}

#[test]
fn cancelled_picker_never_keeps_a_watcher() {
    let dir = tempfile::tempdir().unwrap();
    let shared = picker(dir.path(), true);
    // Either order of cancellation and installation must end with no watcher.
    {
        let mut guard = shared.write().unwrap();
        let picker = guard.as_mut().unwrap();
        picker.cancel();
        picker.stop_background_monitor();
    }
    assert!(shared.wait_for_scan(Duration::from_secs(30)));
    assert!(!shared.wait_for_watcher(Duration::from_secs(30)));
    assert_eq!(watcher_status(&shared), WatcherStatus::Stopped);
    assert!(!shared.read().unwrap().as_ref().unwrap().is_watcher_ready());
}
