#[path = "support/daemon.rs"]
mod daemon;
#[path = "support/sandbox.rs"]
mod sandbox;
#[path = "support/spawn.rs"]
mod spawn;

use daemon::{CommandOutputExt, Daemon, endpoints, wait_for_discovery};
use sandbox::{Sandbox, process_alive};
use spawn::CommandExt;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use interprocess::local_socket::{Stream, prelude::*};
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_fff-mcp");
const EVENTUAL: Duration = Duration::from_secs(15);
// Services watch through the polling backend, so watcher tests do not depend on
// the OS event service delivering events.
const POLL_WATCHER: (&str, &str) = ("FFF_TEST_POLL_WATCHER_MS", "25");

struct Client {
    child: Child,
    input: Option<ChildStdin>,
    output: mpsc::Receiver<Value>,
    id: u64,
}

impl Client {
    fn start(scope: &Path, service: &Path) -> Self {
        Self::start_with(scope, service, &[])
    }

    fn start_with(scope: &Path, service: &Path, extra: &[&str]) -> Self {
        Self::start_configured(BIN, scope, service, "1", extra, &[])
    }

    fn start_configured(
        binary: &str,
        scope: &Path,
        service: &Path,
        idle: &str,
        extra: &[&str],
        env: &[(&str, &str)],
    ) -> Self {
        let mut child = client_command(binary, scope, service, idle, extra)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn_serialized()
            .unwrap();
        let input = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, output) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if let Ok(message) = serde_json::from_str(&line)
                    && tx.send(message).is_err()
                {
                    break;
                }
            }
        });
        let mut client = Self {
            child,
            input,
            output,
            id: 0,
        };
        let result = client.request("initialize", json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"shared-service-test","version":"1"}}));
        assert!(result["serverInfo"].is_object(), "{result}");
        client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        client
    }

    fn send(&mut self, value: Value) {
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{value}").unwrap();
        input.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let reply = self.raw_request(method, params);
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }

    fn raw_request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        self.send(json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params}));
        loop {
            let reply = self
                .output
                .recv_timeout(Duration::from_secs(90))
                .expect("MCP response timeout");
            if reply["id"] == self.id {
                return reply;
            }
        }
    }

    fn tool(&mut self, name: &str, args: Value) -> String {
        self.request("tools/call", json!({"name":name,"arguments":args}))["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.input.take();
        if self.wait_exit(Duration::from_secs(5)).is_some() {
            return;
        }
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        assert!(
            std::thread::panicking(),
            "client did not exit after stdin closed"
        );
    }
}

fn client_command(
    binary: &str,
    scope: &Path,
    service: &Path,
    idle: &str,
    extra: &[&str],
) -> Command {
    let mut command = Command::new(binary);
    command
        .arg(scope)
        .args([
            "--no-update-check",
            "--no-warmup",
            "--idle-timeout-secs",
            idle,
        ])
        .args(extra)
        .arg("--service-dir")
        .arg(service)
        .arg("--log-file")
        .arg(service.with_extension("log"))
        .env("FFF_MCP_TEST_WATCHDOG_INTERVAL_MS", "100")
        .env(POLL_WATCHER.0, POLL_WATCHER.1);
    command
}

fn cli(binary: &str, flag: &str, service: &Path) -> std::process::Output {
    Command::new(binary)
        .arg(flag)
        .arg("--service-dir")
        .arg(service)
        .arg("--log-file")
        .arg(service.with_extension("log"))
        .output_serialized()
        .unwrap()
}

