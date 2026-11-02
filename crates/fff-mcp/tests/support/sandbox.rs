use crate::daemon::{CommandOutputExt, OUTPUT_DEADLINE, endpoints};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_fff-mcp");
const TEARDOWN: Duration = Duration::from_secs(15);
// The end of teardown kept for killing the services that outlived it.
const KILLING: Duration = Duration::from_secs(2);

/// Test root whose service namespace is torn down on drop, also when the test panics.
pub struct Sandbox {
    root: tempfile::TempDir,
    service: String,
}

impl Sandbox {
    pub fn new() -> Self {
        Self::with_service("service")
    }

    /// A sandbox whose namespace is `service`, a path relative to its root.
    pub fn with_service(service: &str) -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            service: service.to_owned(),
        }
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    pub fn service(&self) -> PathBuf {
        self.path().join(&self.service)
    }

    // Asks every service to stop, then removes the namespace so an orphaned one exits. Within
    // `deadline` a failed or slow `ps` counts as unknown, as teardown also runs while panicking.
    fn teardown(&self, deadline: Instant) -> Teardown {
        let service = self.service();
        let listed = self.started_services(deadline);
        let unlisted = listed.is_none();
        let mut pids = listed.unwrap_or_default();
        if !endpoints(&service).is_empty() {
            pids.extend(reported_pids(&service, deadline));
            let _ = cli("--service-stop", &service, deadline);
        }
        pids.sort_unstable();
        pids.dedup();
        let mut unknown = Vec::new();
        if pids.is_empty() {
            return Teardown {
                survivors: pids,
                unknown,
                unlisted,
            };
        }
        let _ = std::fs::remove_dir_all(&service);
        // Time is left to find out which survivors are still this sandbox's.
        let waited = deadline.checked_sub(KILLING).unwrap_or(deadline);
        while !pids.is_empty() && Instant::now() < waited {
            std::thread::sleep(Duration::from_millis(50));
            // Unknown counts as alive, so the survivor is killed rather than left behind.
            unknown.clear();
            pids.retain(|&pid| {
                probe(pid, left(waited)).unwrap_or_else(|_| {
                    unknown.push(pid);
                    true
                })
            });
        }
        Teardown {
            survivors: pids,
            unknown,
            unlisted,
        }
    }

    // Service processes whose command line names this sandbox's namespace, reachable or not.
    // None when the process listing is unavailable.
    #[cfg(unix)]
    fn started_services(&self, deadline: Instant) -> Option<Vec<u64>> {
        let output = Command::new("ps")
            .args(["-A", "-o", "pid=,command="])
            .output_within(left(deadline))
            .ok()?;
        let listing = String::from_utf8_lossy(&output.stdout);
        let pids = listing.lines().filter_map(|line| {
            let (pid, command) = line.trim_start().split_once(' ')?;
            self.names_service(command).then(|| pid.parse().ok())?
        });
        Some(pids.collect())
    }

    #[cfg(windows)]
    fn started_services(&self, _deadline: Instant) -> Option<Vec<u64>> {
        Some(Vec::new())
    }

    #[cfg(unix)]
    fn names_service(&self, command: &str) -> bool {
        let unique = self.path().file_name().unwrap().to_string_lossy();
        let namespace = format!("{unique}/{}", self.service);
        let args: Vec<&str> = command.split_whitespace().collect();
        args.contains(&"--service") && args.iter().any(|arg| arg.ends_with(&namespace))
    }

    // Kills a service only while its command line still names this sandbox; false when its
    // command line could not be read.
    #[cfg(unix)]
    fn kill(&self, pid: u64, deadline: Instant) -> bool {
        let Ok(output) = Command::new("ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output_within(left(deadline))
        else {
            return false;
        };
        if self.names_service(&String::from_utf8_lossy(&output.stdout)) {
            let pid = rustix::process::Pid::from_raw(pid as i32).unwrap();
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
        true
    }

    // The pid answered the authenticated status request of this namespace.
    #[cfg(windows)]
    fn kill(&self, pid: u64, deadline: Instant) -> bool {
        Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output_within(left(deadline))
            .is_ok()
    }
}

// Services left after teardown: those that outlived it, of which `unknown` could not be
// probed, and whether the process listing that finds orphans was unavailable.
struct Teardown {
    survivors: Vec<u64>,
    unknown: Vec<u64>,
    unlisted: bool,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let deadline = Instant::now() + TEARDOWN;
        let Teardown {
            survivors,
            mut unknown,
            unlisted,
        } = self.teardown(deadline);
        for &pid in &survivors {
            if !self.kill(pid, deadline) {
                unknown.push(pid);
            }
        }
        unknown.sort_unstable();
        unknown.dedup();
        let ignored: Vec<_> = (survivors.iter())
            .filter(|pid| !unknown.contains(pid))
            .collect();
        if std::thread::panicking() {
            return;
        }
        assert!(
            !unlisted,
            "the process listing was unavailable, so services of this sandbox went unchecked"
        );
        assert!(
            ignored.is_empty() && unknown.is_empty(),
            "services {ignored:?} ignored stop and namespace removal; probing {unknown:?} failed, so their state is unknown"
        );
    }
}

fn left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn cli(flag: &str, service: &Path, deadline: Instant) -> std::io::Result<std::process::Output> {
    Command::new(BIN)
        .arg(flag)
        .arg("--service-dir")
        .arg(service)
        .arg("--log-file")
        .arg(service.with_extension("log"))
        .output_within(left(deadline))
}

// Processes that answered as services of this namespace.
fn reported_pids(service: &Path, deadline: Instant) -> Vec<u64> {
    let Ok(output) = cli("--service-status", service, deadline) else {
        return Vec::new();
    };
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .ok()
        .and_then(|report| {
            report["services"].as_array().map(|services| {
                services
                    .iter()
                    .filter_map(|service| service["pid"].as_u64())
                    .collect()
            })
        })
        .unwrap_or_default()
}

// Lists processes without signalling them.
pub fn process_alive(pid: u64) -> bool {
    probe(pid, OUTPUT_DEADLINE).unwrap()
}

fn probe(pid: u64, limit: Duration) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string()])
            .output_within(limit)?;
        Ok(output.status.success())
    }
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output_within(limit)?;
        Ok(String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
    }
}
