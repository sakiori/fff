use crate::spawn::CommandExt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

// No step of a test waits longer for a command; one that hangs fails the test that ran it.
pub const OUTPUT_DEADLINE: Duration = Duration::from_secs(90);

pub trait CommandOutputExt {
    fn output_isolated(&mut self) -> io::Result<Output>;
    fn output_within(&mut self, limit: Duration) -> io::Result<Output>;
}

impl CommandOutputExt for Command {
    fn output_isolated(&mut self) -> io::Result<Output> {
        self.output_within(OUTPUT_DEADLINE)
    }

    fn output_within(&mut self, limit: Duration) -> io::Result<Output> {
        let mut child = self
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn_isolated()?;
        let (sender, output) = mpsc::channel();
        drain(child.stdout.take(), true, &sender);
        drain(child.stderr.take(), false, &sender);
        drop(sender);
        let deadline = Instant::now() + limit;
        let timed_out = || {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{self:?} did not finish within {limit:?}"),
            )
        };
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(timed_out());
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        // A process that inherited the pipes keeps them open past the command's exit.
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match output.recv_timeout(left) {
                Ok((true, bytes)) => stdout = bytes,
                Ok((false, bytes)) => stderr = bytes,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(timed_out()),
            }
        }
        Ok(Output {
            status,
            stdout,
            stderr,
        })
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

fn drain(pipe: Option<impl Read + Send + 'static>, stdout: bool, sender: &Sender<(bool, Vec<u8>)>) {
    let sender = sender.clone();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        let _ = sender.send((stdout, bytes));
    });
}