fn report(service: &Path) -> Value {
    let result = cli(BIN, "--service-status", service);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

// Status of this build's service.
fn status(service: &Path) -> Value {
    let report = report(service);
    let current: Vec<_> = report["services"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["current"] == true)
        .cloned()
        .collect();
    assert_eq!(current.len(), 1, "{report}");
    current[0].clone()
}

fn wait_status(service: &Path, what: &str, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + EVENTUAL;
    loop {
        let stats = status(service);
        if done(&stats) {
            return stats;
        }
        assert!(Instant::now() < deadline, "{what}: {stats}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn index_count(expected: usize) -> impl Fn(&Value) -> bool {
    move |stats| stats["index_count"] == expected
}

// One index serves exactly `sessions` sessions.
fn shared_by(sessions: usize) -> impl Fn(&Value) -> bool {
    move |stats| {
        stats["index_count"] == 1 && stats["sessions"].as_array().map(Vec::len) == Some(sessions)
    }
}

fn service_pids(service: &Path) -> Vec<u64> {
    report(service)["services"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["pid"].as_u64())
        .collect()
}

fn stop_service(service: &Path) -> std::process::Output {
    cli(BIN, "--service-stop", service)
}

// Waits until the service processes exited and withdrew their discovery state.
fn wait_stopped(service: &Path, pids: &[u64]) {
    assert!(!pids.is_empty());
    let deadline = Instant::now() + EVENTUAL;
    while Instant::now() < deadline {
        if endpoints(service).is_empty() && !pids.iter().any(|&pid| process_alive(pid)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("service did not stop: endpoints {:?}", endpoints(service));
}

fn fixture(root: &Path) -> (PathBuf, PathBuf) {
    let project = root.join("project");
    let sub = project.join("sub");
    std::fs::create_dir_all(sub.join("src")).unwrap();
    std::fs::create_dir_all(project.join("submarine")).unwrap();
    std::fs::write(sub.join("src/inside.rs"), "fn UniqueNeedle() {}\n").unwrap();
    std::fs::write(
        project.join("submarine/outside.rs"),
        "fn UniqueNeedle() {}\n",
    )
    .unwrap();
    (project, sub)
}

fn start_daemon(service: &Path, idle: &str) -> Daemon {
    let mut child = Daemon(
        Command::new(BIN)
            .args([
                "--service",
                "--no-update-check",
                "--idle-timeout-secs",
                idle,
            ])
            .arg("--service-dir")
            .arg(service)
            .arg("--log-file")
            .arg(service.with_extension("log"))
            .env(POLL_WATCHER.0, POLL_WATCHER.1)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn_serialized()
            .unwrap(),
    );
    wait_for_discovery(&mut child, service);
    child
}

// Sends one raw hello line to the running service and returns its reply.
fn raw_hello(service: &Path, line: &[u8]) -> Value {
    let (recv, mut send) = connect(service).split();
    let line = line.to_vec();
    // The service may stop reading an oversized hello; the reply still arrives.
    let writer = std::thread::spawn(move || {
        let _ = send.write_all(&line);
        let _ = send.flush();
        send
    });
    let mut reply = String::new();
    BufReader::new(recv).read_line(&mut reply).unwrap();
    drop(writer.join());
    serde_json::from_str(&reply).unwrap()
}

// Connects to the running service without a handshake.
fn connect(service: &Path) -> Stream {
    let endpoint: Value =
        serde_json::from_slice(&std::fs::read(&endpoints(service)[0]).unwrap()).unwrap();
    let name = endpoint["name"].as_str().unwrap();
    #[cfg(unix)]
    let name = name
        .to_fs_name::<interprocess::local_socket::GenericFilePath>()
        .unwrap();
    #[cfg(windows)]
    let name = name
        .to_ns_name::<interprocess::local_socket::GenericNamespaced>()
        .unwrap();
    Stream::connect(name).unwrap()
}

fn endpoint_token(service: &Path) -> String {
    let endpoint: Value =
        serde_json::from_slice(&std::fs::read(&endpoints(service)[0]).unwrap()).unwrap();
    endpoint["token"].as_str().unwrap().to_owned()
}

#[test]
fn scope_indexed_by_parent_watcher_joins_parent_coverage() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    // While this file exists, the service's watchers leave new directories unindexed.
    let hold = tmp.path().join("hold-dir-indexing");
    let hold_env = [("FFF_TEST_HOLD_DIR_INDEXING", hold.to_str().unwrap())];
    let mut parent = Client::start_configured(BIN, &project, &service, "1", &[], &hold_env);
    assert!(
        parent
            .tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("outside.rs")
    );
    std::fs::write(&hold, "").unwrap();
    let empty = project.join("new/empty");
    std::fs::create_dir_all(&empty).unwrap();
    // The parent's watcher has not indexed the directory, so the child builds its own index.
    let mut child = Client::start_configured(BIN, &empty, &service, "1", &[], &hold_env);
    assert!(
        !child
            .tool("find_files", json!({"query":"inside"}))
            .contains("inside.rs")
    );
    let empty = empty.canonicalize().unwrap();
    let own = wait_status(&service, "child index never became ready", |stats| {
        stats["index_count"] == 2
            && stats["indexes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|index| index["state"] == "ready")
    });
    assert!(
        own["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["root"] == json!(empty)),
        "{own}"
    );
    // Only the parent's watcher indexing the directory can move the child now.
    std::fs::remove_file(&hold).unwrap();
    wait_status(
        &service,
        "child never joined coverage the watcher added",
        shared_by(2),
    );
    std::fs::write(empty.join("later.rs"), "fn LaterMarker() {}\n").unwrap();
    let deadline = Instant::now() + EVENTUAL;
    loop {
        let parent_text = parent.tool("grep", json!({"query":"LaterMarker"}));
        let child_text = child.tool("grep", json!({"query":"LaterMarker"}));
        if parent_text.contains("new/empty/later.rs") && child_text.contains("later.rs") {
            assert!(!child_text.contains("new/empty/"), "{child_text}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "new file did not reach both scopes"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let pids = service_pids(&service);
    drop(child);
    drop(parent);
    wait_stopped(&service, &pids);
}

#[test]
fn scope_created_after_scan_joins_parent_once_covered() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    git2::Repository::init(&project).unwrap();
    std::fs::write(project.join(".gitignore"), "new/\n").unwrap();
    let mut parent = Client::start(&project, &service);
    assert!(
        parent
            .tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("outside.rs")
    );
    let scope = project.join("new/scope");
    std::fs::create_dir_all(&scope).unwrap();
    for n in 0..3 {
        std::fs::write(scope.join(format!("page{n}.rs")), "fn LateMarker() {}\n").unwrap();
    }
    // The parent index excludes the new directory, so the child starts on its own index.
    let mut child = Client::start(&scope, &service);
    let first = child.tool("find_files", json!({"query":"page", "maxResults":1}));
    let cursor = first
        .lines()
        .find_map(|line| line.strip_prefix("cursor: "))
        .unwrap()
        .to_owned();
    assert_eq!(status(&service)["index_count"], 2);
    std::fs::write(project.join(".gitignore"), "").unwrap();
    wait_status(
        &service,
        "child never joined coverage that grew after the scan",
        shared_by(2),
    );
    let next = child.raw_request(
        "tools/call",
        json!({"name":"find_files","arguments":{"query":"page","cursor":cursor}}),
    );
    assert!(
        next.get("error").is_some() || next["result"]["isError"] == true,
        "{next}"
    );
    let text = child.tool("grep", json!({"query":"LateMarker"}));
    assert!(
        text.contains("page0.rs") && !text.contains("new/scope/"),
        "{text}"
    );
    assert!(
        parent
            .tool("grep", json!({"query":"LateMarker"}))
            .contains("new/scope/page0.rs")
    );
    let pids = service_pids(&service);
    drop(child);
    drop(parent);
    wait_stopped(&service, &pids);
}

#[test]
fn removed_scope_fails_visibly_and_recovers_when_restored() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = fixture(tmp.path());
    let mut parent = Client::start(&project, &service);
    let mut child = Client::start(&sub, &service);
    assert!(
        child
            .tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("src/inside.rs")
    );
    wait_status(
        &service,
        "child never shared the parent index",
        shared_by(2),
    );
    std::fs::remove_dir_all(&sub).unwrap();
    let deadline = Instant::now() + EVENTUAL;
    loop {
        let reply = child.raw_request(
            "tools/call",
            json!({"name":"grep","arguments":{"query":"UniqueNeedle"}}),
        );
        if reply.get("error").is_some() || reply["result"]["isError"] == true {
            assert!(reply.to_string().contains("is unavailable"), "{reply}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "removed scope kept answering: {reply}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        parent
            .tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("outside.rs")
    );
    std::fs::create_dir_all(sub.join("src")).unwrap();
    std::fs::write(sub.join("src/restored.rs"), "fn UniqueNeedle() {}\n").unwrap();
    let deadline = Instant::now() + EVENTUAL;
    loop {
        let reply = child.raw_request(
            "tools/call",
            json!({"name":"grep","arguments":{"query":"UniqueNeedle"}}),
        );
        if reply.to_string().contains("src/restored.rs") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "restored scope never answered: {reply}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    wait_status(&service, "restored scope never shared", shared_by(2));
    let pids = service_pids(&service);
    drop(child);
    drop(parent);
    wait_stopped(&service, &pids);
}

#[test]
fn parent_and_child_share_and_preserve_scope_in_both_orders() {
    for child_first in [false, true] {
        let tmp = Sandbox::new();
        let service = tmp.service();
        let (project, sub) = fixture(tmp.path());
        let mut first = Client::start(if child_first { &sub } else { &project }, &service);
        let mut second = Client::start(if child_first { &project } else { &sub }, &service);
        let stats = wait_status(&service, "scopes never shared", index_count(1));
        assert_eq!(stats["sessions"].as_array().unwrap().len(), 2);
        let (parent, child) = if child_first {
            (&mut second, &mut first)
        } else {
            (&mut first, &mut second)
        };
        assert!(
            parent
                .tool("grep", json!({"query":"UniqueNeedle"}))
                .contains("outside.rs")
        );
        for (tool, args) in [
            ("grep", json!({"query":"UniqueNeedle"})),
            ("grep", json!({"query":"UniqueNedle"})),
            (
                "multi_grep",
                json!({"patterns":["UniqueNeedle"],"constraints":"src/"}),
            ),
            (
                "multi_grep",
                json!({"patterns":["UniqueNeedle"],"constraints":"src/*.rs","output_mode":"count"}),
            ),
            ("find_files", json!({"query":"inside"})),
        ] {
            let text = child.tool(tool, args);
            assert!(text.contains("src/inside.rs"), "{tool}: {text}");
            assert!(
                !text.contains("outside.rs") && !text.contains("sub/src"),
                "{tool}: {text}"
            );
        }
        let pids = service_pids(&service);
        drop(second);
        assert!(
            first
                .tool("grep", json!({"query":"UniqueNeedle"}))
                .contains("inside.rs")
        );
        drop(first);
        wait_stopped(&service, &pids);
    }
}

#[test]
fn concurrent_sessions_search_one_index() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    for group in 0..30 {
        let dir = project.join(format!("bulk/group_{group}"));
        std::fs::create_dir_all(&dir).unwrap();
        for index in 0..100 {
            let body = if group < 15 && index == group {
                "fn SharedMarker() {}\n"
            } else {
                "fn filler() {}\n"
            };
            std::fs::write(dir.join(format!("file_{index}.rs")), body).unwrap();
        }
    }
    let mut clients = [
        Client::start(&project, &service),
        Client::start(&project, &service),
    ];
    wait_status(&service, "sessions never shared", index_count(1));
    let calls = [
        ("grep", json!({"query":"SharedMarker"})),
        ("grep", json!({"query":"Shared[M]arker"})),
        ("grep", json!({"query":"SharedMarkr"})),
        (
            "multi_grep",
            json!({"patterns":["SharedMarker","UniqueNeedle"],"constraints":"*.rs"}),
        ),
        ("find_files", json!({"query":"inside"})),
        ("find_files", json!({"query":"group_1 file_1"})),
    ];
    // Cursor ids are minted per call; everything else must match.
    let call = |client: &mut Client, tool: &str, args: &Value| {
        client
            .tool(tool, args.clone())
            .lines()
            .filter(|line| !line.starts_with("cursor: "))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let expected: Vec<String> = calls
        .iter()
        .map(|(tool, args)| call(&mut clients[0], tool, args))
        .collect();
    assert!(
        expected[0].contains("bulk/group_14/file_14.rs"),
        "{}",
        expected[0]
    );
    std::thread::scope(|scope| {
        for client in &mut clients {
            let (calls, expected, call) = (&calls, &expected, &call);
            scope.spawn(move || {
                for _ in 0..5 {
                    for ((tool, args), expected) in calls.iter().zip(expected) {
                        assert_eq!(&call(client, tool, args), expected, "{tool} {args}");
                    }
                }
            });
        }
    });
    let pids = service_pids(&service);
    drop(clients);
    wait_stopped(&service, &pids);
}

#[test]
fn concurrent_startup_uses_one_service_and_one_index() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let clients: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| Client::start(&project, &service)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let stats = status(&service);
    assert_eq!(stats["index_count"], 1, "{stats}");
    assert_eq!(stats["sessions"].as_array().unwrap().len(), 4);
    assert_eq!(report(&service)["services"].as_array().unwrap().len(), 1);
    let pids = service_pids(&service);
    drop(clients);
    wait_stopped(&service, &pids);
}

#[test]
fn ignored_descendant_keeps_its_own_coverage() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = fixture(tmp.path());
    git2::Repository::init(&project).unwrap();
    std::fs::write(project.join(".gitignore"), "sub/\n").unwrap();
    let mut parent = Client::start(&project, &service);
    let mut child = Client::start(&sub, &service);
    let outer = parent.tool("multi_grep", json!({"patterns":["UniqueNeedle"]}));
    assert!(
        outer.contains("outside.rs") && !outer.contains("inside.rs"),
        "{outer}"
    );
    let inner = child.tool("grep", json!({"query":"UniqueNeedle"}));
    assert!(
        inner.contains("src/inside.rs") && !inner.contains("outside.rs"),
        "{inner}"
    );
    assert_eq!(status(&service)["index_count"], 2);
    let pids = service_pids(&service);
    drop(parent);
    drop(child);
    wait_stopped(&service, &pids);
}

#[test]
fn watcher_updates_shared_content_and_cursors_are_session_bound() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = fixture(tmp.path());
    for n in 0..6 {
        std::fs::write(sub.join(format!("page{n}.txt")), "BeforeChange").unwrap();
    }
    let mut first = Client::start_with(&project, &service, &["--content-indexing"]);
    let mut second = Client::start_with(&sub, &service, &["--content-indexing"]);
    let page = second.tool("find_files", json!({"query":"page", "maxResults":2}));
    let cursor = page
        .lines()
        .find_map(|l| l.strip_prefix("cursor: "))
        .expect("pagination cursor");
    let next = second.tool(
        "find_files",
        json!({"query":"page", "maxResults":2, "cursor":cursor}),
    );
    let files: Vec<_> = page.lines().filter(|l| l.ends_with(".txt")).collect();
    assert_eq!(files.len(), 2);
    assert!(
        next.lines()
            .filter(|l| l.ends_with(".txt"))
            .all(|l| !files.contains(&l))
    );
    let foreign = first.raw_request(
        "tools/call",
        json!({"name":"find_files","arguments":{"query":"page","cursor":cursor}}),
    );
    assert!(
        foreign.get("error").is_some() || foreign["result"]["isError"] == true,
        "{foreign}"
    );
    let fallback = second.tool(
        "find_files",
        json!({"query":"page txt missingterm", "maxResults":2}),
    );
    let cursor = fallback
        .lines()
        .find_map(|line| line.strip_prefix("cursor: "))
        .unwrap();
    let next = second.tool(
        "find_files",
        json!({"query":"page txt missingterm", "maxResults":2, "cursor":cursor}),
    );
    let files: Vec<_> = fallback
        .lines()
        .filter(|line| line.ends_with(".txt"))
        .collect();
    assert_eq!(files.len(), 2);
    let next_files: Vec<_> = next.lines().filter(|line| line.ends_with(".txt")).collect();
    assert_eq!(
        next_files.len(),
        2,
        "fallback pagination lost its query: {next}"
    );
    assert!(next_files.iter().all(|line| !files.contains(line)));
    std::fs::write(sub.join("fresh.txt"), "AfterChange").unwrap();
    let deadline = Instant::now() + EVENTUAL;
    loop {
        let text = second.tool("multi_grep", json!({"patterns":["AfterChange"]}));
        if text.contains("fresh.txt") {
            break;
        }
        assert!(Instant::now() < deadline, "watcher did not update: {text}");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        first
            .tool("grep", json!({"query":"AfterChange"}))
            .contains("sub/fresh.txt")
    );
    let pids = service_pids(&service);
    drop(first);
    drop(second);
    wait_stopped(&service, &pids);
}

#[test]
fn interleaved_searches_keep_their_cursors() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    for n in 0..4 {
        std::fs::write(project.join(format!("page{n}.txt")), "").unwrap();
        std::fs::write(project.join(format!("note{n}.md")), "").unwrap();
    }
    let mut client = Client::start(&project, &service);
    let cursor = |text: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix("cursor: "))
            .expect("pagination cursor")
            .to_owned()
    };
    let pages = cursor(&client.tool("find_files", json!({"query":"page", "maxResults":1})));
    let notes = cursor(&client.tool("find_files", json!({"query":"note", "maxResults":1})));
    let next = client.tool(
        "find_files",
        json!({"query":"page", "maxResults":1, "cursor":pages}),
    );
    assert!(next.contains("page") && next.contains(".txt"), "{next}");
    let next = client.tool(
        "find_files",
        json!({"query":"note", "maxResults":1, "cursor":notes}),
    );
    assert!(next.contains("note") && next.contains(".md"), "{next}");
    let mismatched = client.raw_request(
        "tools/call",
        json!({"name":"find_files","arguments":{"query":"note","cursor":pages}}),
    );
    assert!(
        mismatched.get("error").is_some() || mismatched["result"]["isError"] == true,
        "{mismatched}"
    );
    let pids = service_pids(&service);
    drop(client);
    wait_stopped(&service, &pids);
}

#[cfg(unix)]
#[test]
fn canonical_aliases_share_an_index() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let alias = tmp.path().join("alias");
    std::os::unix::fs::symlink(&project, &alias).unwrap();
    let first = Client::start(&project, &service);
    let second = Client::start(&alias, &service);
    assert_eq!(status(&service)["index_count"], 1);
    let pids = service_pids(&service);
    drop(first);
    drop(second);
    wait_stopped(&service, &pids);
}

#[test]
fn incompatible_options_keep_separate_indexes_in_one_service() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    // Idle exit is disabled so only the explicit stop can end the service.
    let first = Client::start_configured(BIN, &project, &service, "0", &[], &[]);
    let second = Client::start_configured(BIN, &project, &service, "0", &["--no-watch"], &[]);
    let stats = status(&service);
    assert_eq!(stats["index_count"], 2);
    assert_eq!(stats["sessions"].as_array().unwrap().len(), 2);
    let stop = stop_service(&service);
    assert!(!stop.status.success(), "must not stop connected clients");
    assert!(
        String::from_utf8_lossy(&stop.stdout).contains("2 connected clients"),
        "{}",
        String::from_utf8_lossy(&stop.stdout)
    );
    let pids = service_pids(&service);
    drop(first);
    drop(second);
    wait_status(&service, "sessions did not end", |s| {
        s["sessions"].as_array().unwrap().is_empty()
    });
    let stopped = stop_service(&service);
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    wait_stopped(&service, &pids);
}

#[test]
fn frecency_database_is_part_of_index_compatibility() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let first_db = tmp.path().join("frecency-a");
    let second_db = tmp.path().join("frecency-b");
    let first_db = first_db.to_str().unwrap();
    let second_db = second_db.to_str().unwrap();
    let mut first = Client::start_with(&project, &service, &["--frecency-db", first_db]);
    let second = Client::start_with(&project, &service, &["--frecency-db", first_db]);
    assert_eq!(status(&service)["index_count"], 1);
    let third = Client::start_with(&project, &service, &["--frecency-db", second_db]);
    assert_eq!(status(&service)["index_count"], 2);
    // An unusable database only disables frecency ranking.
    let blocker = tmp.path().join("blocker");
    std::fs::write(&blocker, "").unwrap();
    let broken = blocker.join("frecency");
    let mut fourth = Client::start_with(
        &project,
        &service,
        &["--frecency-db", broken.to_str().unwrap()],
    );
    assert!(
        fourth
            .tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("inside.rs")
    );
    assert!(
        first
            .tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("inside.rs")
    );
    let pids = service_pids(&service);
    drop((first, second, third, fourth));
    wait_stopped(&service, &pids);
}

#[test]
fn crashed_service_fails_clients_and_recovers_stale_discovery() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let mut daemon = start_daemon(&service, "0");
    let mut first = Client::start(&project, &service);
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    let exit = first
        .wait_exit(Duration::from_secs(5))
        .expect("client ignored service failure");
    assert!(!exit.success());
    drop(first);
    let mut replacement = Client::start(&project, &service);
    assert!(
        replacement
            .tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("inside.rs")
    );
    let pids = service_pids(&service);
    drop(replacement);
    wait_stopped(&service, &pids);
    #[cfg(unix)]
    assert!(
        std::fs::read_dir(&service)
            .unwrap()
            .flatten()
            .all(|entry| entry.path().extension() != Some("sock".as_ref())),
        "socket left behind"
    );
}

