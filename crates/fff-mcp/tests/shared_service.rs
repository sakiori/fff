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

use fff::file_picker::{FFFMode, FilePicker};
use fff::types::PaginationArgs;
use fff::{FilePickerOptions, FuzzySearchOptions, QueryParser};
use fff_service::client::ClientError;
use fff_service::program::{
    GlobRequest, GrepCursor, GrepMode, GrepOptions, GrepRequest, GrepResults, MixedEntry,
    MultiGrepRequest, ProgramError, SearchRequest,
};
use fff_service::protocol::{IndexOptions, ServiceError};
use fff_service::session::{CONNECT_TIMEOUT, ProgramConfig, ProgramSession, RESPONSE_TIMEOUT};
use fff_service::watch::{Delivery, Subscription, WatchEvent, WatchEventKind, WatchRequest};
use interprocess::local_socket::{Stream, prelude::*};
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_fff-mcp");
const PROTOCOL: u32 = fff_service::protocol::PROTOCOL;
const EVENTUAL: Duration = Duration::from_secs(15);
// The longest a test waits for one reply, from a client, a program or the service.
const REPLY: Duration = Duration::from_secs(90);
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
        let child = client_command(binary, scope, service, idle, extra)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn_isolated()
            .unwrap();
        Self::attach(child)
    }

    // Initializes a client spawned with piped standard input and output.
    fn attach(child: Child) -> Self {
        let mut client = Self::wrap(child);
        let result = client.request("initialize", json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"shared-service-test","version":"1"}}));
        assert!(result["serverInfo"].is_object(), "{result}");
        client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        client
    }

    // Talks to a client spawned with piped standard input and output, not initialized yet.
    fn wrap(mut child: Child) -> Self {
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
        Self {
            child,
            input,
            output,
            id: 0,
        }
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
                .recv_timeout(REPLY)
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
        exited_within(&mut self.child, timeout)
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

fn exited_within(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

// Lines a child writes, read on a thread so a test can wait for them with a deadline.
fn lines_of(output: impl std::io::Read + Send + 'static) -> mpsc::Receiver<String> {
    let (sender, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    lines
}

// Waits for a line containing `text` and returns whether one came before the output ended.
fn line_containing(lines: &mpsc::Receiver<String>, text: &str) -> bool {
    let deadline = Instant::now() + REPLY;
    loop {
        match lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(line) if line.contains(text) => return true,
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("no line containing {text:?}"),
        }
    }
}

// The remaining lines once the child exited.
fn rest_of(lines: mpsc::Receiver<String>) -> Vec<String> {
    let deadline = Instant::now() + REPLY;
    let mut rest = Vec::new();
    loop {
        match lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(line) => rest.push(line),
            Err(mpsc::RecvTimeoutError::Disconnected) => return rest,
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("output still open: {rest:?}"),
        }
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
    cli_with(binary, flag, service, &[])
}

fn cli_with(
    binary: &str,
    flag: &str,
    service: &Path,
    env: &[(&str, &str)],
) -> std::process::Output {
    Command::new(binary)
        .envs(env.iter().copied())
        .arg(flag)
        .arg("--service-dir")
        .arg(service)
        .arg("--log-file")
        .arg(service.with_extension("log"))
        .output_isolated()
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
    start_daemon_with(service, idle, &[])
}

fn start_daemon_with(service: &Path, idle: &str, env: &[(&str, &Path)]) -> Daemon {
    let mut child = Daemon(
        Command::new(BIN)
            .envs(env.iter().copied())
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
            .spawn_isolated()
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
    let (written, writer) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.write_all(&line);
        let _ = send.flush();
        let _ = written.send(send);
    });
    let reply = reply_line(recv);
    drop(
        writer
            .recv_timeout(REPLY)
            .expect("the hello was never written"),
    );
    serde_json::from_str(&reply).unwrap()
}

// One line the service sends, read on a thread, as a socket has no read timeout everywhere.
fn reply_line(recv: impl std::io::Read + Send + 'static) -> String {
    let (sender, reply) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(recv).read_line(&mut line);
        let _ = sender.send(line);
    });
    reply
        .recv_timeout(REPLY)
        .expect("no reply from the service")
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
    let second =
        Client::start_configured(BIN, &project, &service, "0", &["--follow-symlinks"], &[]);
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

// The service roots of the sessions, sorted.
fn session_roots(stats: &Value) -> Vec<String> {
    let mut roots: Vec<String> = stats["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["root"].as_str().unwrap().to_owned())
        .collect();
    roots.sort();
    roots
}

#[test]
fn content_indexing_is_a_capability_in_both_arrival_orders() {
    for content_first in [true, false] {
        let tmp = Sandbox::new();
        let service = tmp.service();
        let (project, _) = fixture(tmp.path());
        let content: &[&str] = &["--content-indexing"];
        let (first, second) = if content_first {
            (content, &[][..])
        } else {
            (&[][..], content)
        };
        let mut first = Client::start_with(&project, &service, first);
        let mut second = Client::start_with(&project, &service, second);
        if content_first {
            assert_eq!(
                status(&service)["index_count"],
                1,
                "paths-only request built an index"
            );
        }
        // A paths-only session moves to the content index once it is ready, and the
        // paths-only index is released.
        let stats = wait_status(
            &service,
            "sessions never shared the content index",
            shared_by(2),
        );
        assert_eq!(
            stats["indexes"][0]["options"]["content_indexing"], true,
            "{stats}"
        );
        for client in [&mut first, &mut second] {
            assert!(
                client
                    .tool("grep", json!({"query":"UniqueNeedle"}))
                    .contains("inside.rs")
            );
        }
        let pids = service_pids(&service);
        drop(first);
        drop(second);
        wait_stopped(&service, &pids);
    }
}

#[test]
fn symlink_policy_separates_indexes() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let mut plain = Client::start(&project, &service);
    let mut following = Client::start_with(&project, &service, &["--follow-symlinks"]);
    for client in [&mut plain, &mut following] {
        assert!(
            client
                .tool("grep", json!({"query":"UniqueNeedle"}))
                .contains("inside.rs")
        );
    }
    let stats = status(&service);
    assert_eq!(stats["index_count"], 2, "{stats}");
    let mut policies: Vec<_> = stats["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|index| index["options"]["follow_symlinks"].as_bool().unwrap())
        .collect();
    policies.sort();
    assert_eq!(policies, [false, true]);
    let pids = service_pids(&service);
    drop(plain);
    drop(following);
    wait_stopped(&service, &pids);
}

#[test]
fn snapshot_request_uses_a_watched_index_and_keeps_its_binding() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = fixture(tmp.path());
    let watched = Client::start(&sub, &service);
    let mut snapshot = Client::start_with(&sub, &service, &["--no-watch"]);
    assert_eq!(status(&service)["index_count"], 1);
    // Broader coverage moves the watched session but not the snapshot.
    let parent = Client::start(&project, &service);
    let sub = sub.canonicalize().unwrap().to_str().unwrap().to_owned();
    let project = project.canonicalize().unwrap().to_str().unwrap().to_owned();
    let mut expected = vec![project.clone(), project, sub];
    expected.sort();
    let stats = wait_status(&service, "watched session never moved", |stats| {
        session_roots(stats) == expected
    });
    assert_eq!(stats["index_count"], 2, "{stats}");
    assert!(
        snapshot
            .tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("src/inside.rs")
    );
    let pids = service_pids(&service);
    drop((watched, snapshot, parent));
    wait_stopped(&service, &pids);
}

// An index that snapshot sessions keep is not replaced, so a watched index does not copy it.
#[test]
fn watched_request_beside_a_snapshot_index_gets_only_what_it_needs() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let mut snapshot =
        Client::start_with(&project, &service, &["--no-watch", "--content-indexing"]);
    let mut watched = Client::start(&project, &service);
    for client in [&mut snapshot, &mut watched] {
        assert!(
            client
                .tool("find_files", json!({"query":"inside"}))
                .contains("inside.rs")
        );
    }
    let stats = status(&service);
    assert_eq!(stats["index_count"], 2, "{stats}");
    let options = |watch: bool| {
        stats["indexes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|index| index["options"]["watch"] == watch)
            .map(|index| index["options"].clone())
            .unwrap_or_else(|| panic!("no index with watch {watch}: {stats}"))
    };
    assert_eq!(options(true)["content_indexing"], false, "{stats}");
    assert_eq!(options(false)["content_indexing"], true, "{stats}");
    let pids = service_pids(&service);
    drop((snapshot, watched));
    wait_stopped(&service, &pids);
}

#[test]
fn request_without_frecency_database_shares_an_index_with_one() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let db = tmp.path().join("frecency");
    let with = Client::start_with(&project, &service, &["--frecency-db", db.to_str().unwrap()]);
    let without = Client::start(&project, &service);
    let stats = status(&service);
    assert_eq!(stats["index_count"], 1, "{stats}");
    assert!(stats["indexes"][0]["options"]["frecency_db"].is_string());
    let pids = service_pids(&service);
    drop((with, without));
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
    let hello =
        json!({"token":endpoint_token(&service),"protocol":PROTOCOL,"request":{"request":"stop"}});
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
    let reply = reply_line(recv);
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

// Debug builds take a build variant from the environment, standing in for another release.
const OTHER_BUILD: (&str, &str) = ("FFF_MCP_TEST_BUILD_ID", "other build");

