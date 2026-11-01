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
processes retain only transport and parent-lifetime handling. The service
client, its protocol and the namespace checks live in the `fff-service` library
crate, so programs other than `fff-mcp` can reach the service the same way.

Compatible clients share an index for the same directory or covered descendants.
Canonical paths resolve aliases. A repository index can serve its subdirectories;
a broader non-repository scope can replace narrower coverage without changing
existing clients' search scopes. A client moves to broader coverage once that
index has finished scanning. This also happens when an existing index gains
coverage later, because its watcher indexed a new directory or a rescan
included one: the service consolidates on that change instead of waiting for
another client, and releases the index that is no longer used. When a client's
directory is removed, its tool calls report the scope as unavailable until the
directory is restored. Built-in directory exclusions apply at any depth with
both walker backends, so a parent index serves a descendant with the same files
a standalone index would contain.

An index is reused for whatever a request needs rather than for equal options.
Options that decide which files it holds must match: separate repository
contexts and different `--follow-symlinks` settings keep separate indexes.
Content indexing, watching, warmup and the content cache budget
(`--max-cached-files`) are capabilities an index may offer beyond the request: a
client without content indexing uses a content index, a `--no-watch` client
uses a watched index, and a client with a smaller cache budget uses an index
with a larger one. A budget left to the core caches between the core's smallest
and largest size-based budgets, so it serves a request up to the smallest one
and is served by an explicit budget of at least the largest. A client that
needs a capability the covering index lacks gets an index of its own, which
also offers every capability of the indexes at the same root that it can
replace, those no `--no-watch` client keeps; once it is ready, compatible clients move to it as they move to
broader coverage, and the index they leave is released. The frecency database
belongs to the index and is chosen by the client that creates it; a client that
names one only shares indexes that use the same one, and both clients send it
in one spelling, absolute with its directory resolved. Permission
to index the home directory or the filesystem root is checked for each client
and does not separate indexes. Clients using `--no-watch` keep their original
binding when broader coverage arrives.

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
whose size or modification time changed, also to an older time, and nothing for
unchanged files; upstream reported every file of the walk as created and kept
the newest modification time it had seen. A walk leaves a file as it is when its
inode, size and modification time match the index, so the index holds what it
read from that very file; a file in a replaced directory is usually a new inode
and is read again. Filesystems such as ext4 and xfs reuse freed inode numbers
quickly, so a file regenerated within the same second with the same size can
keep the content the index read before until it changes again. Upstream read
every file of the walk again. On platforms without inode numbers every file of
the walk is read again. Files of a directory
created while a scan runs are indexed by that scan, so the walk that follows
does not report them again, as with the events the scan holds.

An indexed directory, the index's root included, that is reported as removed,
created or renamed and is a directory when the watcher applies the event may
have been replaced, as by `rm -r d && mkdir d` or `mv d d.bak && mv new d`. The
watcher walks it and replaces the entries below it in the same update as the
rest of the batch: a file present before and after the replacement stays
searchable throughout, files the new directory lacks leave the index, and a file
moved out of it in the same batch is reported as renamed. A replaced directory
that the ignore rules now exclude leaves the index, as it would after a scan.
A directory the walk cannot read is left out as a full scan leaves it out;
granting access to one again is an attribute change on it, which walks it again.
One removed while the walk runs gets no files, and its own entry leaves with its
removal event. A file replaced by a
directory, and a directory replaced by a file, leave the index with their
entries. Upstream emptied the subtree on a removal and indexed it again
afterwards, so searches in between found none of its files, and on a rename only
added the new directory's files, so the old ones stayed.

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

File, directory, mixed and glob searches take an `exclude_frecency` option in
`FuzzySearchOptions`, and grep and multi-grep take one in `GrepSearchOptions`.
With it, a search ranks as an index without a frecency database would, although
the index keeps the frecency scores other searches use: a query without fuzzy
text then lists its entries in path order, so pages stay stable, and grep
searches files by modification time alone. This is a core option, available to
every fff consumer.

Concurrent searches from different clients share one bounded search pool
without waiting for each other: a search that has finished its own work returns
even while another client's search is still running. Results, offsets and
pagination are the same as for a search running alone.

