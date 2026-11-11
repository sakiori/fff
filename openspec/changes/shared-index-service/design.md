# Shared MCP indexing

## Service boundary

Retain the stdio interface as a lightweight transport adapter. One independently
owned service accepts private local connections and runs the existing MCP
handlers. Index ownership belongs to the service, not the first client.

Use the maintained interprocess crate for portable local sockets and the existing
rmcp implementation for MCP framing and dispatch. Standard file locks serialize
startup and establish exclusive service ownership. Compared with Streamable
HTTP, local sockets preserve automatic stdio startup without introducing an HTTP
listener, authentication configuration, or another MCP proxy implementation.
Platform-specific IPC is contained behind one module.

## Index ownership and scope

Canonical paths identify directories. Index configuration and repository context
are part of the reuse decision. Repository scopes share the repository index;
search scope remains the requested directory. A containing non-repository index
can serve a descendant only when the descendant is covered and has compatible
exclusion semantics. Broader requests replace compatible narrower coverage;
existing sessions keep their own scope and switch index bindings atomically.

Coverage can also grow inside an existing index: the watcher adds a directory
or a rescan commits a tree that now includes one. The core notifies listeners
whenever the set of live indexed directories changes, and the service then
re-evaluates every watched session with the same rules it applies when a session
registers or an index becomes ready. The notification runs on the indexing
thread, possibly under the picker lock, so the listener only signals the
service's consolidation task; signals coalesce, and that task reads indexes and
sessions afresh on each pass. Polling the indexes would add latency and idle
work, and repeating the check only on registration left redundant indexes alive
until an unrelated client connected. Shrinking coverage needs no eager step: a
tool call finds that its index no longer covers its scope and rebinds, and a
scope that no longer exists fails the call until the directory is restored.

Scope filtering belongs in the core before ranking and pagination. It is a field
of the search options rather than a query constraint, so it cannot disappear
during fuzzy or literal retries. A scope resolves once per query to the
contiguous range of index directories under it; scope-relative paths for fuzzy
matching are cached per scan. Built-in exclusions match at any depth with both
walker backends, which makes a parent index equivalent to a standalone index for
a covered descendant. Paths
and glob constraints are interpreted relative to the session scope. Shared file
content and content indexes remain owned by the underlying picker.

## Search execution

One service runs every session's searches on the shared search pool. Rayon's
work stealing lets a worker that waits for its own parallel children run an
unrelated job, so submitting a whole search through `install` with nested
parallel iterators ties its return to other requests. Searches therefore
coordinate on the calling thread, never inside the pool, and dispatch contiguous
blocks of their input through `in_place_scope_fifo` and `spawn_fifo`. Jobs never
nest. A wave spawns at most one job per remaining pool thread and the caller
works on its own share; jobs claim blocks from a shared counter and give their
worker back after a short time slice, so the next wave queues behind work other
requests submitted meanwhile. Blocks are reassembled in input order, and each
job keeps its scratch state across waves so a time-budget abort stays latched.
Inputs below a per-path threshold stay on the caller. Literal, regex,
multi-pattern and fuzzy grep, constraint filtering, fuzzy file and directory
matching, scoring and frecency ranking use this dispatch; candidate selection,
sorting, pagination and per-file line matching run serially on the caller.
Scanning and content-index builds run on the separate background pool. A
service started with full-parallel indexing runs them on the search pool
instead, so searches made during a scan queue behind it.

Dedicated pools per session would isolate requests but multiply threads, and
serializing searches would remove useful concurrency; the scoped Rayon API
provides the needed ownership and ordering without another executor.

## Index freshness during scans

Every scan, initial or repeated, subscribes to filesystem events before walking.
Recursive platforms watch the root before the walk; where watches are per
directory, the walker subscribes each directory before reading its entries or
ignore files. Events arriving during the walk are held and replayed in arrival
order after the scan result is committed, so neither the gap before the watcher
starts nor a rescan's commit can lose a change. The hold is bounded by the
watcher's batch limit; beyond it the events are discarded and one recovery
rescan is scheduled, which rate limiting defers rather than drops. Only the scan
that owns the picker can take the hold. The watcher is reported ready once the
held events are applied. The content-index overlay starts at commit, so a file
edited after the content index read it is re-checked instead of excluded.

Starting a scan is an atomic claim: at most one scan runs per picker. A request
that arrives while a scan runs sets a pending flag, and the finishing scan
starts one follow-up, so concurrent requests coalesce and none is lost.

## Concurrency and lifecycle