#[test]
fn different_builds_run_side_by_side() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let mut old = Client::start_configured(BIN, &project, &service, "0", &[], &[OTHER_BUILD]);
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
    let stop = cli_with(BIN, "--service-stop", &service, &[OTHER_BUILD]);
    assert!(!stop.status.success(), "stopped a build with clients");
    drop(new);
    let deadline = Instant::now() + EVENTUAL;
    while !stop_service(&service).status.success() {
        assert!(Instant::now() < deadline, "idle services did not stop");
        std::thread::sleep(Duration::from_millis(100));
    }
    wait_stopped(&service, &pids);
}

// The identity is compiled in, so a copy that was moved, re-signed or otherwise
// changed after the build joins the original's service.
#[test]
fn copied_executable_joins_the_same_service() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let copy = tmp
        .path()
        .join("copy")
        .join(Path::new(BIN).file_name().unwrap());
    std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
    std::fs::copy(BIN, &copy).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&copy)
        .unwrap()
        .write_all(b"appended after the build")
        .unwrap();
    let copy = copy.to_str().unwrap();
    let mut original = Client::start(&project, &service);
    let mut copied = Client::start_configured(copy, &project, &service, "1", &[], &[]);
    let report = report(&service);
    let services = report["services"].as_array().unwrap();
    assert_eq!(services.len(), 1, "{report}");
    assert_eq!(services[0]["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(services[0]["index_count"], 1);
    for client in [&mut original, &mut copied] {
        assert!(
            client
                .tool("grep", json!({"query":"UniqueNeedle"}))
                .contains("inside.rs")
        );
    }
    let pids = service_pids(&service);
    drop(copied);
    drop(original);
    wait_stopped(&service, &pids);
}

#[test]
fn service_of_another_build_refuses_to_start_for_a_client() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let output = Command::new(BIN)
        .args(["--service", "--service-build", "another-build"])
        .arg("--service-dir")
        .arg(&service)
        .arg("--log-file")
        .arg(service.with_extension("log"))
        .output_isolated()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("needs build another-build"), "{stderr}");
    assert!(endpoints(&service).is_empty());
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
    let reply = raw_hello(&service, hello("wrong", PROTOCOL).as_bytes());
    assert_eq!(reply["error"]["kind"], "unauthorized", "{reply}");
    let reply = raw_hello(&service, hello(&token, 999).as_bytes());
    assert_eq!(reply["error"]["kind"], "protocol", "{reply}");
    let reply = raw_hello(&service, &vec![b' '; 70 * 1024]);
    assert_eq!(reply["error"]["kind"], "invalid_request", "{reply}");
    let reply = raw_hello(&service, hello(&token, PROTOCOL).as_bytes());
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
            .spawn_isolated()
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
            .output_isolated()
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
            .output_isolated()
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
            .spawn_isolated()
            .unwrap(),
    );
    wait_for_discovery(&mut daemon, &service);

    let mut client = Client::wrap(
        Command::new(BIN)
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
            .spawn_isolated()
            .unwrap(),
    );

    let init = client.raw_request(
        "initialize",
        json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"update-notice-test","version":"1"}}),
    );
    // Upstream appends its notice to the first tool result after the check answers.
    let found = client.raw_request(
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
    client.input.take();
    let exit = client.wait_exit(EVENTUAL).expect("client did not exit");
    assert!(exit.success());
    exited_within(&mut daemon, EVENTUAL).expect("idle service kept running");
    assert!(!ran.exists(), "fff-mcp ran curl to check for updates");
}

fn program_options() -> IndexOptions {
    IndexOptions {
        warmup: false,
        content_indexing: false,
        watch: true,
        follow_symlinks: false,
        max_cached_files: None,
        frecency_db: None,
        enable_home_scan: false,
        enable_root_scan: false,
    }
}

fn program_config(scope: &Path, service: &Path) -> ProgramConfig {
    ProgramConfig {
        scope: scope.to_path_buf(),
        options: program_options(),
        executable: BIN.into(),
        service_dir: Some(service.to_path_buf()),
        idle_timeout_secs: 1,
        log_file: Some(service.with_extension("log")),
        log_level: None,
        connect_timeout: CONNECT_TIMEOUT,
        response_timeout: RESPONSE_TIMEOUT,
    }
}

fn open_program(scope: &Path, service: &Path, options: IndexOptions) -> ProgramSession {
    let session = ProgramSession::open(&ProgramConfig {
        options,
        ..program_config(scope, service)
    })
    .unwrap();
    assert!(
        session.wait_ready(EVENTUAL).unwrap(),
        "index never became ready"
    );
    session
}

// An in-process index of `root` with the options the service uses for program sessions.
fn local_index(root: &Path) -> FilePicker {
    let mut picker = FilePicker::new(FilePickerOptions {
        base_path: root.canonicalize().unwrap().to_str().unwrap().into(),
        watch: false,
        mode: FFFMode::Ai,
        ..Default::default()
    })
    .unwrap();
    picker.collect_files().unwrap();
    picker
}

fn local_options(
    offset: usize,
    limit: usize,
    exclude_frecency: bool,
) -> FuzzySearchOptions<'static> {
    FuzzySearchOptions {
        pagination: PaginationArgs { offset, limit },
        exclude_frecency,
        ..Default::default()
    }
}

fn search(query: &str, offset: usize, limit: usize) -> SearchRequest {
    SearchRequest {
        query: query.into(),
        offset,
        limit,
        exclude_frecency: true,
        ..Default::default()
    }
}

fn grep_options(cursor: Option<GrepCursor>, page_size: usize) -> GrepOptions {
    GrepOptions {
        max_file_size: 10 * 1024 * 1024,
        smart_case: true,
        cursor,
        page_size,
        ..Default::default()
    }
}

fn scored<'a>(
    paths: impl Iterator<Item = String>,
    scores: impl Iterator<Item = &'a i32>,
) -> Vec<(String, i32)> {
    let mut pairs: Vec<_> = paths.zip(scores.copied()).collect();
    pairs.sort();
    pairs
}

type Line = (String, u64, String, Vec<(u32, u32)>);

fn service_lines(results: &GrepResults) -> Vec<Line> {
    results
        .matches
        .iter()
        .map(|m| {
            let path = results.files[m.file].relative_path.clone();
            (
                path,
                m.line_number,
                m.line_content.clone(),
                m.match_ranges.clone(),
            )
        })
        .collect()
}

fn local_lines(result: &fff::grep::GrepResult, picker: &FilePicker) -> Vec<Line> {
    result
        .matches
        .iter()
        .map(|m| {
            let path = result.files[m.file_index].relative_path(picker);
            let ranges = m.match_byte_offsets.to_vec();
            (path, m.line_number, m.line_content.clone(), ranges)
        })
        .collect()
}