Scope boundaries apply before matching, ranking, counts and pagination in file,
directory, mixed, glob and content searches, including fallback searches.
Returned paths and query path constraints are relative to the client's
directory. Each client has independent cursors; cursors for different searches
coexist, and a changed index binding invalidates the old ones.

### Program sessions

Programs reach the shared service through the `fff-service` library. A program
session is opened like an MCP session, for a directory and index options, and
follows the same index reuse rules; service status marks each session as `mcp`
or `program`. After the service accepts it, the connection carries one request
at a time as a big-endian 32-bit length followed by a MessagePack body, and the
service answers each with one frame. Requests are file, directory and mixed
fuzzy search, glob, grep and multi-grep, a readiness wait with a timeout of at
most an hour, whether the index is scanning, its scan progress, a rescan
coalesced with any running one, the git work tree containing the directory,
reported changes, and close. Reported changes name files the program changed,
absolute or relative to its directory; the service applies them to the
session's index through the core's watcher path, as `apply_reported_changes`
of `SharedFilePicker`, without a rescan. What a report guarantees:

- Each reported file that exists is in the index, added or updated, with its
  content indexed, when the reply arrives, unless the index's ignore rules
  exclude it; each that no longer exists is gone from it, with any indexed
  files below it. While the index scans, the changes wait with the scan's held
  events and are applied when it commits, after the reply.
- Report files, not directories. A reported new directory is walked only by a
  watched index, by its watcher and after the reply; an index without a
  watcher does not index it.
- A reported `.gitignore` or `.ignore` file requests a rescan, as the watcher's
  event does, and the new rules apply once it has run.
- Paths are normalised lexically, so `a/../b` names `b`, and then spelled as on
  disk: every component in its on-disk case, with a symlink kept as the link.
  Where the filesystem ignores case, a deleted path matches the indexed entry
  regardless of ASCII case only: a deleted file with a non-ASCII name, reported
  in another case, stays indexed until the watcher's own event or the next
  rescan removes it. Matching it as APFS does would need Unicode case folding
  and normalisation for a rare case the watcher already covers. A path that
  resolves outside the session's directory, also through a symlink, refuses the
  whole request (`OutsideScope`).
- An index that does not follow symlinks, which is the default, holds none, as
  its scan skips them: a reported symlink is not added, and a reported path that
  became a symlink is removed.

The readiness wait and the scanning state count a scan until its
post-scan phase ends and a requested rescan until it has run. Scan progress
counts the files of the session's directory once the index is ready, and the
files the shared index has scanned so far before. The scanning state and scan
progress never wait for searches or index updates: while those hold the index,
progress repeats the session's last count. Results are the core's:
paths relative to the session's directory, scores, totals and, for grep, line
numbers, line content, byte match ranges, the number of files searched, the
regex fallback error and the next cursor, which names the index that issued
it beside the core's file offset. Every search, grep included, can exclude
frecency. A grep with a cursor of another index than the session's current
one fails, also after the session grepped on the new index, and the program
searches again without it. Searches never wait for the initial scan, unlike
tool calls: they answer from what the index holds so far, nothing before the
walk is committed, as an in-process instance does. The
session ends when the program closes it or its connection ends, also when the
program exits without closing it or while the service works on its request,
which stops a grep in progress.

`fff_service::session::ProgramSession` is the blocking client: it connects to
the service of its own build in the configured namespace, or in the one
`FFF_MCP_SERVICE_DIR` names, or in the default namespace, and starts the
service from the `fff-mcp` executable it is given when none runs. Relative
paths of the directory, executable, log file and frecency database are
resolved against the program's working directory. Every call is bounded:
opening the session takes at most its connect timeout, 90 seconds by default,
and a call at most its response timeout, 30 seconds by default, beyond what it
asks for itself, a readiness wait's timeout or a grep's time budget; closing
waits at most five seconds. A call that times out or finds the connection
broken drops the connection, and the session's next call opens a new session
for the same directory and options, starting the service again if none runs,
within that call's own bound: one deadline covers reopening and the call. A
service that is busy cannot be told from one that hangs, so a grep that may
run longer than the response timeout passes a time budget.
A write to a service that died fails the call instead of raising `SIGPIPE` in
the host. A host program that starts a service collects it when it exits,
without leaving a zombie process.