#[test]
fn service_without_idle_timeout_exits_once_its_namespace_is_removed() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let mut daemon = start_daemon(&service, "0");
    std::fs::remove_dir_all(&service).unwrap();
    let deadline = Instant::now() + EVENTUAL;
    while daemon.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "unreachable service kept running"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn orphaned_service_finishes_its_sessions_and_spares_its_replacement() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let mut old = Client::start_configured(BIN, &project, &service, "0", &[], &[]);
    let orphan = service_pids(&service);
    // A reset namespace lets the next client start a replacement service.
    std::fs::remove_dir_all(&service).unwrap();
    let mut new = Client::start(&project, &service);
    let replacement = service_pids(&service);
    assert!(orphan.iter().all(|pid| !replacement.contains(pid)));
    assert!(
        old.tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("inside.rs")
    );
    assert!(orphan.iter().all(|&pid| process_alive(pid)));
    drop(old);
    let deadline = Instant::now() + EVENTUAL;
    while orphan.iter().any(|&pid| process_alive(pid)) {
        assert!(Instant::now() < deadline, "orphaned service kept running");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A fresh connection proves the orphan left the replacement's socket in place.
    assert_eq!(status(&service)["pid"], replacement[0]);
    assert!(
        new.tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("inside.rs")
    );
    drop(new);
    wait_stopped(&service, &replacement);
}

