use crate::daemon::{CommandOutputExt, endpoints};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_fff-mcp");
const TEARDOWN: Duration = Duration::from_secs(15);

/// Test root whose service namespace is torn down on drop, also when the test panics.
pub struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    pub fn service(&self) -> PathBuf {
        self.path().join("service")
    }

    // Asks every service to stop, then removes the namespace so an orphaned one exits.
    fn teardown(&self) -> Vec<u64> {
        let service = self.service();
        let mut pids = self.started_services();
        if !endpoints(&service).is_empty() {
            pids.extend(reported_pids(&service));
            let _ = cli("--service-stop", &service);
        }
        pids.sort_unstable();
        pids.dedup();
        if pids.is_empty() {
            return pids;
        }
        let _ = std::fs::remove_dir_all(&service);
        let deadline = Instant::now() + TEARDOWN;
        while !pids.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            pids.retain(|&pid| process_alive(pid));
        }
        pids
    }

    // Service processes whose command line names this sandbox's namespace, reachable or not.
    #[cfg(unix)]
    fn started_services(&self) -> Vec<u64> {
        let output = Command::new("ps")
            .args(["-A", "-o", "pid=,command="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let (pid, command) = line.trim_start().split_once(' ')?;
                self.names_service(command).then(|| pid.parse().ok())?
            })
            .collect()
    }

    #[cfg(windows)]
    fn started_services(&self) -> Vec<u64> {
        Vec::new()
    }

    #[cfg(unix)]
    fn names_service(&self, command: &str) -> bool {
        let unique = self.path().file_name().unwrap().to_string_lossy();
        let namespace = format!("{unique}/service");
        let args: Vec<&str> = command.split_whitespace().collect();
        args.contains(&"--service") && args.iter().any(|arg| arg.ends_with(&namespace))
    }

    // Kills a service only while its command line still names this sandbox.
    #[cfg(unix)]
    fn kill(&self, pid: u64) {
        let output = Command::new("ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        if self.names_service(&String::from_utf8_lossy(&output.stdout)) {
            let pid = rustix::process::Pid::from_raw(pid as i32).unwrap();
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }

    // The pid answered the authenticated status request of this namespace.
    #[cfg(windows)]
    fn kill(&self, pid: u64) {
        let _ = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let survivors = self.teardown();
        for &pid in &survivors {
            self.kill(pid);
        }
        assert!(
            survivors.is_empty() || std::thread::panicking(),
            "services {survivors:?} ignored stop and namespace removal"
        );
    }
}

fn cli(flag: &str, service: &Path) -> std::process::Output {
    Command::new(BIN)
        .arg(flag)
        .arg("--service-dir")
        .arg(service)
        .arg("--log-file")
        .arg(service.with_extension("log"))
        .output_serialized()
        .unwrap()
}

// Processes that answered as services of this namespace.
fn reported_pids(service: &Path) -> Vec<u64> {
    let output = cli("--service-status", service);
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
    #[cfg(unix)]
    {
        Command::new("ps")
            .args(["-p", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
    }
}