### C library and Node package

`fff_create_service_instance` in the C library opens a program session for the
instance's directory and returns the handle that in-process instances use. The
create options choose the index as the corresponding MCP flags do; it also
takes the `fff-mcp` executable that starts the service, an optional namespace
and a timeout that replaces both bounds of the session. Calls on one
service-backed instance may come from any thread and are serialized;
`fff_destroy` must not overlap them. A failed call on a service-backed
instance carries its kind in `int_value`, one of the `FFF_ERROR_*` constants:
the instance lost its service (`FFF_ERROR_LOST_SERVICE`, also for the health
check at the first of its two calls that loses it), a grep cursor of an index
the session left (`FFF_ERROR_STALE_CURSOR`), a call without a service
equivalent (`FFF_ERROR_UNAVAILABLE`), a directory the service cannot serve
(`FFF_ERROR_SCOPE_UNAVAILABLE`), an invalid request
(`FFF_ERROR_INVALID_REQUEST`), a path outside the instance's directory
(`FFF_ERROR_OUTSIDE_SCOPE`), an index whose coverage changed during the call,
which a retry resolves (`FFF_ERROR_RETRY`), or another failure
(`FFF_ERROR_FAILED`). A directory the service cannot index counts as one it
cannot serve. The
kind follows from the error the session or the service returns, which the
program protocol already distinguishes. Search, glob, grep, scan, wait, scan progress, base path and health
functions work on both kinds of instance and return the same result structs;
the health check reports the git work tree from the session's repository state.
Watching, query tracking and history, git status refresh and a restart in
another directory fail on a service-backed instance with an error naming the
call as unavailable over the shared service; a restart in the same directory is
a rescan. Searches and greps have `_ex` variants (`_ex2` for the greps) that
exclude frecency per call, for either kind of instance. On a service-backed
instance the integer offset of the grep functions continues only the
instance's last grep page. `fff_service_call` sends one program request as
JSON and returns the whole reply as one JSON document with the field names of
the result structs, so a binding decodes a result of any size in one pass;
its grep cursors name their index. String arguments of the service functions
that are not UTF-8 fail the call. The in-process functions are unchanged for
the other bindings.