// A service stopping after its namespace was replaced must not unlink the successor's socket.
#[cfg(unix)]
#[test]
fn stopped_service_spares_the_socket_of_its_replacement() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let mut daemon = start_daemon(&service, "0");
    let old = service_pids(&service);
    let name = endpoints(&service)[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let tag = name
        .strip_prefix("endpoint-")
        .unwrap()
        .strip_suffix(".json")
        .unwrap();
    let socket = service.join(format!("{tag}.sock"));
    // Holding the startup lock keeps the stop pending until the replacement is running.
    let startup = std::fs::File::options()
        .read(true)
        .write(true)
        .open(service.join(format!("startup-{tag}.lock")))
        .unwrap();
    startup.lock().unwrap();
    let hello = json!({"token":endpoint_token(&service),"protocol":2,"request":{"request":"stop"}});
    let (recv, mut send) = connect(&service).split();
    send.write_all(format!("{hello}\n").as_bytes()).unwrap();
    send.flush().unwrap();
    // Lets the stop request reach the service loop before the namespace disappears.
    std::thread::sleep(Duration::from_millis(500));
    std::fs::remove_dir_all(&service).unwrap();
    let mut new = Client::start(&project, &service);
    let replacement = service_pids(&service);
    assert!(old.iter().all(|pid| !replacement.contains(pid)));
    drop(startup);
    let mut reply = String::new();
    BufReader::new(recv).read_line(&mut reply).unwrap();
    assert!(reply.contains(r#""reply":"stopped""#), "{reply}");
    drop(send);
    let deadline = Instant::now() + EVENTUAL;
    while daemon.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "stopped service kept running");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        socket.exists(),
        "stopped service removed its replacement's socket"
    );
    assert_eq!(status(&service)["pid"], replacement[0]);
    assert!(
        new.tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("inside.rs")
    );
    drop(new);
    wait_stopped(&service, &replacement);
}