Registry locks protect bookkeeping only. Scanning, searching and coverage checks
do not hold a registry lock, and coverage listeners never take a picker lock. Each session owns its cursor store. An index generation change
invalidates pagination rather than applying an old offset to a different index.
Registration does not wait for scanning: each index tracks its own readiness,
and tool calls wait for it with a bounded timeout. A watcher that fails to start
leaves the index usable without live updates. Dropping the last handle to an
index cancels its scan and watcher.

Disconnect releases the session lease; unused indexes are dropped. Idle exit and
explicit shutdown count search sessions only, so diagnostics cannot keep a
service alive or block its shutdown. The service exits after the configured
interval with no sessions. Explicit shutdown refuses connected clients.

Clients reach a service only through its namespace path, discovery file and
socket, so a service checks them periodically. Once the path no longer leads to
the directory it opened, or the discovery file or socket is gone or belongs to a
replacement, no new session can arrive: the service serves its current sessions
and exits when none remain, whatever the idle timeout. Without this check, a
service with idle exit disabled would outlive a removed namespace forever. The
check runs off the service loop with a timeout, so a hung filesystem cannot stall
the service, and only a definite answer counts: an entry that cannot be read, or
a check that does not finish, leaves the service running. The exit follows only
from an answer that arrives once no session remains.

A replacement may own the endpoint and socket names by the time a service exits,
whatever the reason for exiting. The service therefore never lets the socket
library unlink its socket path. It removes the endpoint only while it carries the
service's token and the socket only while it has the identity recorded at bind,
both relative to its opened directory and only while the namespace path still
leads there.

Endpoint, socket and lock names carry a build tag, so each executable build runs
its own service in a shared namespace. Upgrades need no coordinated shutdown:
older services exit when their clients leave. Compared with rejecting mismatched
builds, this keeps searches available during an upgrade at the cost of one
index set per running build.

A client starts the service detached from itself, since the service outlives it
and serves every client of the namespace. On Unix the service gets its own
process group, `/` as its working directory, standard streams on `/dev/null`
and its startup log, and only the variables of the client's environment it uses
(home, user, path, locale, time zone, temporary and XDG directories, and its own
`FFF_` and `RUST_` settings); a client's loader, git or application variables
would otherwise apply to every client. Between fork and exec the child marks
every descriptor above the standard streams close-on-exec, with `close_range`
on Linux and a loop up to the descriptor limit elsewhere, so the service holds
nothing the client inherited without that flag, such as the pipe through which
an AppImage runtime keeps its mount alive. Marking rather than closing keeps the
pipe through which the standard library reports a failed exec. Once the service
no longer holds that pipe, the AppImage's mount ends with its application, and
a service still executing from it would fail on its next page-in; an executable
inside the running AppImage (`$APPDIR`) is therefore copied into the namespace
first and started from there. A copy is the same build, so it joins the same
service; it is installed atomically under its build's name, reused by later
starts, and removed by a client of another build once no service of its build
runs and no client of it is starting, which both hold that build's locks. The
environment excludes `LD_*` and `DYLD_*`: the glibc build needs only the system
loader, which finds glibc through its own configuration, while an application's
library path would load the application's libraries into the service; nix-ld's
`NIX_LD` variables stay, without which NixOS cannot start a binary built
elsewhere. Without `close_range`, the child walks every descriptor up to a
bound computed before the fork, since nothing between fork and exec may
allocate. Other transient locations, such as a mounted disk image, are not
detected; the service runs from the path it is given.

A private namespace, an authenticated typed handshake and an exclusive ownership
lock protect discovery and stale-socket recovery. The launching client removes
stale discovery state under the startup lock, the endpoint is replaced
atomically, and a client retries through the lock when a service stops before
accepting it. On Unix the namespace path is opened without following symlinks,
its parents must not be writable by other users, state files are opened relative
to the verified directory, and both ends check the peer's user. Clients starting
together create the same lock files, so a state file is created exclusively or
else opened as an existing file: a directory-relative create that may open an
existing file is not atomic on every kernel and can fail for all but one of the
racing creators. The retries are bounded, so a removed directory fails instead
of looping. Windows directory, file and pipe permissions grant access only
to the user and privileged OS principals. A failed service connection fails
visibly rather than silently creating a private index.

## Verification

Use disposable directories and service namespaces. Verify concurrent startup,
same-root and nested-root reuse in both connection orders, exclusion boundaries,
relative paths, all search tools and fallback paths, pagination, watcher updates,
parent exit, stale discovery state, unreachable services, and idle cleanup.
Verify independent completion of overlapping searches with gated work, changes
made between a scan's walk and its commit with a test hook, and a session that
built its own index moving to a parent index once the parent's watcher indexes
its directory, with a debug-build hook that holds new-directory indexing, all
without relying on timing. Retain existing SDK tests.