fn program_fixture(root: &Path) -> PathBuf {
    let project = root.join("project");
    for (path, body) in [
        ("src/main.rs", "fn main() { UniqueNeedle(); }\n"),
        ("src/lib.rs", "pub fn UniqueNeedle() {}\nfn filler() {}\n"),
        ("src/nested/deep.rs", "// filler UniqueNeedle\n"),
        ("docs/guide.md", "Mentions UniqueNeedle and filler\n"),
        ("README.md", "filler only\n"),
    ] {
        let path = project.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    project
}

#[test]
fn program_results_match_an_in_process_index() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let project = program_fixture(tmp.path());
    let _daemon = start_daemon(&service, "0");
    let session = open_program(&project, &service, program_options());
    let local = local_index(&project);

    let progress = session.progress().unwrap();
    assert!(!progress.scanning);
    assert_eq!(progress.files, local.live_file_count());

    let listed = session.search_files(search("", 0, 0)).unwrap();
    let paths: Vec<_> = listed
        .items
        .iter()
        .map(|i| i.relative_path.clone())
        .collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "an empty query lists files in path order");
    let expected = local.fuzzy_search(
        &QueryParser::default().parse(""),
        None,
        local_options(0, 0, true),
    );
    let expected: Vec<_> = expected
        .items
        .iter()
        .map(|f| f.relative_path(&local))
        .collect();
    assert_eq!(paths, expected);
    let page = session.search_files(search("", 1, 2)).unwrap();
    let page: Vec<_> = page.items.iter().map(|i| i.relative_path.clone()).collect();
    assert_eq!(page, &paths[1..3]);

    let found = session.search_files(search("lib", 0, 10)).unwrap();
    let query = QueryParser::default().parse("lib");
    let expected = local.fuzzy_search(&query, None, local_options(0, 10, true));
    assert_eq!(found.total_matched, expected.total_matched);
    assert_eq!(found.total_files, expected.total_files);
    assert_eq!(
        scored(
            found.items.iter().map(|i| i.relative_path.clone()),
            found.scores.iter().map(|s| &s.total)
        ),
        scored(
            expected.items.iter().map(|f| f.relative_path(&local)),
            expected.scores.iter().map(|s| &s.total)
        ),
    );

    let dirs = session.search_directories(search("nest", 0, 10)).unwrap();
    let query = QueryParser::new(fff_query_parser::DirSearchConfig).parse("nest");
    let expected = local.fuzzy_search_directories(&query, local_options(0, 10, true));
    assert_eq!(
        scored(
            dirs.items.iter().map(|d| d.relative_path.clone()),
            dirs.scores.iter().map(|s| &s.total)
        ),
        scored(
            expected.items.iter().map(|d| d.relative_path(&local)),
            expected.scores.iter().map(|s| &s.total)
        ),
    );
    assert_eq!(dirs.total_dirs, expected.total_dirs);

    let mixed = session.search_mixed(search("", 0, 0)).unwrap();
    let query = QueryParser::new(fff_query_parser::MixedSearchConfig).parse("");
    let expected = local.fuzzy_search_mixed(&query, None, local_options(0, 0, true));
    let mixed: Vec<_> = mixed
        .items
        .iter()
        .map(|item| match item {
            MixedEntry::File(file) => file.relative_path.clone(),
            MixedEntry::Dir(dir) => dir.relative_path.clone(),
        })
        .collect();
    let expected: Vec<_> = expected
        .items
        .iter()
        .map(|item| match item {
            fff::types::MixedItemRef::File(file) => file.relative_path(&local),
            fff::types::MixedItemRef::Dir(dir) => dir.relative_path(&local),
        })
        .collect();
    assert_eq!(mixed, expected);

    let globbed = session
        .glob(GlobRequest {
            pattern: "**/*.rs".into(),
            exclude_frecency: true,
            ..Default::default()
        })
        .unwrap();
    let expected = local.glob("**/*.rs", local_options(0, 0, true));
    let globbed: Vec<_> = globbed
        .items
        .iter()
        .map(|i| i.relative_path.clone())
        .collect();
    let expected: Vec<_> = expected
        .items
        .iter()
        .map(|f| f.relative_path(&local))
        .collect();
    assert_eq!(globbed, expected);
    assert_eq!(globbed.len(), 3);

    for (query, mode) in [
        ("UniqueNeedle", GrepMode::PlainText),
        ("Unique[N]eedle", GrepMode::Regex),
        ("UniqNeedle", GrepMode::Fuzzy),
    ] {
        let found = session
            .grep(GrepRequest {
                query: query.into(),
                mode,
                ai_syntax: false,
                options: grep_options(None, 50),
            })
            .unwrap();
        let options = fff::grep::GrepSearchOptions {
            mode: match mode {
                GrepMode::PlainText => fff::grep::GrepMode::PlainText,
                GrepMode::Regex => fff::grep::GrepMode::Regex,
                GrepMode::Fuzzy => fff::grep::GrepMode::Fuzzy,
            },
            max_file_size: 10 * 1024 * 1024,
            smart_case: true,
            page_limit: 50,
            ..Default::default()
        };
        let expected = local.grep(&fff::grep::parse_grep_query(query), &options);
        assert_eq!(
            service_lines(&found),
            local_lines(&expected, &local),
            "{query}"
        );
        assert!(!found.matches.is_empty(), "{query}");
        assert_eq!(found.total_files_searched, expected.total_files_searched);
    }

    let found = session
        .multi_grep(MultiGrepRequest {
            patterns: vec!["UniqueNeedle".into(), "filler".into()],
            constraints: Some("*.md".into()),
            options: grep_options(None, 50),
        })
        .unwrap();
    let constraints = QueryParser::new(fff_query_parser::AiGrepConfig).parse_constraints("*.md");
    let options = fff::grep::GrepSearchOptions {
        max_file_size: 10 * 1024 * 1024,
        smart_case: true,
        page_limit: 50,
        ..Default::default()
    };
    let expected = local.multi_grep(&["UniqueNeedle", "filler"], &constraints, &options);
    assert_eq!(service_lines(&found), local_lines(&expected, &local));
    assert_eq!(found.files.len(), 2);

    let invalid = session
        .grep(GrepRequest {
            query: "Unique(Needle".into(),
            mode: GrepMode::Regex,
            ai_syntax: false,
            options: grep_options(None, 50),
        })
        .unwrap();
    assert!(invalid.regex_fallback_error.is_some());
    assert_eq!(session.repository().unwrap(), None);
    session.close().unwrap();
    wait_status(&service, "closed session stayed registered", |stats| {
        stats["sessions"].as_array().unwrap().is_empty()
    });
}

#[test]
fn program_grep_without_frecency_orders_files_as_an_index_without_one() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let project = program_fixture(tmp.path()).canonicalize().unwrap();
    // Older files come later, so frecency alone moves `README.md` to the front.
    let now = std::time::SystemTime::now();
    for (age, path) in ["src/main.rs", "src/lib.rs", "docs/guide.md", "README.md"]
        .iter()
        .enumerate()
    {
        let file = std::fs::File::options()
            .write(true)
            .open(project.join(path))
            .unwrap();
        file.set_modified(now - Duration::from_secs(60 * (age as u64 + 1)))
            .unwrap();
    }
    let db = tmp.path().join("frecency");
    let tracker = fff::frecency::FrecencyTracker::open(&db).unwrap();
    for _ in 0..5 {
        tracker.track_access(&project.join("README.md")).unwrap();
    }
    drop(tracker);
    let _daemon = start_daemon(&service, "0");
    let options = IndexOptions {
        frecency_db: Some(db.to_str().unwrap().into()),
        ..program_options()
    };
    let session = open_program(&project, &service, options);
    let local = local_index(&project);
    let order = |exclude_frecency: bool, multi: bool| {
        let options = GrepOptions {
            exclude_frecency,
            ..grep_options(None, 50)
        };
        let results = if multi {
            session.multi_grep(MultiGrepRequest {
                patterns: vec!["filler".into()],
                constraints: None,
                options,
            })
        } else {
            session.grep(GrepRequest {
                query: "filler".into(),
                options,
                ..Default::default()
            })
        };
        let results = results.unwrap();
        let paths = results.files.iter().map(|f| f.relative_path.clone());
        paths.collect::<Vec<_>>()
    };
    let local_options = fff::grep::GrepSearchOptions {
        max_file_size: 10 * 1024 * 1024,
        smart_case: true,
        ..Default::default()
    };
    let local_grep = local.grep(&fff::grep::parse_grep_query("filler"), &local_options);
    let expected: Vec<_> = local_grep
        .files
        .iter()
        .map(|f| f.relative_path(&local))
        .collect();
    assert_eq!(expected.last().unwrap(), "README.md");

    for multi in [false, true] {
        assert_eq!(order(false, multi)[0], "README.md", "frecency ranks first");
        assert_eq!(order(true, multi), expected);
    }
    session.close().unwrap();
}

#[test]
fn program_grep_cursor_continues_and_expires_when_the_session_moves() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = fixture(tmp.path());
    for n in 0..6 {
        std::fs::write(sub.join(format!("page{n}.txt")), "PageNeedle\n").unwrap();
    }
    let _daemon = start_daemon(&service, "0");
    let child = open_program(&sub, &service, program_options());
    let page = |cursor| {
        let mut options = grep_options(cursor, 2);
        options.max_matches_per_file = 1;
        GrepRequest {
            query: "PageNeedle".into(),
            options,
            ..Default::default()
        }
    };
    let first = child.grep(page(None)).unwrap();
    assert!(first.next_cursor.is_some());
    let second = child.grep(page(first.next_cursor.clone())).unwrap();
    let names = |results: &GrepResults| {
        results
            .files
            .iter()
            .map(|f| f.relative_path.clone())
            .collect::<Vec<_>>()
    };
    let (first_names, second_names) = (names(&first), names(&second));
    assert_eq!(first_names.len(), 2);
    assert_eq!(second_names.len(), 2);
    assert!(second_names.iter().all(|name| !first_names.contains(name)));
    assert!(first_names.iter().all(|name| name.starts_with("page")));
    // A broader index takes over the session; offsets into the old one expire.
    let parent = open_program(&project, &service, program_options());
    wait_status(
        &service,
        "child never moved to the broader index",
        index_count(1),
    );
    // A cursor names the index it came from, also after a grep on the new index.
    let fresh = child.grep(page(None)).unwrap();
    assert!(names(&fresh).iter().all(|name| name.starts_with("page")));
    assert_ne!(
        fresh.next_cursor.as_ref().map(|c| &c.index),
        second.next_cursor.as_ref().map(|c| &c.index)
    );
    let stale = child.grep(page(second.next_cursor.clone()));
    assert!(
        matches!(stale, Err(ClientError::Program(ProgramError::StaleCursor))),
        "{stale:?}"
    );
    let continued = child.grep(page(fresh.next_cursor.clone())).unwrap();
    assert!(
        names(&continued)
            .iter()
            .all(|name| !names(&fresh).contains(name))
    );
    let all = child.search_files(search("", 0, 0)).unwrap();
    assert!(
        all.items
            .iter()
            .all(|i| !i.relative_path.contains("outside"))
    );
    assert_eq!(all.total_matched, 7);
    assert_eq!(
        child.progress().unwrap().files,
        7,
        "progress counts the scope's files"
    );
    let seen = parent.grep(page(None)).unwrap();
    assert!(!seen.files.is_empty());
    assert!(names(&seen).iter().all(|name| name.starts_with("sub/page")));
}