#[test]
fn sandbox_stops_services_left_by_a_panicking_test() {
    let pids = std::sync::Mutex::new(Vec::new());
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let tmp = Sandbox::new();
        let service = tmp.service();
        let (project, _) = fixture(tmp.path());
        let _client = Client::start_configured(BIN, &project, &service, "0", &[], &[]);
        pids.lock().unwrap().extend(service_pids(&service));
        panic!("simulated test failure");
    }));
    assert!(failed.is_err());
    let pids = pids.into_inner().unwrap();
    assert!(!pids.is_empty());
    assert!(
        pids.iter().all(|&pid| !process_alive(pid)),
        "services {pids:?} outlived their test"
    );
}

#[test]
fn different_builds_run_side_by_side() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let other = tmp.path().join(Path::new(BIN).file_name().unwrap());
    std::fs::copy(BIN, &other).unwrap();
    // Trailing bytes change the build identity without changing behaviour.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&other)
        .unwrap()
        .write_all(b"other build")
        .unwrap();
    let other = other.to_str().unwrap();
    let mut old = Client::start_configured(other, &project, &service, "0", &[], &[]);
    let mut new = Client::start_configured(BIN, &project, &service, "0", &[], &[]);
    let services = report(&service)["services"].as_array().unwrap().clone();
    assert_eq!(services.len(), 2, "{services:?}");
    assert_eq!(services.iter().filter(|s| s["current"] == true).count(), 1);
    assert!(services.iter().all(|s| s["index_count"] == 1));
    for client in [&mut old, &mut new] {
        assert!(
            client
                .tool("grep", json!({"query":"UniqueNeedle"}))
                .contains("inside.rs")
        );
    }
    let pids = service_pids(&service);
    assert!(!stop_service(&service).status.success());
    drop(old);
    let stop = cli(other, "--service-stop", &service);
    assert!(!stop.status.success(), "stopped a build with clients");
    drop(new);
    let deadline = Instant::now() + EVENTUAL;
    while !stop_service(&service).status.success() {
        assert!(Instant::now() < deadline, "idle services did not stop");
        std::thread::sleep(Duration::from_millis(100));
    }
    wait_stopped(&service, &pids);
}

