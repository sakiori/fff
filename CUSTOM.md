# Downstream maintenance

This document describes how downstream changes to fff are maintained and how
they differ from upstream. Update it whenever a feature changes downstream
behavior, configuration, compatibility, or maintenance requirements.

## Branches

- `main` is the fork's branch and carries the downstream changes on top of
  upstream. It changes only through merge requests and is never force-pushed.
- Feature branches start from `main` and contain one logical change each.

Completed features are squash-merged into `main` through a merge request,
producing one commit per feature. The project merges by fast-forward; if `main`
moved on since the feature branch started, GitLab's rebase brings the branch up
to date before the merge. The commit describes the resulting behavior and why it
changed. Subsequent work starts from the updated `main`; a previously
squash-merged feature branch is not reused.

## Upstream updates

The upstream fff repository is not a remote of the fork. `.git-intake.toml`
describes it by URL and branch, and upstream updates are made with git-intake
in follow mode, which merges upstream `main` into `main`.

`git intake status` shows the last synced upstream commit and how far `main` is
behind, and `git intake log` lists the upstream commits still pending.
`git intake start fff=<rev>` merges upstream up to the chosen revision, a
commit of upstream `main` or an upstream tag (see _Choosing a target_ in the
[git-intake](https://gitlab.com/sakiori-oss/git-intake) README), in a
separate update worktree; without `=<rev>` it takes the tip it fetches. Resolve
conflicts against the intended behavior of both upstream and downstream
changes, stage the resolutions and run `git intake continue`.
`git intake verify` runs the install command and the checks listed in the
configuration; the combined result must pass them before the update is ready.
`git intake finish` opens the merge request into `main`.

`mise.toml` pins the Zig version that builds zlob. It is downstream-only and
must match the Zig version upstream pins in its workflow setup-zig steps and in
`flake.nix`; when an update changes upstream's pin, change `mise.toml` in the
same update.

Merge the update's merge request by fast-forward with _Squash commits_
unticked, so that the verified merge commit itself lands on `main`, keeping
upstream ancestry and the sync record that `status` and the next update read.
If `main` moved on and GitLab asks for a rebase, do not rebase: a rebase or a
squash replaces the verified commit, and `finish` refuses it. Run
`git intake abort`, update the local `main` and start the update again. Once
the merge request is merged, run `git intake finish` again to complete the
update.

## Feature delivery

Implement and verify each feature on its feature branch. Keep product
requirements and consequential design decisions alongside the affected code,
and update this document with the downstream behavior that is actually delivered.

Before integration, verify the feature against the current `main`, including
relevant upstream compatibility checks. Squash integration records the feature
as a single change; it does not replace functional verification.

## Downstream behavior

### Shared MCP service

Each MCP stdio process connects to a shared service for the current OS user.
The service owns file indexes, content caches and filesystem watchers. The stdio
processes retain only transport and parent-lifetime handling.

Compatible clients share an index for the same directory or covered descendants.
Canonical paths resolve aliases. A repository index can serve its subdirectories;
a broader non-repository scope can replace narrower coverage without changing
existing clients' search scopes. A client moves to broader coverage once that
index has finished scanning. This also happens when an existing index gains
coverage later, because its watcher indexed a new directory or a rescan
included one: the service consolidates on that change instead of waiting for
another client, and releases the index that is no longer used. When a client's
directory is removed, its tool calls report the scope as unavailable until the
directory is restored. Separate repository contexts and conflicting
indexing options retain separate indexes in the service. Built-in directory
exclusions apply at any depth with both walker backends, so a parent index
serves a descendant with the same files a standalone index would contain.
Clients using `--no-watch` keep their original snapshot when broader coverage
arrives.

Watched indexes track empty directories created or restored after the initial
scan, including nested directories. The core notifies subscribers whenever the
set of indexed directories changes. Compatible descendant clients can reuse
that coverage without waiting for a file to be created. Directory exclusions
still apply through the original root's traversal policy. An update rejected
because incremental capacity is exhausted requests a rescan; a request delayed
by rate limiting stays pending and runs when the cooldown ends, and any
admitted rescan satisfies it. A file that disappears before it is indexed does
not trigger a rescan. When the watcher walks a directory that is already
indexed, which happens after an event for the directory itself, subscribers
receive `Created` for files that were not indexed, `Modified` for indexed files
whose size or modification time changed, and nothing for unchanged files;
upstream reported every file of the walk as created. Files of a directory
created while a scan runs are indexed by that scan, so the walk that follows
does not report them again, as with the events the scan holds.

Changes made while an index scans are not lost. Every scan subscribes to
filesystem events before it walks the tree, holds the events that arrive during
the walk and applies them in order once the scan result is in place; a watched
index reports its watcher ready only after that. When more events are held than
one update batch accepts, they are discarded in favour of one recovery rescan
that rate limiting defers but never drops. At most one scan runs per index;
requests arriving meanwhile coalesce into one follow-up scan. Content edited
after the content index read a file stays searchable. These scan and watcher
changes are in the core crate and apply to every fff consumer, not only the
MCP service.

Concurrent searches from different clients share one bounded search pool
without waiting for each other: a search that has finished its own work returns
even while another client's search is still running. Results, offsets and
pagination are the same as for a search running alone.

Scope boundaries apply before matching, ranking, counts and pagination in file,
directory, mixed, glob and content searches, including fallback searches.
Returned paths and query path constraints are relative to the client's
directory. Each client has independent cursors; cursors for different searches
coexist, and a changed index binding invalidates the old ones.

### Building and running

Run `make build-mcp` to build the downstream executable from a checkout in
`target/release`. That build uses the default walker and needs no Zig; builds
with zlob, as `make test-rust` uses, need the Zig version pinned in `mise.toml`;
with mise, run `mise trust` and `mise install` in the checkout once. Configure
each MCP client to launch that executable, passing its desired search directory
as the positional argument. The upstream install scripts and package manager
releases do not contain these downstream changes.

The first client starts the service automatically. A client completes MCP
initialization as soon as its session is registered; the index keeps scanning in
the service. Tool calls wait up to 30 seconds for the initial scan and, for
watched sessions, the filesystem watcher, and otherwise ask the caller to retry.
If the watcher cannot start, the index keeps serving searches without live
updates and service status reports the watcher state. A frecency database that
cannot be opened produces a warning instead of a failed session. The existing
MCP tools and stdio configuration remain available. `--no-warmup`,
`--content-indexing`, `--no-content-indexing`, `--no-watch`, `--follow-symlinks`,
`--max-cached-files` and `--frecency-db` participate in index compatibility.

- `--service-status` lists every service in the namespace with its active scopes,
  index bindings and process, and marks the service of the current build.
- `--service-stop` stops every service in the namespace that has no connected
  clients and fails if any service refuses.
- `--service-dir` or `FFF_MCP_SERVICE_DIR` selects a separate service namespace.
  The default namespace is `fff/service` in the user state directory:
  `XDG_STATE_HOME` or `~/.local/state` on Unix, `LOCALAPPDATA` on Windows.
- `--idle-timeout-secs` sets the service idle timeout when it starts. The default
  is one hour; zero disables idle exit. Only search sessions keep a service busy;
  status and stop requests do not. A client whose parent process cannot be
  observed exits after the same timeout without MCP traffic.

Namespaces and state files must be private to the current OS user. On Unix, the
namespace directory must be owned by the user with no group or other access, and
every parent directory must be owned by the user or root and not writable by
others unless it is a root-owned sticky directory. Symlinks are refused, state
files are opened relative to the verified directory, and both ends of a
connection check the peer's user. Choose a short namespace directory: socket
paths have a platform-defined length limit. On Windows, protected ACLs also
allow SYSTEM and Administrators; directory and file ACLs are validated
independently, and the pipe receives an explicit ACL.

Local connections require the namespace token and a matching protocol version.
Each executable build runs its own service in the namespace, so a new build can
start while clients of an older build keep running; the older service exits once
it is idle. A client retries through the startup lock when a service shuts down
before accepting its session. A crashed service fails current clients visibly;
subsequent launches recover stale discovery state. A service that clients can no
longer reach, because its namespace directory, discovery file or socket was
removed or taken over by a replacement, finishes its current sessions and then
exits even when idle exit is disabled. An unreadable namespace or a hung check
leaves a service running. Whatever the reason for exiting, a service removes its
discovery file and socket only while they still lead to it, so a replacement's
files stay in place. Service startup errors are written to a log in the
namespace and reported by the launching client.

The last session using an index releases it; dropping an index cancels its scan
and watcher. An idle service holds no file index. Disconnecting or losing the
parent of one stdio process does not stop other clients. A service contains
multiple indexes when coverage or policies differ; sharing does not require
scanning the entire filesystem.

### Verification

`make test-mcp` runs the MCP tests and core search regression suite.
`make lint-mcp` checks both crates and their test targets without relaxing lints.
The shared-service tests use isolated namespaces and cover concurrent startup,
scoped tools and fallbacks, watcher updates, consolidation when an index gains
coverage through a rescan or a directory its watcher indexes, removed scopes,
concurrent searches over one index, cursor isolation, disconnects, parent exit,
side-by-side builds, handshake rejection, namespace permissions, stale discovery
recovery, unreachable services, shutdown races and idle cleanup. Each test stops
the services of its namespace when it ends, also when it fails. In debug
builds, watchers leave new directories unindexed while the file named by
`FFF_TEST_HOLD_DIR_INDEXING` exists, so the watcher consolidation test orders
its steps without timing.

In test and debug builds, watchers use notify's polling backend instead of the
OS event service when `FFF_TEST_POLL_WATCHER_MS` sets an interval in
milliseconds, or for roots a test registered with `watch::poll_watcher_for`;
release builds always use the native backend. The polled events take the same
debouncer and handler, so tests check how fff handles watcher events without
depending on how promptly the OS delivers them. The shared-service tests and the
downstream core tests of scans and overflow recovery always poll, and
`git intake verify` sets the variable for `make test-rust` and `make test-mcp`,
so upstream's watcher tests poll there too. Verify skips five tests by exact
name. The three `fs_delete_handler_test` tests
(`raw_notify_nonrecursive_detects_deletion`,
`raw_notify_multi_nonrecursive_detects_deletion` and
`debounced_nonrecursive_detects_deletion`) build notify's native watcher
themselves and test OS and notify event delivery, not fff code, so polling
cannot reach them. The two `watch_subscription_test` tests
`renaming_out_of_a_watched_subtree_still_notifies` and
`renaming_into_a_watched_subtree_still_notifies` assert that a rename arrives
as one paired event, which native backends deliver; a polling walk is not an
atomic snapshot, so it can report the two sides in separate batches; how fff
delivers a rename to subscribers of either side is checked with injected events
in the watch registry's unit tests. Native
event delivery is covered by `make test-rust` and `make test-mcp` run without
the variable and without skips, these tests included, and by upstream's CI. `make test-rust`
and `make test-mcp` pass `TEST_ARGS` to the test harness. In debug builds,
`SharedFilePicker::end_rescan_cooldown` lets a test end the rescan cooldown
instead of waiting for it.