#[test]
fn mcp_and_program_sessions_share_one_index() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let mut agent = Client::start(&project, &service);
    let program = open_program(&project, &service, program_options());
    let stats = status(&service);
    assert_eq!(stats["index_count"], 1, "{stats}");
    let mut kinds: Vec<_> = stats["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["kind"].as_str().unwrap().to_owned())
        .collect();
    kinds.sort();
    assert_eq!(kinds, ["mcp", "program"]);
    let found = program
        .grep(GrepRequest {
            query: "UniqueNeedle".into(),
            options: grep_options(None, 50),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(found.files.len(), 2);
    assert!(
        agent
            .tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("inside.rs")
    );
    let pids = service_pids(&service);
    drop(program);
    drop(agent);
    wait_stopped(&service, &pids);
}

// Holds a program session until it is killed; runs only when spawned by the test below.
#[test]
fn program_session_holder() {
    let (Some(scope), Some(service)) = (
        std::env::var_os("FFF_TEST_HOLDER_SCOPE"),
        std::env::var_os("FFF_TEST_HOLDER_SERVICE"),
    ) else {
        return;
    };
    if std::env::var_os("FFF_TEST_HOLDER_WAIT").is_some() {
        let session =
            ProgramSession::open(&program_config(Path::new(&scope), Path::new(&service))).unwrap();
        println!("holding");
        let waited = session.wait_ready(Duration::from_secs(60 * 60));
        println!("waited: {waited:?}");
        return;
    }
    let _session = open_program(Path::new(&scope), Path::new(&service), program_options());
    println!("holding");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

// Programs send one spelling of a frecency database, so equal databases share an index.
#[test]
fn program_sessions_with_one_frecency_database_share_an_index() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let state = tmp.path().join("state");
    std::fs::create_dir_all(state.join("nested")).unwrap();
    let _daemon = start_daemon(&service, "0");
    let with_database = |path: PathBuf| IndexOptions {
        frecency_db: Some(path.to_string_lossy().into_owned()),
        ..program_options()
    };
    let _first = open_program(&project, &service, with_database(state.join("frecency")));
    let _second = open_program(
        &project,
        &service,
        with_database(state.join("nested/../frecency")),
    );
    let stats = status(&service);
    assert_eq!(stats["index_count"], 1, "{stats}");
}

// A live service of another build under this build's file names fails the client at once.
#[test]
fn a_build_tag_collision_is_reported() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let namespace = fff_service::namespace::Namespace::open(&service).unwrap();
    let files = fff_service::Build::current().files();
    let owner = namespace.open_file(&files.owner_lock(), true).unwrap();
    owner.lock().unwrap();
    let endpoint = fff_service::protocol::Endpoint {
        name: "unreachable".into(),
        token: "token".into(),
        protocol: fff_service::protocol::PROTOCOL,
        build: "f".repeat(64),
        pid: std::process::id(),
    };
    let file = namespace.open_file(&files.endpoint(), true).unwrap();
    serde_json::to_writer(file, &endpoint).unwrap();
    let error = ProgramSession::open(&program_config(&project, &service))
        .err()
        .expect("opened a session beside a colliding service");
    assert!(
        matches!(error, ClientError::BuildCollision { .. }),
        "{error}"
    );
}

// A killed program ends its session also while the service works on its request.
#[test]
fn program_session_ends_when_its_process_exits_during_a_request() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let hold = tmp.path().join("hold-readiness");
    std::fs::write(&hold, "").unwrap();
    let _daemon = start_daemon_with(&service, "0", &[("FFF_MCP_TEST_HOLD_READINESS", &hold)]);
    let mut holder = Daemon(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "program_session_holder", "--nocapture"])
            .env("FFF_TEST_HOLDER_SCOPE", &project)
            .env("FFF_TEST_HOLDER_SERVICE", &service)
            .env("FFF_TEST_HOLDER_WAIT", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn_isolated()
            .unwrap(),
    );
    let lines = lines_of(holder.stdout.take().unwrap());
    assert!(
        line_containing(&lines, "holding"),
        "holder never opened its session"
    );
    wait_status(&service, "program session not registered", shared_by(1));
    holder.kill().unwrap();
    holder.wait().unwrap();
    let rest = rest_of(lines);
    assert!(rest.iter().all(|line| !line.contains("waited")), "{rest:?}");
    wait_status(&service, "a waiting program kept its session", |stats| {
        stats["sessions"].as_array().unwrap().is_empty()
    });
}

#[test]
fn program_session_ends_when_its_process_exits() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let mut agent = Client::start(&project, &service);
    let mut holder = Daemon(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "program_session_holder", "--nocapture"])
            .env("FFF_TEST_HOLDER_SCOPE", &project)
            .env("FFF_TEST_HOLDER_SERVICE", &service)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn_isolated()
            .unwrap(),
    );
    let lines = lines_of(holder.stdout.take().unwrap());
    assert!(
        line_containing(&lines, "holding"),
        "holder never opened its session"
    );
    wait_status(&service, "program session not registered", shared_by(2));
    holder.kill().unwrap();
    holder.wait().unwrap();
    let stats = wait_status(&service, "killed program kept its session", |stats| {
        stats["sessions"].as_array().unwrap().len() == 1
    });
    assert_eq!(stats["sessions"][0]["kind"], "mcp");
    assert_eq!(stats["index_count"], 1);
    assert!(
        agent
            .tool("grep", json!({"query":"UniqueNeedle"}))
            .contains("inside.rs")
    );
    let pids = service_pids(&service);
    drop(agent);
    wait_stopped(&service, &pids);
}

#[test]
fn program_rescan_and_repository_state() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = fixture(tmp.path());
    let snapshot = IndexOptions {
        watch: false,
        ..program_options()
    };
    let mut config = ProgramConfig {
        scope: sub.clone(),
        options: snapshot.clone(),
        executable: tmp.path().join("missing-executable"),
        service_dir: Some(service.clone()),
        idle_timeout_secs: 1,
        log_file: Some(service.with_extension("log")),
        log_level: None,
        connect_timeout: CONNECT_TIMEOUT,
        response_timeout: RESPONSE_TIMEOUT,
    };
    let missing = ProgramSession::open(&config)
        .err()
        .expect("opened without a service");
    assert!(matches!(missing, ClientError::Io(_)), "{missing}");
    // The session starts the service itself; a snapshot index runs no watcher.
    config.executable = BIN.into();
    let session = ProgramSession::open(&config).unwrap();
    assert!(session.wait_ready(EVENTUAL).unwrap());
    assert!(!session.is_scanning().unwrap());
    assert_eq!(session.repository().unwrap(), None);
    std::fs::write(sub.join("added.rs"), "fn AddedLater() {}\n").unwrap();
    let added = |session: &ProgramSession| {
        session
            .search_files(search("added", 0, 10))
            .unwrap()
            .items
            .iter()
            .any(|i| i.relative_path == "added.rs")
    };
    assert!(!added(&session), "a snapshot saw a file without a rescan");
    session.rescan().unwrap();
    assert!(session.wait_ready(EVENTUAL).unwrap());
    assert!(added(&session));
    git2::Repository::init(&project).unwrap();
    assert_eq!(
        session.repository().unwrap(),
        Some(project.canonicalize().unwrap())
    );
}

// Waits until `done` holds, polling the service.
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + EVENTUAL;
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// Program searches answer from what a building index holds, within the session's scope.
#[test]
fn program_searches_answer_while_the_index_builds() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = fixture(tmp.path());
    let hold = tmp.path().join("hold-readiness");
    std::fs::write(&hold, "").unwrap();
    let _daemon = start_daemon_with(&service, "0", &[("FFF_MCP_TEST_HOLD_READINESS", &hold)]);
    let _parent = ProgramSession::open(&program_config(&project, &service)).unwrap();
    let session = ProgramSession::open(&program_config(&sub, &service)).unwrap();
    let stats = status(&service);
    assert_eq!(stats["index_count"], 1, "{stats}");
    let paths = |session: &ProgramSession| -> Vec<String> {
        let files = session.search_files(search("", 0, 100)).unwrap();
        files.items.into_iter().map(|i| i.relative_path).collect()
    };
    // Before and after the walk is committed, only the session's own files appear.
    eventually("the building index never held the scope's files", || {
        let found = paths(&session);
        assert!(
            found.iter().all(|path| path == "src/inside.rs"),
            "{found:?}"
        );
        !found.is_empty()
    });
    let grep = session
        .grep(GrepRequest {
            query: "UniqueNeedle".into(),
            options: grep_options(None, 50),
            ..Default::default()
        })
        .unwrap();
    let grepped: Vec<_> = grep.files.iter().map(|f| &f.relative_path).collect();
    assert_eq!(grepped, ["src/inside.rs"]);
    session
        .glob(GlobRequest {
            pattern: "**/*.rs".into(),
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    session.search_directories(search("src", 0, 10)).unwrap();
    session.search_mixed(search("inside", 0, 10)).unwrap();
    assert!(
        session.is_scanning().unwrap(),
        "searches waited for the index"
    );
    std::fs::remove_file(&hold).unwrap();
    assert!(session.wait_ready(EVENTUAL).unwrap());
    assert_eq!(paths(&session), ["src/inside.rs"]);
}

// Scan state answers while another session's search holds the index locked.
#[test]
fn program_scan_state_answers_while_a_search_holds_the_index() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let hold = tmp.path().join("hold-search");
    let _daemon = start_daemon_with(&service, "0", &[("FFF_MCP_TEST_HOLD_SEARCH", &hold)]);
    let searcher = open_program(&project, &service, program_options());
    let observer = open_program(&project, &service, program_options());
    let before = observer.progress().unwrap();
    assert_eq!(before.files, 2);
    std::fs::write(&hold, "").unwrap();
    let search = std::thread::spawn(move || {
        searcher.grep(GrepRequest {
            query: "UniqueNeedle".into(),
            options: grep_options(None, 50),
            ..Default::default()
        })
    });
    let held = tmp.path().join("hold-search.held");
    eventually("the search never held the index", || held.exists());
    let during = observer.progress().unwrap();
    assert!(!during.scanning);
    assert_eq!(during.files, before.files);
    assert!(!observer.is_scanning().unwrap());
    assert_eq!(observer.repository().unwrap(), None);
    std::fs::remove_file(&hold).unwrap();
    assert_eq!(search.join().unwrap().unwrap().files.len(), 2);
}