#[test]
fn broader_coverage_invalidates_existing_cursor() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = fixture(tmp.path());
    for n in 0..5 {
        std::fs::write(sub.join(format!("page{n}.txt")), "needle").unwrap();
    }
    let mut child = Client::start(&sub, &service);
    let first = child.tool("find_files", json!({"query":"page", "maxResults":1}));
    let cursor = first
        .lines()
        .find_map(|line| line.strip_prefix("cursor: "))
        .unwrap();
    let parent = Client::start(&project, &service);
    wait_status(
        &service,
        "child never moved to the broader index",
        index_count(1),
    );
    let next = child.raw_request(
        "tools/call",
        json!({"name":"find_files","arguments":{"query":"page","cursor":cursor}}),
    );
    assert!(
        next.get("error").is_some() || next["result"]["isError"] == true,
        "{next}"
    );
    let pids = service_pids(&service);
    drop(parent);
    drop(child);
    wait_stopped(&service, &pids);
}

#[test]
fn exclusion_changes_preserve_descendant_coverage() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = fixture(tmp.path());
    git2::Repository::init(&project).unwrap();
    let mut parent = Client::start(&project, &service);
    let mut child = Client::start(&sub, &service);
    assert_eq!(status(&service)["index_count"], 1);
    std::fs::write(project.join(".gitignore"), "sub/\n").unwrap();
    let deadline = Instant::now() + EVENTUAL;
    loop {
        let result = parent.tool("multi_grep", json!({"patterns":["UniqueNeedle"]}));
        if !result.contains("inside.rs") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ignore change did not update parent coverage"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        child
            .tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("src/inside.rs")
    );
    assert_eq!(status(&service)["index_count"], 2);
    let pids = service_pids(&service);
    drop(parent);
    drop(child);
    wait_stopped(&service, &pids);
}