The fork's Node package is `@sakiori/fff-node`. Its platform packages
`@sakiori/fff-bin-darwin-arm64`, `@sakiori/fff-bin-linux-x64-gnu` and
`@sakiori/fff-bin-linux-x64-musl` each carry the native library and the
`fff-mcp` executable of one build at their root; the other `packages/fff-bin-*`
directories stay upstream's and are not published by the fork. The Node
package depends on all three, and they declare no `os`, `cpu` or `libc`, since
a registry may serve neither optional dependencies nor those fields; the package
picks the running platform's one, on Linux by glibc or musl.
`FileFinder.create` always opens a service-backed instance with the
executable of the platform package, starting the service when none of that
build runs, and returns the service's error when it cannot start or be reached
instead of building a private index. `create` and every method that reaches
the service (searches, glob, grep and multi-grep, scan state, waits, rescans,
the health check and `destroy`) return a Promise of the `Result` the method
returned before and run their native call on one of at most four worker
threads the package starts when needed, so a slow or unresponsive service
blocks neither the event loop of the host, typically a server, nor the libuv
thread pool its file system, DNS, crypto and zlib calls use. Idle workers do
not keep the process alive; a worker that dies fails its call and is replaced.
Workers load the library file the main thread resolved, including its
`.asar.unpacked` copy. Calls and replies use a message channel of their own,
not the worker's parent port, where Node posts messages of its own, such as
the modules a worker requires under `node --watch`. Workers start without the
host's Node.js flags, which could change how they read their source, as
`--input-type=module` does. The package does not
implement upstream's synchronous `FileFinderApi`, and `waitForScanBlocking` is
gone; `getBasePath`, `isDestroyed`, the unavailable methods and the static
helpers stay synchronous. Each instance queues its calls and sends one at a time, as the
session serializes them anyway, so an instance holds at most one worker; when
a call finds the service lost, the calls queued behind it fail with its error
at once, and the next call reconnects. No call takes longer than its bound
once a worker runs it, waiting in the instance's queue or for one of the four
workers aside: `create` 90 seconds, a call 30 seconds plus the time it asks
for, a grep's `timeBudgetMs` included, and `timeoutMs` replaces both. The
health check makes two calls, so it can take up to twice the bound. `destroy` lets earlier calls finish, refuses later ones and
always frees the native instance; `nativeInstanceCount` reports the instances
not yet freed.
Every failed `Result` of the package carries a `code` beside the `error` text,
one of the `ErrorCode` constants the package exports: `lostService`,
`staleCursor`, `unavailable`, `scopeUnavailable`, `invalidRequest`,
`outsideScope` and `retry` map the library's kinds, and `unsupportedPlatform`, `libraryUnavailable`, `destroyed`
and `failed` come from the package. The package exports its own `Result`,
`Failure`, `ok` and `err`, whose failures carry the code, in place of the
shared API's. It loads ffi-rs only after finding the platform's native files,
so importing it on a platform ffi-rs has no binary for still works and
`FileFinder.create` reports `unsupportedPlatform`.
`notifyChanged(paths)` reports files the program changed, so that a search
after it resolves sees them without the watcher or a rescan.
`fileSearch`, `directorySearch`, `mixedSearch`, `glob`, `grep` and `multiGrep`
take `excludeFrecency`. Searches, greps, scan state and waits go through
`fff_service_call`; `waitForScan`, `waitForIndexReady` and `getScanProgress`
return the error of a service that went away. `watch`, `trackQuery`,
`getHistoricalQuery` and `refreshGitStatus` return an error naming them as
unavailable over the shared service, `reindex` rescans the instance's own
directory, and `historyDbPath` is refused. `serviceDir`, or `FFF_MCP_SERVICE_DIR`, selects the namespace, and
`FFF_MCP_IDLE_TIMEOUT_SECS` the idle timeout of a service the package starts.
The package finds the library and the executable together once per process:
the library `FFF_C_LIB` names, with the executable beside it, or else both in
its platform package or a development build. It maps the directory holding
them from an `.asar` archive to its `.asar.unpacked` counterpart when that
exists, so Electron applications need no patches; it reports an error when
only one of the files is unpacked, and names the supported platforms on any
other. It ships as an ES module whose CommonJS `require` entry is the same
module, which needs Node 20.19 or 22.12 and later.

`make prepare-node` builds the executable and the library in separate cargo
invocations and stages them for the package's development build;
`make test-node` runs the package's tests, each against a service in a
disposable namespace with a temporary state directory, including overlapping
calls on one and on several instances and a service stopped with `SIGSTOP`,
whose calls fail within their bound while a timer keeps firing and, with more
instances than workers, file reads keep their latency.
`make stage-node-platform` copies the same build into the host's platform
package.

### Building and running

Run `make build-mcp` to build the downstream executable from a checkout in
`target/release`. That build uses the default walker and needs no Zig; builds
with zlob, as `make test-rust` uses, need the Zig version pinned in `mise.toml`;
with mise, run `mise trust` and `mise install` in the checkout once. Configure
each MCP client to launch that executable, passing its desired search directory
as the positional argument. The upstream install scripts and package manager
releases do not contain these downstream changes.

The upstream update notice is off. Upstream's `fff-mcp` asks GitHub for
upstream's latest release when its service starts and tells agents to install it
with upstream's script, which would replace the downstream build. The fork's
service makes no update request and adds no notice to its instructions or tool
results; `--no-update-check` is still accepted, so existing client
configurations keep working.