// A call the service does not answer fails within its bound, and the next call opens a new
// session on the same service instead of failing for good.
#[test]
fn program_call_that_times_out_leaves_a_session_that_reconnects() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let hold = tmp.path().join("hold-search");
    let _daemon = start_daemon_with(&service, "0", &[("FFF_MCP_TEST_HOLD_SEARCH", &hold)]);
    let session = ProgramSession::open(&ProgramConfig {
        response_timeout: Duration::from_millis(500),
        ..program_config(&project, &service)
    })
    .unwrap();
    assert!(session.wait_ready(EVENTUAL).unwrap());
    std::fs::write(&hold, "").unwrap();
    let grep = |session: &ProgramSession| {
        session.grep(GrepRequest {
            query: "UniqueNeedle".into(),
            options: grep_options(None, 50),
            ..Default::default()
        })
    };
    let error = grep(&session).unwrap_err();
    assert!(matches!(error, ClientError::TimedOut(_)), "{error}");
    std::fs::remove_file(&hold).unwrap();
    // The lost session's index may already be released, so the reconnected session can land
    // on a fresh index that is still scanning.
    assert!(session.wait_ready(EVENTUAL).unwrap());
    assert_eq!(session.progress().unwrap().files, 2);
    assert_eq!(grep(&session).unwrap().files.len(), 2);
}

// Reopening a lost session counts against the call's own bound, not the connect timeout.
#[cfg(unix)]
#[test]
fn program_reconnect_to_an_unresponsive_service_stays_within_the_call_bound() {
    use rustix::process::{Pid, Signal, kill_process};

    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let hold = tmp.path().join("hold-search");
    let daemon = start_daemon_with(&service, "0", &[("FFF_MCP_TEST_HOLD_SEARCH", &hold)]);
    let session = ProgramSession::open(&ProgramConfig {
        response_timeout: Duration::from_millis(500),
        ..program_config(&project, &service)
    })
    .unwrap();
    assert!(session.wait_ready(EVENTUAL).unwrap());
    std::fs::write(&hold, "").unwrap();
    let request = SearchRequest {
        query: "inside".into(),
        limit: 10,
        ..Default::default()
    };
    let error = session.search_files(request.clone()).unwrap_err();
    assert!(error.lost_service(), "{error}");
    std::fs::remove_file(&hold).unwrap();
    // The daemon this test started stops answering while the session reconnects.
    let pid = Pid::from_raw(daemon.id() as i32).unwrap();
    kill_process(pid, Signal::STOP).unwrap();
    let started = Instant::now();
    let reconnect = session.progress();
    let elapsed = started.elapsed();
    kill_process(pid, Signal::CONT).unwrap();
    let error = reconnect.unwrap_err();
    assert!(error.lost_service(), "{error}");
    assert!(
        elapsed < Duration::from_secs(10),
        "reconnecting took {elapsed:?}"
    );
    assert!(session.wait_ready(EVENTUAL).unwrap());
    assert!(!session.search_files(request).unwrap().items.is_empty());
}

// A namespace whose socket path exceeds the platform limit listens in the per-user runtime
// directory, under the name a client derives from the same namespace.
#[cfg(unix)]
#[test]
fn a_long_namespace_listens_on_a_short_socket_its_clients_reach() {
    use fff_service::namespace::Namespace;
    use fff_service::protocol::Endpoint;
    use fff_service::socket::SocketPlace;

    let segment = "n".repeat(60);
    let tmp = Sandbox::with_service(&format!("{segment}/{segment}/service"));
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    // A short private runtime root of the test's own, in place of the shared one.
    let runtime = tempfile::Builder::new()
        .prefix("fr")
        .tempdir_in("/tmp")
        .unwrap();
    let root = runtime.path().canonicalize().unwrap().join("rt");
    let _daemon = start_daemon_with(&service, "0", &[("FFF_MCP_TEST_RUNTIME_DIR", &root)]);
    let session = open_program(&project, &service, program_options());
    let request = SearchRequest {
        query: "inside".into(),
        limit: 10,
        ..Default::default()
    };
    assert!(!session.search_files(request).unwrap().items.is_empty());

    let [endpoint] = endpoints(&service).try_into().unwrap();
    let files =
        fff_service::ServiceFiles::from_endpoint(endpoint.file_name().unwrap().to_str().unwrap())
            .unwrap();
    let published: Endpoint = serde_json::from_slice(&std::fs::read(&endpoint).unwrap()).unwrap();
    let namespace = Namespace::open(&service).unwrap();
    let place =
        SocketPlace::open_with_roots(&namespace, &files, std::slice::from_ref(&root)).unwrap();
    assert_eq!(Path::new(&published.name), place.path(&namespace));
    assert_eq!(place.dir(&namespace).path(), root);
    assert!(!namespace.path().join(files.socket()).exists());
    assert!(Path::new(&published.name).exists());
}

// What a test can observe of a running process: its working directory, executable,
// environment and whether descriptor `fd` is open in it.
#[cfg(unix)]
struct ProcessView {
    cwd: PathBuf,
    executable: PathBuf,
    environment: String,
    holds_fd: bool,
}

#[cfg(target_os = "linux")]
fn process_view(pid: u64, fd: i32) -> ProcessView {
    let proc = PathBuf::from(format!("/proc/{pid}"));
    ProcessView {
        cwd: std::fs::read_link(proc.join("cwd")).unwrap(),
        executable: std::fs::read_link(proc.join("exe")).unwrap(),
        environment: String::from_utf8_lossy(&std::fs::read(proc.join("environ")).unwrap())
            .replace('\0', "\n"),
        holds_fd: std::fs::read_link(proc.join("fd").join(fd.to_string()))
            .is_ok_and(|target| target.to_string_lossy().starts_with("pipe:")),
    }
}