#[test]
fn new_repository_inside_scope_rebinds_the_session() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = fixture(tmp.path());
    let mut parent = Client::start(&project, &service);
    let mut child = Client::start(&sub, &service);
    assert!(
        child
            .tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("src/inside.rs")
    );
    assert_eq!(status(&service)["index_count"], 1);
    git2::Repository::init(&sub).unwrap();
    let inner = child.tool("grep", json!({"query":"UniqueNeedle"}));
    assert!(
        inner.contains("src/inside.rs") && !inner.contains("outside.rs"),
        "{inner}"
    );
    let stats = status(&service);
    assert_eq!(stats["index_count"], 2, "{stats}");
    let sub = sub.canonicalize().unwrap();
    assert!(
        stats["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["root"] == json!(sub)),
        "{stats}"
    );
    assert!(
        parent
            .tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("outside.rs")
    );
    let pids = service_pids(&service);
    drop(parent);
    drop(child);
    wait_stopped(&service, &pids);
}

#[test]
fn independent_repository_and_snapshot_scopes_stay_separate() {
    for nested_repository in [false, true] {
        let tmp = Sandbox::new();
        let service = tmp.service();
        let (project, sub) = fixture(tmp.path());
        let extra = if nested_repository {
            git2::Repository::init(&project).unwrap();
            git2::Repository::init(&sub).unwrap();
            vec![]
        } else {
            vec!["--no-watch"]
        };
        let mut child = Client::start_with(&sub, &service, &extra);
        let parent = Client::start_with(&project, &service, &extra);
        assert!(
            child
                .tool("grep", json!({"query":"UniqueNeedle"}))
                .contains("src/inside.rs")
        );
        assert_eq!(status(&service)["index_count"], 2);
        let pids = service_pids(&service);
        drop(parent);
        drop(child);
        wait_stopped(&service, &pids);
    }
}

#[test]
fn handshake_rejects_bad_tokens_protocols_and_oversized_hellos() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let _daemon = start_daemon(&service, "0");
    let token = endpoint_token(&service);
    let hello = |token: &str, protocol: u32| {
        format!(
            "{}\n",
            json!({"token":token,"protocol":protocol,"request":{"request":"status"}})
        )
    };
    let reply = raw_hello(&service, hello("wrong", 2).as_bytes());
    assert_eq!(reply["error"]["kind"], "unauthorized", "{reply}");
    let reply = raw_hello(&service, hello(&token, 999).as_bytes());
    assert_eq!(reply["error"]["kind"], "protocol", "{reply}");
    let reply = raw_hello(&service, &vec![b' '; 70 * 1024]);
    assert_eq!(reply["error"]["kind"], "invalid_request", "{reply}");
    let reply = raw_hello(&service, hello(&token, 2).as_bytes());
    assert_eq!(reply["reply"], "status", "{reply}");
    assert!(stop_service(&service).status.success());
}

#[test]
fn clients_connecting_during_idle_shutdown_start_a_new_service() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let mut pids = Vec::new();
    // Reconnect around the one-second idle deadline, so some attempts race the shutdown.
    for delay in [900, 950, 1000, 1050, 1100, 1150] {
        let mut client = Client::start(&project, &service);
        assert!(
            client
                .tool("grep", json!({"query":"UniqueNeedle"}))
                .contains("inside.rs")
        );
        pids.extend(service_pids(&service));
        drop(client);
        std::thread::sleep(Duration::from_millis(delay));
    }
    pids.sort_unstable();
    pids.dedup();
    wait_stopped(&service, &pids);
}

#[test]
fn status_polling_does_not_keep_an_idle_service_alive() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let client = Client::start(&project, &service);
    let pids = service_pids(&service);
    drop(client);
    let deadline = Instant::now() + EVENTUAL;
    while !endpoints(&service).is_empty() {
        assert!(
            Instant::now() < deadline,
            "status polling kept the service alive"
        );
        let _ = cli(BIN, "--service-status", &service);
        std::thread::sleep(Duration::from_millis(100));
    }
    wait_stopped(&service, &pids);
}

