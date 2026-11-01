use crate::spawn::CommandExt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

pub trait CommandOutputExt {
    fn output_serialized(&mut self) -> io::Result<Output>;
}

impl CommandOutputExt for Command {
    fn output_serialized(&mut self) -> io::Result<Output> {
        let child = self
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn_serialized()?;
        child.wait_with_output()
    }
}

/// Reap a test-owned daemon even when a test unwinds before explicit shutdown.
pub struct Daemon(pub Child);

impl std::ops::Deref for Daemon {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for Daemon {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// Discovery files of every service build in the namespace.
pub fn endpoints(service: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(service)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("endpoint-") && name.ends_with(".json"))
        })
        .collect()
}

pub fn wait_for_discovery(child: &mut Child, service: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if endpoints(service).iter().any(|path| {
            std::fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some()
        }) {
            return;
        }
        assert!(Instant::now() < deadline && child.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(20));
    }
}