#[cfg(target_os = "macos")]
fn process_view(pid: u64, fd: i32) -> ProcessView {
    let run = |program: &str, args: &[&str]| {
        let output = Command::new(program).args(args).output_isolated().unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let pid = pid.to_string();
    let named = |listing: String| {
        listing
            .lines()
            .find_map(|line| line.strip_prefix('n'))
            .map(PathBuf::from)
    };
    ProcessView {
        cwd: named(run("lsof", &["-a", "-p", &pid, "-d", "cwd", "-Fn"])).unwrap(),
        executable: PathBuf::from(run("ps", &["-o", "comm=", "-p", &pid]).trim()),
        environment: run("ps", &["-E", "-ww", "-o", "command=", "-p", &pid]),
        holds_fd: run("lsof", &["-a", "-p", &pid, "-d", &fd.to_string(), "-Ft"])
            .lines()
            .any(|line| line == "tPIPE"),
    }
}

// A service started by a client inherits neither descriptors the client's parent left open
// nor its working directory or environment, and one whose executable lives in the running
// AppImage runs from a private copy in the namespace.
#[cfg(unix)]
#[test]
fn a_started_service_holds_nothing_of_the_program_that_started_it() {
    use std::os::fd::AsRawFd;

    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    // An application's mount, holding the client executable.
    let appdir = tmp.path().join("mount");
    std::fs::create_dir(&appdir).unwrap();
    let binary = appdir.join("fff-mcp");
    std::fs::copy(BIN, &binary).unwrap();
    // A pipe without close-on-exec, as an application runtime keeps to hold its mount, at a
    // free descriptor number high enough that the service has no reason to use it itself.
    let (keep_read, _keep_write) = rustix::pipe::pipe().unwrap();
    // SAFETY: F_DUPFD returns a new descriptor this test owns, without close-on-exec.
    let inherited = unsafe { libc::fcntl(keep_read.as_raw_fd(), libc::F_DUPFD, 500) };
    assert!(inherited >= 500, "{}", std::io::Error::last_os_error());
    let _inherited =
        unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(inherited) };
    // A copy of another build that nothing runs or starts, left from an earlier version.
    let namespace = fff_service::namespace::Namespace::open(&service).unwrap();
    let stale = namespace.path().join("fff-mcp-0123456789abcdef");
    std::fs::write(&stale, "").unwrap();
    let start = || {
        let child = client_command(binary.to_str().unwrap(), &project, &service, "1", &[])
            .env("APPDIR", &appdir)
            .env("NOT_FOR_THE_SERVICE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn_keeping(&[inherited])
            .unwrap();
        Client::attach(child)
    };
    let mut client = start();
    assert!(
        process_view(client.child.id().into(), inherited).holds_fd,
        "the client was not given descriptor {inherited}"
    );
    assert!(!stale.exists(), "the stale copy was kept");
    for lock in ["startup", "owner"] {
        let created = namespace
            .path()
            .join(format!("{lock}-0123456789abcdef.lock"));
        assert!(!created.exists(), "a lock was created for the stale build");
    }
    let [pid] = service_pids(&service).try_into().unwrap();
    let view = process_view(pid, inherited);
    assert!(!view.holds_fd, "the service holds descriptor {inherited}");
    assert_eq!(view.cwd, Path::new("/"));
    let namespace = service.canonicalize().unwrap();
    assert!(
        view.executable.starts_with(&namespace),
        "the service runs from {}",
        view.executable.display()
    );
    assert!(
        !view.environment.contains("NOT_FOR_THE_SERVICE"),
        "{}",
        view.environment
    );
    assert!(!view.environment.contains("APPDIR"), "{}", view.environment);
    let result = client.request(
        "tools/call",
        json!({"name": "find_files", "arguments": {"query": "inside"}}),
    );
    assert!(result.to_string().contains("inside.rs"), "{result}");

    // The next service of the build starts from the same copy, which is not written again.
    use std::os::unix::fs::MetadataExt;
    let copy = std::fs::metadata(&view.executable).unwrap();
    drop(client);
    eventually("the first service never exited", || {
        service_pids(&service).is_empty()
    });
    let _client = start();
    let [pid] = service_pids(&service).try_into().unwrap();
    let again = process_view(pid, inherited);
    assert_eq!(again.executable, view.executable);
    let reused = std::fs::metadata(&again.executable).unwrap();
    assert_eq!((reused.dev(), reused.ino()), (copy.dev(), copy.ino()));
}

// Reported changes reach an index without a watcher before the reply, and a path outside the
// session's directory refuses the request.
#[test]
fn program_reported_changes_apply_before_the_reply() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = fixture(tmp.path());
    let snapshot = IndexOptions {
        watch: false,
        ..program_options()
    };
    let session = open_program(&sub, &service, snapshot);
    let names = |session: &ProgramSession| {
        let results = session.search_files(search("", 0, 100)).unwrap();
        results
            .items
            .iter()
            .map(|item| item.relative_path.clone())
            .collect::<Vec<_>>()
    };
    std::fs::write(sub.join("src/reported.rs"), "fn Reported() {}\n").unwrap();
    session.changed(vec!["src/reported.rs".into()]).unwrap();
    assert!(names(&session).contains(&"src/reported.rs".to_owned()));
    std::fs::remove_file(sub.join("src/reported.rs")).unwrap();
    session.changed(vec![sub.join("src/reported.rs")]).unwrap();
    assert!(!names(&session).contains(&"src/reported.rs".to_owned()));

    // A path through a missing component and back is inside, lexically.
    std::fs::write(sub.join("src/back.rs"), "fn Back() {}\n").unwrap();
    session.changed(vec!["nope/../src/back.rs".into()]).unwrap();
    assert!(names(&session).contains(&"src/back.rs".to_owned()));

    // Where the filesystem ignores case, a report spelled otherwise updates the indexed
    // entry rather than adding a second one, also once the file is gone.
    if sub.join("SRC").exists() {
        session.changed(vec!["SRC/INSIDE.RS".into()]).unwrap();
        let listed = names(&session);
        let inside: Vec<_> = listed
            .iter()
            .filter(|name| name.eq_ignore_ascii_case("src/inside.rs"))
            .collect();
        assert_eq!(inside, ["src/inside.rs"], "{listed:?}");
        std::fs::remove_file(sub.join("src/back.rs")).unwrap();
        session.changed(vec!["Src/Back.RS".into()]).unwrap();
        assert!(!names(&session).contains(&"src/back.rs".to_owned()));
    }

    // The index does not follow symlinks, as its scan does not: a reported symlink stays out,
    // and a file that became one leaves.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("inside.rs", sub.join("src/link.rs")).unwrap();
        session.changed(vec!["src/link.rs".into()]).unwrap();
        assert!(!names(&session).contains(&"src/link.rs".to_owned()));
        std::fs::write(sub.join("src/turned.rs"), "fn Turned() {}\n").unwrap();
        session.changed(vec!["src/turned.rs".into()]).unwrap();
        assert!(names(&session).contains(&"src/turned.rs".to_owned()));
        std::fs::remove_file(sub.join("src/turned.rs")).unwrap();
        std::os::unix::fs::symlink("inside.rs", sub.join("src/turned.rs")).unwrap();
        session.changed(vec!["src/turned.rs".into()]).unwrap();
        assert!(!names(&session).contains(&"src/turned.rs".to_owned()));
    }

    let outside = project.join("submarine/outside.rs");
    let error = session.changed(vec![outside]).unwrap_err();
    assert!(
        matches!(error, ClientError::Program(ProgramError::OutsideScope(_))),
        "{error}"
    );
    for escaping in ["../src/inside.rs", "src/../../x.rs"] {
        let error = session.changed(vec![escaping.into()]).unwrap_err();
        assert!(
            matches!(error, ClientError::Program(ProgramError::OutsideScope(_))),
            "{escaping}: {error}"
        );
    }
    // A symlink inside the directory that leads out of it does not carry a report out.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(project.join("submarine"), sub.join("door")).unwrap();
        let error = session.changed(vec!["door/outside.rs".into()]).unwrap_err();
        assert!(
            matches!(error, ClientError::Program(ProgramError::OutsideScope(_))),
            "{error}"
        );
    }
}

// A subscription whose deliveries arrive on a channel, once it is ready.
fn subscribe(scope: &Path, service: &Path, request: WatchRequest) -> (Subscription, Events) {
    subscribe_with(program_config(scope, service), request)
}

fn subscribe_with(config: ProgramConfig, request: WatchRequest) -> (Subscription, Events) {
    let (sender, deliveries) = mpsc::channel();
    let subscription = Subscription::start(&config, request, move |delivery| {
        let _ = sender.send(delivery);
    })
    .unwrap();
    let events = Events(deliveries);
    match events.next("the subscription to be ready") {
        Delivery::Ready => {}
        other => panic!("subscription not ready: {other:?}"),
    }
    (subscription, events)
}

struct Events(mpsc::Receiver<Delivery>);

impl Events {
    fn next(&self, what: &str) -> Delivery {
        self.0
            .recv_timeout(EVENTUAL)
            .unwrap_or_else(|error| panic!("{what}: {error}"))
    }

    // Returns the first event `wanted` accepts and every event before it.
    fn until(&self, what: &str, wanted: impl Fn(&WatchEvent) -> bool) -> Vec<WatchEvent> {
        let deadline = Instant::now() + EVENTUAL;
        let mut seen = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.0.recv_timeout(left) {
                Ok(Delivery::Events(events)) => {
                    for event in events {
                        let done = wanted(&event);
                        seen.push(event);
                        if done {
                            return seen;
                        }
                    }
                }
                Ok(other) => panic!("{what}: {other:?}; received {seen:?}"),
                Err(error) => panic!("{what}: {error}; received {seen:?}"),
            }
        }
    }

    // Events come in order, and a batch's file events go out before its refs events, so once
    // the second of two sentinels is heard, every change before the first was. Each is a new
    // file named by `what`, as one write may be heard twice.
    fn fence(&self, dir: &Path, what: &str) {
        for round in 1..=2 {
            let name = format!("{}-{round}.txt", what.replace(' ', "-"));
            std::fs::write(dir.join(&name), "sentinel\n").unwrap();
            self.until(what, |event| {
                event.path == name && event.kind != WatchEventKind::Refs
            });
        }
    }

    // Changes and stages `file` until a repository event for `root` shows the repository's
    // git dir is watched. A rescan, which the queue delivers in place of the changes after
    // it, proves nothing, so the queue is drained and the change made again, a few times.
    fn hears_staging(&self, work_tree: &Path, file: &str, root: &str) {
        for _ in 0..3 {
            change_and_stage(work_tree, file);
            let seen = self.until("a repository event or a rescan", |event| {
                is_rescan(event) || repository_event(root)(event)
            });
            if seen.last().is_some_and(|event| !is_rescan(event)) {
                return;
            }
            while self.0.try_recv().is_ok() {}
        }
        panic!("only rescans followed staging in {root}");
    }

    // The error that ended the subscription, after any events.
    fn failure(&self, what: &str) -> ClientError {
        loop {
            match self.next(what) {
                Delivery::Events(_) => {}
                Delivery::Failed(error) => return error,
                Delivery::Ready => panic!("{what}: ready again"),
            }
        }
    }
}

fn change(path: &str, kind: WatchEventKind) -> impl Fn(&WatchEvent) -> bool {
    move |event| event.path == path && event.kind == kind
}

fn is_rescan(event: &WatchEvent) -> bool {
    event.kind == WatchEventKind::Rescan
}

fn with_repository() -> WatchRequest {
    WatchRequest {
        repository: true,
        ..WatchRequest::default()
    }
}