#[test]
fn client_without_parent_exits_after_idle_timeout() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    // A client with a live parent keeps the service up, so its processes are known.
    let anchor = Client::start(&project, &service);
    let pids = service_pids(&service);
    // Nothing is sent, so no pending reply competes with the idle timeout.
    let mut client = Daemon(
        client_command(BIN, &project, &service, "1", &[])
            .env("FFF_MCP_TEST_NO_PARENT", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn_serialized()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let exit = loop {
        if let Some(exit) = client.try_wait().unwrap() {
            break exit;
        }
        assert!(
            Instant::now() < deadline,
            "parentless client ignored its idle timeout"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(exit.success());
    drop(anchor);
    wait_stopped(&service, &pids);
}

#[cfg(unix)]
#[test]
fn home_and_root_scopes_require_opt_in() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let home = tmp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(home.join("note.txt"), "HomeMarker").unwrap();
    for scope in [home.as_path(), Path::new("/")] {
        let refused = client_command(BIN, scope, &service, "1", &[])
            .env("HOME", &home)
            .output_serialized()
            .unwrap();
        assert!(!refused.status.success());
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(stderr.contains("requires explicit opt-in"), "{stderr}");
    }
    let mut allowed = Client::start_configured(
        BIN,
        &home,
        &service,
        "1",
        &["--enable-home-scan"],
        &[("HOME", home.to_str().unwrap())],
    );
    assert!(
        allowed
            .tool("grep", json!({"query":"HomeMarker"}))
            .contains("note.txt")
    );
    let pids = service_pids(&service);
    drop(allowed);
    wait_stopped(&service, &pids);
}

#[cfg(unix)]
#[test]
fn shared_or_public_namespaces_are_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = Sandbox::new();
    let (project, _) = fixture(tmp.path());
    let public = tmp.path().join("public");
    std::fs::create_dir(&public).unwrap();
    std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755)).unwrap();
    let writable = tmp.path().join("writable");
    std::fs::create_dir(&writable).unwrap();
    std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o777)).unwrap();
    for service in [public, writable.join("service")] {
        let refused = client_command(BIN, &project, &service, "1", &[])
            .output_serialized()
            .unwrap();
        assert!(!refused.status.success());
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains("private") || stderr.contains("writable by others"),
            "{stderr}"
        );
        assert!(endpoints(&service).is_empty());
    }
}

// The service never checks upstream for updates and never advises installing
// upstream's build, even without --no-update-check.
#[cfg(unix)]
#[test]
fn the_service_never_checks_upstream_for_updates() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("main.rs"), "fn main() {}\n").unwrap();

    // Upstream's check runs curl against GitHub. This curl answers as GitHub would
    // for a newer upstream release, and leaves a mark when anything runs it.
    let bin = tmp.path().join("bin");
    let ran = tmp.path().join("curl-ran");
    std::fs::create_dir_all(&bin).unwrap();
    let curl = bin.join("curl");
    std::fs::write(
        &curl,
        format!(
            "#!/bin/sh\ntouch '{}'\necho '{{\"tag_name\":\"v99.0.0\"}}'\n",
            ran.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());

    // The service is the process that would check; it exits once idle.
    let service = tmp.path().join("service");
    let mut daemon = Daemon(
        Command::new(BIN)
            .args(["--service", "--idle-timeout-secs", "1"])
            .arg("--service-dir")
            .arg(&service)
            .arg("--log-file")
            .arg(service.with_extension("log"))
            .env("PATH", &path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn_serialized()
            .unwrap(),
    );
    wait_for_discovery(&mut daemon, &service);

    let mut client = Command::new(BIN)
        .arg(&project)
        .args(["--no-warmup", "--no-watch", "--idle-timeout-secs", "1"])
        .arg("--service-dir")
        .arg(&service)
        .arg("--log-file")
        .arg(service.with_extension("log"))
        .env("PATH", &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn_serialized()
        .unwrap();
    let mut input = client.stdin.take().unwrap();
    let mut output = BufReader::new(client.stdout.take().unwrap()).lines();
    let mut request = |id: u64, method: &str, params: Value| -> Value {
        let message = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        writeln!(input, "{message}").unwrap();
        input.flush().unwrap();
        loop {
            let line = output.next().expect("MCP response").unwrap();
            if let Ok(reply) = serde_json::from_str::<Value>(&line)
                && reply["id"] == id
            {
                return reply;
            }
        }
    };

    let init = request(
        1,
        "initialize",
        json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"update-notice-test","version":"1"}}),
    );
    // Upstream appends its notice to the first tool result after the check answers.
    let found = request(
        2,
        "tools/call",
        json!({"name":"find_files","arguments":{"query":"main"}}),
    )["result"]
        .to_string();
    assert!(found.contains("main.rs"), "{found}");
    for reply in [init.to_string(), found] {
        assert!(!reply.contains("update available"), "{reply}");
        assert!(!reply.contains("install-mcp.sh"), "{reply}");
    }

    // Closing the transport ends the session; the idle service then exits, so
    // whatever it started has run by then.
    drop(input);
    assert!(client.wait().unwrap().success());
    daemon.wait().unwrap();
    assert!(!ran.exists(), "fff-mcp ran curl to check for updates");
}