The first client starts the service automatically, detached from itself: in
its own process group, with `/` as its working directory, standard streams on
`/dev/null` and its startup log, only the variables of the client's environment
it uses (home, user, path, locale, time zone, temporary and XDG directories,
`FFF_` and `RUST_` settings, and nix-ld's `NIX_LD` and `NIX_LD_LIBRARY_PATH`
on NixOS; no `LD_*` or `DYLD_*`, which would load an application's libraries),
and none of the client's descriptors beyond the standard streams: the child
marks them close-on-exec before exec, with `close_range` on Linux, otherwise up
to the soft descriptor limit, or the system's per-process maximum when that is
unlimited, computed before the fork. So the service holds nothing of the
application that started its client, such as the pipe through which an
AppImage keeps its mount alive. A client whose executable lives inside the
running AppImage (`$APPDIR`) starts a copy in the namespace instead, since the
mount ends with the application: it installs the copy of its build once and
reuses it, retries a start the freshly written file briefly refuses, and
removes copies of other builds that no service runs and no client is
starting. Other transient locations, such as a mounted disk image, are not
detected. A client completes MCP
initialization as soon as its session is registered; the index keeps scanning in
the service. Tool calls wait up to 30 seconds for the initial scan and, for
watched sessions, the filesystem watcher, and otherwise ask the caller to retry.
If the watcher cannot start, the index keeps serving searches without live
updates and service status reports the watcher state. A frecency database that
cannot be opened produces a warning instead of a failed session. The existing
MCP tools and stdio configuration remain available. Service status lists each
index with the options it was built with.

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
connection check the peer's user. When the socket path in the namespace
would exceed the platform's limit (104 bytes on macOS, 108 on Linux, including
the terminator), as under a long `HOME`, the service listens in a private
per-user runtime directory instead, under a name derived from a hash of the
namespace path and the build. It tries `$XDG_RUNTIME_DIR/fff` on Linux when
that is set, then `/tmp/fff-<uid>`. A runtime directory is created with mode
0700 and must be owned by the user, private and not a symlink; one that is
not, or cannot be created, is skipped, and only when none is usable does the
service refuse to start, naming each refusal. The namespace's state, including
the endpoint that names the socket, stays in the namespace. The service renews
the times of that socket and its directory every hour, so age-based cleaners
of `/tmp` such as systemd-tmpfiles keep them. On Windows, protected ACLs also
allow SYSTEM and Administrators; directory and file ACLs are validated
independently, and the pipe receives an explicit ACL.

Local connections require the namespace token and a matching protocol version.
Each build runs its own service in the namespace, so a new build can start
while clients of an older build keep running; the older service exits once it
is idle. A build is identified by the `fff-service` version, the commit and the
walker backend, compiled in when it is built; a build from a tree with
uncommitted changes adds a hash of those changes. A copy of an executable that
was moved or re-signed therefore joins the original's service. A client that
starts a service checks that the executable is of its own build and reports
the mismatch otherwise. A client retries through the startup lock when a service shuts down
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

`make test-mcp` runs the MCP and service client tests and the core search
regression suite. `make lint-mcp` checks these crates and their test targets
without relaxing lints.
The shared-service tests use isolated namespaces and cover concurrent startup,
scoped tools and fallbacks, watcher updates, consolidation when an index gains
coverage through a rescan or a directory its watcher indexes, removed scopes,
concurrent searches over one index, cursor isolation, disconnects, parent exit,
side-by-side builds, copied executables, handshake rejection, namespace permissions, stale discovery
recovery, unreachable services, shutdown races, idle cleanup and the absence of
an upstream update check. Program session tests compare every search with an
in-process index of the same directory and cover cursors across a move to a
broader index, sharing with an MCP session, a program that exits without
closing its session or while it waits for the index, one frecency database
spelled two ways, rescans and repository state, searches while the index
builds, scan state while a search holds the index, a watched request beside a
snapshot index, a build tag collision, a session that starts the service
itself, a call that times out and the reconnect after it, a reconnect to a
stopped service within the call's bound, and a namespace whose socket path
exceeds the platform limit. In debug builds, `FFF_MCP_TEST_RUNTIME_DIR` replaces
the runtime directories, so tests never write to the shared ones. In debug builds, indexes stay building
while the file named by `FFF_MCP_TEST_HOLD_READINESS` exists, and program searches
hold their index locked for writing while the file named by
`FFF_MCP_TEST_HOLD_SEARCH` exists. In debug builds, `FFF_MCP_TEST_BUILD_ID` gives
the executable another build identity, so one test binary stands in for two
releases. Each test stops
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