fn watch_sessions(stats: &Value) -> usize {
    stats["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|session| session["kind"] == "watch")
        .count()
}

// A repository with `sub` inside and its log files ignored.
fn repository_fixture(root: &Path) -> (PathBuf, PathBuf) {
    let (project, sub) = fixture(root);
    git2::Repository::init(&project).unwrap();
    std::fs::write(project.join(".gitignore"), "*.log\n").unwrap();
    (project, sub)
}

// Changes `file` and stages it, so the index really changes: staging a file as it is
// already staged may leave the index as it was.
fn change_and_stage(work_tree: &Path, file: &str) {
    let path = work_tree.join(file);
    let mut body = std::fs::read(&path).unwrap();
    body.extend_from_slice(b"// staged\n");
    std::fs::write(&path, body).unwrap();
    stage(work_tree, file);
}

fn stage(work_tree: &Path, file: &str) {
    let repository = git2::Repository::open(work_tree).unwrap();
    let mut index = repository.index().unwrap();
    index.add_path(Path::new(file)).unwrap();
    index.write().unwrap();
}

fn commit_all(work_tree: &Path) {
    let repository = git2::Repository::open(work_tree).unwrap();
    let mut index = repository.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("fff", "fff@example.invalid").unwrap();
    repository
        .commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
        .unwrap();
}

fn repository_event(path: &str) -> impl Fn(&WatchEvent) -> bool {
    move |event| event.kind == WatchEventKind::Repository && event.path == path
}

fn refs_event(path: &str) -> impl Fn(&WatchEvent) -> bool {
    move |event| event.kind == WatchEventKind::Refs && event.path == path
}

fn canonical(path: &Path) -> String {
    path.canonicalize().unwrap().to_string_lossy().into_owned()
}

#[test]
fn watch_subscription_delivers_the_changes_of_its_directory_only() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = repository_fixture(tmp.path());
    let _daemon = start_daemon(&service, "0");
    let (_subscription, events) = subscribe(&sub, &service, WatchRequest::default());
    assert_eq!(watch_sessions(&status(&service)), 1);

    std::fs::write(sub.join("src/inside.rs"), "fn Changed() {}\n").unwrap();
    events.until(
        "a modified file",
        change("src/inside.rs", WatchEventKind::Modified),
    );

    // Ignored and outside files come before a file the subscription hears, so their events
    // would arrive with or before its.
    std::fs::write(sub.join("debug.log"), "ignored\n").unwrap();
    std::fs::write(project.join("submarine/outside.rs"), "fn Outside() {}\n").unwrap();
    std::fs::write(sub.join("src/new.rs"), "fn New() {}\n").unwrap();
    let seen = events.until(
        "a created file",
        change("src/new.rs", WatchEventKind::Created),
    );
    assert!(
        seen.iter()
            .all(|event| !event.path.contains("debug.log") && !event.path.contains("outside")),
        "{seen:?}"
    );

    // Directories named like a git dir's parts are a project's own.
    for logs in [sub.join("logs"), sub.join("src/logs")] {
        std::fs::create_dir(&logs).unwrap();
        std::fs::write(logs.join("today.txt"), "first\n").unwrap();
        let path = logs.strip_prefix(&sub).unwrap().join("today.txt");
        let path = path.to_string_lossy();
        events.until(
            "a file of a new logs directory",
            change(&path, WatchEventKind::Created),
        );
        std::fs::write(logs.join("today.txt"), "second\n").unwrap();
        events.until(
            "a later change there",
            change(&path, WatchEventKind::Modified),
        );
    }
    let session = open_program(&sub, &service, program_options());
    let indexed = session.search_files(search("today", 0, 10)).unwrap();
    assert_eq!(indexed.items.len(), 2, "{:?}", indexed.items);

    // A rescan a program requests reaches every subscription of the index.
    session.rescan().unwrap();
    events.until("a rescan of the index", is_rescan);
}

#[test]
fn watch_subscription_hears_repository_state_only_when_asked() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = repository_fixture(tmp.path());
    let _daemon = start_daemon(&service, "0");
    let (_with, with_state) = subscribe(&project, &service, with_repository());
    let (_without, without_state) = subscribe(&project, &service, WatchRequest::default());
    let (_below, below) = subscribe(&sub, &service, with_repository());

    stage(&project, "sub/src/inside.rs");
    let seen = with_state.until("a repository event", |event| {
        event.kind == WatchEventKind::Repository
    });
    assert_eq!(
        seen.last().unwrap().path,
        "",
        "the root is the directory itself"
    );
    // A subscription below the work tree hears it by its absolute path.
    below.until(
        "the containing repository",
        repository_event(&canonical(&project)),
    );

    std::fs::write(project.join("sentinel.rs"), "fn Sentinel() {}\n").unwrap();
    let seen = without_state.until(
        "the sentinel",
        change("sentinel.rs", WatchEventKind::Created),
    );
    assert!(
        seen.iter().all(
            |event| event.kind != WatchEventKind::Repository && !event.path.starts_with(".git")
        ),
        "{seen:?}"
    );
}

#[test]
fn watch_subscription_hears_refs_only_when_asked() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = repository_fixture(tmp.path());
    commit_all(&project);
    let repository = git2::Repository::open(&project).unwrap();
    let linked = tmp.path().join("linked");
    repository.worktree("linked", &linked, None).unwrap();
    let _daemon = start_daemon(&service, "0");
    let with_refs = || WatchRequest {
        refs: true,
        ..WatchRequest::default()
    };
    let (_refs, refs) = subscribe(&project, &service, with_refs());
    let (_state, state) = subscribe(&project, &service, with_repository());
    let (_below, below) = subscribe(&sub, &service, with_refs());
    let (_linked, linked_refs) = subscribe(&linked, &service, with_refs());

    let head = repository.head().unwrap().peel_to_commit().unwrap();
    repository.branch("feature", &head, false).unwrap();
    refs.until("a new branch", refs_event(""));
    below.until(
        "the containing repository's branch",
        refs_event(&canonical(&project)),
    );
    // A linked worktree's branches live in its main repository.
    linked_refs.until("a branch of the main repository", refs_event(""));
    refs.fence(&project, "after the branch");
    repository
        .tag_lightweight("v1", head.as_object(), false)
        .unwrap();
    refs.until("a new tag", refs_event(""));
    refs.fence(&project, "after the tag");
    repository.set_head("refs/heads/feature").unwrap();
    refs.until("a switched HEAD", refs_event(""));

    std::fs::write(project.join("sentinel.rs"), "fn Sentinel() {}\n").unwrap();
    let seen = state.until(
        "the sentinel",
        change("sentinel.rs", WatchEventKind::Created),
    );
    assert!(
        seen.iter().all(|event| event.kind != WatchEventKind::Refs),
        "{seen:?}"
    );
}

#[test]
fn nested_repositories_reach_their_own_and_enclosing_subscriptions() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (outer, inner) = repository_fixture(tmp.path());
    git2::Repository::init(&inner).unwrap();
    // A folder that is no repository, with one inside, and a directory that becomes one.
    let plain = tmp.path().join("plain");
    let app = plain.join("app");
    let late = plain.join("late");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&late).unwrap();
    std::fs::write(app.join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(late.join("lib.rs"), "fn lib() {}\n").unwrap();
    git2::Repository::init(&app).unwrap();
    let _daemon = start_daemon(&service, "0");
    let (_outer, outer_events) = subscribe(&outer, &service, with_repository());
    let (_inner, inner_events) = subscribe(&inner, &service, with_repository());
    let (_plain, plain_events) = subscribe(&plain, &service, with_repository());
    let (_app, app_events) = subscribe(&app, &service, with_repository());

    stage(&inner, "src/inside.rs");
    inner_events.until("the nested repository's own event", repository_event(""));
    outer_events.until(
        "the nested repository, seen from outside",
        repository_event("sub"),
    );

    stage(&outer, ".gitignore");
    outer_events.until("the outer repository's event", repository_event(""));
    std::fs::write(inner.join("sentinel.rs"), "fn Sentinel() {}\n").unwrap();
    let seen = inner_events.until(
        "the sentinel",
        change("sentinel.rs", WatchEventKind::Created),
    );
    // Its own repository may report again, by its own empty path; the outer one would come
    // by the outer work tree's absolute path.
    assert!(
        seen.iter()
            .all(|event| event.kind != WatchEventKind::Repository || event.path.is_empty()),
        "the nested subscription heard the outer repository: {seen:?}"
    );

    stage(&app, "main.rs");
    app_events.until("a repository inside a plain folder", repository_event(""));
    plain_events.until("the repository inside the folder", repository_event("app"));

    // A fresh repository's log appears with its first commit, after its git dir was
    // watched, and commits only touch the log.
    let fresh = plain.join("fresh");
    std::fs::create_dir(&fresh).unwrap();
    std::fs::write(fresh.join("lib.rs"), "fn fresh() {}\n").unwrap();
    let repository = git2::Repository::init(&fresh).unwrap();
    plain_events.until("the fresh repository", repository_event("fresh"));
    stage(&fresh, "lib.rs");
    plain_events.until("the fresh repository's index", repository_event("fresh"));
    let tree = repository
        .find_tree(repository.index().unwrap().write_tree().unwrap())
        .unwrap();
    let signature = git2::Signature::now("fff", "fff@example.invalid").unwrap();
    let first = repository
        .commit(Some("HEAD"), &signature, &signature, "first", &tree, &[])
        .unwrap();
    plain_events.until("the first commit", repository_event("fresh"));
    let parent = repository.find_commit(first).unwrap();
    repository
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "second",
            &tree,
            &[&parent],
        )
        .unwrap();
    plain_events.until("a commit seen through the log", repository_event("fresh"));

    // A repository initialised after the walk reached its directory.
    git2::Repository::init(&late).unwrap();
    stage(&late, "lib.rs");
    plain_events.until(
        "a repository created after the scan",
        repository_event("late"),
    );

    // A repository removed and created again, as a fresh clone is, is watched anew.
    std::fs::remove_dir_all(&app).unwrap();
    plain_events.until(
        "the removed repository",
        change("app/main.rs", WatchEventKind::Removed),
    );
    std::fs::create_dir(&app).unwrap();
    std::fs::write(app.join("main.rs"), "fn main() {}\n").unwrap();
    git2::Repository::init(&app).unwrap();
    plain_events.until(
        "the recreated repository",
        change("app/main.rs", WatchEventKind::Created),
    );
    stage(&app, "main.rs");
    plain_events.until("the recreated repository's state", repository_event("app"));

    // Also when it goes and comes back before the watcher looks, within one batch.
    std::fs::remove_dir_all(&app).unwrap();
    std::fs::create_dir(&app).unwrap();
    std::fs::write(app.join("main.rs"), "fn main() { again() }\n").unwrap();
    git2::Repository::init(&app).unwrap();
    // It is announced, or its first state change heard; a later change then shows that its
    // new git dir is watched.
    stage(&app, "main.rs");
    plain_events.until("the repository back at once", |event| {
        repository_event("app")(event) || is_rescan(event)
    });
    while plain_events.0.try_recv().is_ok() {}
    plain_events.hears_staging(&app, "main.rs", "app");
}

#[test]
fn linked_worktrees_and_submodules_report_their_git_state() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = repository_fixture(tmp.path());
    commit_all(&project);
    let repository = git2::Repository::open(&project).unwrap();
    // Linked worktrees inside and outside the main work tree, whose `.git` is a file.
    let inside = project.join("feature");
    let outside = tmp.path().join("elsewhere");
    repository.worktree("feature", &inside, None).unwrap();
    repository.worktree("elsewhere", &outside, None).unwrap();
    // A submodule whose git dir lies in the main repository's `.git`.
    let vendor = project.join("vendor");
    std::fs::create_dir(&vendor).unwrap();
    std::fs::write(vendor.join("lib.rs"), "fn vendored() {}\n").unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.workdir_path(&vendor).no_dotgit_dir(true);
    git2::Repository::init_opts(repository.path().join("modules/vendor"), &options).unwrap();
    assert!(vendor.join(".git").is_file());
    let _daemon = start_daemon(&service, "0");
    let (_main, main_events) = subscribe(&project, &service, with_repository());
    let (_outside, outside_events) = subscribe(&outside, &service, with_repository());
    let (_vendor, vendor_events) = subscribe(&vendor, &service, with_repository());

    change_and_stage(&outside, "sub/src/inside.rs");
    outside_events.until(
        "a worktree whose git dir is outside it",
        repository_event(""),
    );
    change_and_stage(&inside, "sub/src/inside.rs");
    main_events.until(
        "a worktree inside the main work tree",
        repository_event("feature"),
    );
    stage(&vendor, "lib.rs");
    vendor_events.until("the submodule's own event", repository_event(""));
    main_events.until(
        "the submodule, seen from its superproject",
        repository_event("vendor"),
    );

    // A worktree removed and added again is watched anew. Its `.gitignore` going or
    // coming may rescan the index, which stands for its files' events, and a quick return
    // may read as a modification.
    let feature = repository.find_worktree("feature").unwrap();
    let mut prune = git2::WorktreePruneOptions::new();
    prune.valid(true).working_tree(true);
    feature.prune(Some(&mut prune)).unwrap();
    main_events.until("the removed worktree", |event| {
        is_rescan(event) || change("feature/.gitignore", WatchEventKind::Removed)(event)
    });
    let branch = repository.find_reference("refs/heads/feature").unwrap();
    let mut add = git2::WorktreeAddOptions::new();
    add.reference(Some(&branch));
    repository.worktree("feature", &inside, Some(&add)).unwrap();
    main_events.until("the added worktree", |event| {
        is_rescan(event)
            || event.path == "feature/.gitignore" && event.kind != WatchEventKind::Removed
    });
    main_events.hears_staging(&inside, "sub/src/inside.rs", "feature");
}

#[test]
fn slow_watch_consumer_loses_its_own_changes_only() {
    const ROUNDS: usize = 10;
    const PER_ROUND: usize = 1500;
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (_, sub) = fixture(tmp.path());
    let many = sub.join("many");
    std::fs::create_dir(&many).unwrap();
    for n in 0..ROUNDS * PER_ROUND {
        std::fs::write(many.join(format!("f{n}.rs")), "").unwrap();
    }
    let _daemon = start_daemon(&service, "0");
    let (blocked, gate) = mpsc::channel::<()>();
    let (sender, deliveries) = mpsc::channel();
    let _slow = Subscription::start(&program_config(&sub, &service), WatchRequest::default(), {
        let mut gate = Some(gate);
        move |delivery| {
            // The first batch holds the subscription's thread until the test lets go.
            if matches!(delivery, Delivery::Events(_))
                && let Some(gate) = gate.take()
            {
                let _ = gate.recv();
            }
            let _ = sender.send(delivery);
        }
    })
    .unwrap();
    let slow = Events(deliveries);
    assert!(matches!(slow.next("ready"), Delivery::Ready));
    let (_reader, reader) = subscribe(&sub, &service, WatchRequest::default());
    std::fs::write(sub.join("first.rs"), "fn First() {}\n").unwrap();
    reader.until(
        "the first change",
        change("first.rs", WatchEventKind::Created),
    );

    // Rounds of modifications, each well below the watcher's own batch limit, while one
    // subscription reads nothing and another reads everything.
    let other = open_program(&sub, &service, program_options());
    for round in 0..ROUNDS {
        let body = "x".repeat(round + 1);
        for n in round * PER_ROUND..(round + 1) * PER_ROUND {
            std::fs::write(many.join(format!("f{n}.rs")), &body).unwrap();
        }
        let last = format!("f{}", (round + 1) * PER_ROUND - 1);
        eventually("the index holds the round", || {
            other
                .search_files(search(&last, 0, 10))
                .unwrap()
                .items
                .iter()
                .any(|item| {
                    item.relative_path.ends_with(&format!("{last}.rs"))
                        && item.size == body.len() as u64
                })
        });
    }

    drop(blocked);
    slow.until("a rescan for the changes it could not hold", is_rescan);
    std::fs::write(sub.join("after.rs"), "fn After() {}\n").unwrap();
    slow.until(
        "a change after the rescan",
        change("after.rs", WatchEventKind::Created),
    );
    // The reader kept up, so the index itself rescanned nothing.
    let seen = reader.until(
        "the reader's change",
        change("after.rs", WatchEventKind::Created),
    );
    assert!(!seen.iter().any(is_rescan), "the watcher rescanned");
}

#[test]
fn watch_subscription_follows_its_session_to_a_broader_index() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = fixture(tmp.path());
    let _daemon = start_daemon(&service, "0");
    let (_subscription, events) = subscribe(&sub, &service, WatchRequest::default());
    let _parent = open_program(&project, &service, program_options());
    wait_status(
        &service,
        "the subscription moves to the parent index",
        index_count(1),
    );
    events.until("a rescan for the move", is_rescan);
    std::fs::write(sub.join("src/inside.rs"), "fn Moved() {}\n").unwrap();
    events.until(
        "a change relative to the subscription's directory",
        change("src/inside.rs", WatchEventKind::Modified),
    );
}

#[test]
fn watch_subscription_resubscribes_to_a_new_service_with_a_rescan() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let mut daemon = start_daemon(&service, "0");
    let (_subscription, events) = subscribe(&project, &service, WatchRequest::default());
    let old = daemon.id() as u64;
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    // The subscription starts the service itself, as opening a session does.
    events.until("a rescan once subscribed again", is_rescan);
    let stats = status(&service);
    assert_ne!(stats["pid"].as_u64(), Some(old), "{stats}");
    assert_eq!(watch_sessions(&stats), 1, "{stats}");
}

#[test]
fn watch_subscription_ends_when_its_directory_cannot_be_served() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, sub) = fixture(tmp.path());
    let mut daemon = start_daemon(&service, "0");
    // A removed directory ends the subscription the service holds.
    let (_removed, removed) = subscribe(&sub, &service, WatchRequest::default());
    std::fs::remove_dir_all(&sub).unwrap();
    let error = removed.failure("the end of a removed directory's subscription");
    assert!(
        matches!(error, ClientError::Service(ServiceError::Scope { .. })),
        "{error}"
    );

    // A reconnect the new service refuses ends it too, instead of retrying.
    let gone = project.join("submarine");
    let (_refused, refused) = subscribe(&gone, &service, WatchRequest::default());
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    std::fs::remove_dir_all(&gone).unwrap();
    let error = refused.failure("the end of a refused reconnect");
    assert!(
        matches!(error, ClientError::Service(ServiceError::Scope { .. })),
        "{error}"
    );
}

#[test]
fn ended_watch_subscription_releases_its_session() {
    let tmp = Sandbox::new();
    let service = tmp.service();
    let (project, _) = fixture(tmp.path());
    let _daemon = start_daemon(&service, "0");
    let (subscription, events) = subscribe(&project, &service, WatchRequest::default());
    assert_eq!(watch_sessions(&status(&service)), 1);
    drop(subscription);
    // Its delivery ended with it, so nothing can arrive any more.
    while events.0.try_recv().is_ok() {}
    assert!(matches!(
        events.0.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    ));
    wait_status(
        &service,
        "the session ends with its subscription",
        |stats| stats["sessions"].as_array().is_some_and(Vec::is_empty),
    );

    let snapshot = IndexOptions {
        watch: false,
        ..program_options()
    };
    let (sender, deliveries) = mpsc::channel();
    let _refused = Subscription::start(
        &ProgramConfig {
            options: snapshot,
            ..program_config(&project, &service)
        },
        WatchRequest::default(),
        move |delivery| {
            let _ = sender.send(delivery);
        },
    )
    .unwrap();
    let error = Events(deliveries).failure("a subscription to an index without a watcher");
    assert!(
        matches!(
            error,
            ClientError::Service(ServiceError::InvalidRequest { .. })
        ),
        "{error}"
    );
}
