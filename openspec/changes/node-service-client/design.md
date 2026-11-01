# Program clients of the shared service

## Client library

The service client (namespace checks, discovery, startup locks, handshake,
spawning the service) lives in `fff-mcp` and is private to it. A Node client
written in JavaScript would have to repeat the namespace privacy checks and OS
file locks, which Node does not provide. Instead the client, the protocol types
and the namespace module move into a library crate that `fff-mcp` and `fff-c`
both depend on. `fff-c` exposes service-backed instances through the same
handle and result types its in-process functions use:
`fff_create_service_instance` takes the existing create options, the service
executable and an optional namespace. Reading a result through those structs
costs the Node bindings one FFI round trip per field of every item, which for a
listing of tens of thousands of files dominates the call. `fff_service_call`
therefore takes one program request as JSON and returns the whole reply as one
JSON document with the field names of the result structs, which JavaScript
decodes natively in one pass; the Node package uses it for every search, grep,
scan state and wait. The per-item functions stay for the other bindings. Searches and greps gain `_ex` variants
(`_ex2` for the greps that already had one) with a trailing `exclude_frecency`,
which in-process instances honour as well. Calls without a service equivalent
(watching, query tracking and history, git status refresh, a restart in another
directory) fail on a service-backed instance with an error naming them as
unavailable over the shared service; a restart in the same directory is a
rescan, and creating one with a query history database is refused.

The C library needs the path of the service executable to start a service. The
Node package passes the executable from its platform package; the build identity
of that executable must equal the library's, which the release guarantees by
building both from one commit.

The library is the crate `fff-service`. Besides the asynchronous client that
`fff-mcp` uses, it offers a blocking program session for `fff-c`: each session
drives its own single-threaded Tokio runtime and blocks on the reply, as the C
functions block on an in-process search. A service the library starts is a
child of the host program, which usually outlives it, so the library spawns it
through the standard library rather than Tokio's process driver, which would
install a `SIGCHLD` handler in the host, and a thread waits for the child once
the client lets go of it, so a long-running host collects no zombies. For the
same reason the session must not let a dead service end its host: a host that
does not ignore `SIGPIPE` would die on a write to the closed connection. On
Linux the standard library writes to Unix sockets with `MSG_NOSIGNAL`; macOS has
no such flag, so the client sets the socket's `SO_NOSIGPIPE` option. Every call
has a time limit, searches included: a response timeout beyond what the call
asks for itself (a readiness wait's timeout, a grep's time budget), 30 seconds
by default, which no search over an index approaches; opening takes at most a
connect timeout, 90 seconds by default, which covers starting the service.
Both are part of the session's configuration. An open that fails or times out
does not wait for its blocking work. A call that times out leaves the stream
mid-frame, so the session drops the connection, which also ends the service's
side of it and stops a grep in progress, and the next call opens a new session
within its own deadline rather than failing for good: a long-running host
keeps its instance across a service that hung or restarted, and no call takes
longer than its limit. The session sends paths absolute, and
the frecency database in the spelling the MCP client uses, since the service
resolves paths from its own directory and compares databases by name.

## Asynchronous Node API

A typical consumer runs the Node package inside a server, where a synchronous
FFI call holds the event loop for as long as the service takes to answer:
every client of the server stalls for normal IPC latency, and for the whole
time limit when the service is alive but does not answer. `FileFinder.create`
and every method that reaches the service therefore return Promises, and their
native calls block a thread of their own instead.

That thread is not one of the libuv pool, which ffi-rs's `runInNewThread`
would use: the pool has four threads by default and serves the file system,
DNS, crypto and zlib APIs of the whole process, so four instances waiting on a
hung service would stall all of those for the length of their time limits. The
package runs the calls on worker threads it owns, at most four, started when a
call needs one. A worker loads the same ffi-rs module and native library file
the main thread resolved, so an Electron application's `.asar.unpacked` copy
serves both, and refers to an instance by its native address, which is valid
in every thread of the process. Workers are unreferenced while idle, so they
never keep a process alive by themselves, and referenced while a call runs,
which bounds how long they can. A worker that dies fails its call and is
replaced by the next call that needs one.

Four workers let four instances reach the service at once, which covers the
concurrency a host gets from one service, whose sessions answer one call at a
time each; more would cost a V8 isolate and a copy of ffi-rs per thread for
parallelism the service does not provide, and fewer would let one hung call
delay unrelated instances. The pool's size does not depend on the number of
instances: calls of all instances wait in one queue, and each instance has at
most one call in it at a time, so instances share the workers fairly and an
instance whose service hangs holds at most one worker.

Each instance queues its own calls in JavaScript, which preserves their order
and costs nothing since the session serializes calls anyway. Every call is
bounded in the native library by one deadline that also covers reopening a
lost session, so a worker is always released. When a call reports that it lost
the service, which the C library marks in the failure's `int_value`, the calls
queued behind it fail with the same error at once instead of each waiting out
the limit again, and the next call reconnects. `destroy` takes its turn in the
queue but always runs, so calls made before it finish, none overlaps the
freeing of the instance, and a lost service never leaks it. The health check
stops at the first call that loses the service and fails like any other call;
it makes two calls, so it can take up to twice the bound. A bound starts when a
worker runs the call: with more unresponsive instances than workers, a call
first waits for a free worker.
A healthy service cannot be told apart from an unresponsive one while it
works, so a grep that may need longer than the instance's limit passes a time
budget, which the limit then includes. The package drops upstream's
synchronous `FileFinderApi` and its blocking `waitForScanBlocking`; accessors
that need no service stay synchronous.

## Error codes

Programs branch on failures: a lost service is retried, a stale grep cursor
means searching again from the first page. Matching error text would tie them
to wording that changes. Every failed `Result` of the Node package therefore
carries a `code`. Failures the service and the native library report keep the
kind they already have on the wire and in Rust: the C library classifies the
session's error, a lost connection, a stale cursor, a refused scope or an
invalid request, into one of its `FFF_ERROR_*` constants in the failure's
`int_value`, and the package maps the constant to its code. The protocol needs
no change, so `fff-mcp` clients of the same build are unaffected. Codes for
failures that never reach the library (a destroyed instance, a platform without
native files, an option the shared service does not support) come from the
package itself. A constant the package does not know, from a newer library,
becomes `failed`, so a consumer never sees a code the package does not declare.
The package requires ffi-rs only once it has found the native files of the
running platform: ffi-rs refuses to load where it has no binary of its own, and
importing the package there must still yield `unsupportedPlatform` from
`FileFinder.create` rather than a failed import.

## Reported changes

A program that writes files and searches right after cannot wait for the
watcher, whose events arrive after its debounce, and a rescan per write would
walk the whole tree. Programs therefore report the paths they changed, and the
service applies them through the same per-path code the watcher's events take:
the paths become synthetic modification events for `handle_debounced_events`,
which decides from the filesystem whether each is an addition, an update or a
removal, applies the index's ignore rules and updates the content index. A
report and a watcher event for the same change apply the same update twice,
which leaves the index as one would. A report that arrives while a scan runs
goes through the scan's event hold like a watcher event, since the scan is
about to replace the index it would update. Paths must lie inside the
session's directory; a session may share a broader index, but it must not
change entries other sessions own. The index holds every path in its on-disk
spelling, so a reported path is spelled the same way before it is applied:
normalised lexically, resolved component by component with its on-disk case,
and, for a deleted path on a filesystem that ignores case, matched against the
indexed entries regardless of case. Otherwise a report spelled `SRC/Foo.ts`
would add a second entry beside `src/foo.ts` that no watcher event ever
removes.

## Socket placement

A namespace under a long home directory gives a socket path beyond `sockaddr_un`'s limit, and the service could not
listen. The service then places its socket in a private per-user runtime
directory, as tmux and ssh-agent keep their sockets short, under a name
derived from a hash of the canonical namespace path and the build tag, so
namespaces never share a socket. It tries `$XDG_RUNTIME_DIR/fff` on Linux when
that is set, then `/tmp/fff-<uid>`: a runtime directory that is missing or
belongs to another user, as in containers or after switching users, falls
through to the next, and only when every one fails does the service refuse,
naming why each did. The directory is created with mode 0700 and opened like a
namespace, through directory handles from the root with the final component
not followed, and must be owned by the user and private; a symlink or a
directory another user created in its place makes the service refuse to start
rather than listen there. Clients find the socket through the endpoint in the
namespace as before, so the namespace stays the only place of discovery, and
the peer check and token still authenticate the connection. A short namespace
keeps its socket in place. Age-based cleaners such as systemd-tmpfiles remove
entries of `/tmp` unused for days, which would cut off a long-running service,
so the service renews the times of a socket in a runtime directory, and of the
directory, every hour.

## Protocol

The handshake's request enum gains a program session beside the MCP session,
and the protocol version increases. After the service accepts a program session
the connection carries length-prefixed request and reply frames, one request at
a time per connection; a program that wants concurrent calls opens more
sessions. Requests mirror the core calls the Node API needs: file, directory and
mixed fuzzy search, glob, grep and multi-grep, readiness wait, scanning state,
scan progress, rescan and repository state. Scan progress counts the files of
the session's directory once the index is ready, which is what an in-process
index of that directory reports. Replies carry core result data with scope-relative
paths. MessagePack keeps a 25,000-entry listing compact and cheap to decode; the
handshake stays newline-delimited JSON as before.

A grep cursor names the index that issued it beside the core's file offset. A
session that is rebound to another index invalidates its cursors as MCP cursors
are invalidated today, and a grep with an old cursor reports it, also after the
session grepped on the new index; tracking only the index of the session's last
grep could not tell an old cursor from a new one. The integer offsets of the C
grep functions cannot name an index, so on a service-backed instance they
continue only the instance's last grep page.

Requests carry the core's own parameters: pagination as an item offset and
limit, grep options with the cursor as the file offset, and whether grep
constraints use the syntax of instances in AI mode, which `fff-c` chooses by
the instance's mode. A file's git status travels as the raw status bits. The
readiness wait ends once the initial scan and watcher have settled and no scan
runs or waits to run: the core clears its scanning flag before the post-scan
phase and starts a rescan requested meanwhile only when that phase ends, so the
wait follows the core's scan job and pending rescan instead. A wait lasts at
most an hour.

Program searches do not wait for the initial scan. An in-process index answers
from what it holds while it scans, and applications search once their own
readiness timeout passes, so a search answers at once with what the index holds
so far; tool calls keep their bounded wait. The scanning state and scan progress
read the core's atomics instead of the index lock, which a long search with an
index update queued behind it can hold longer than a client waits for a control
call; a file count the index cannot give at once repeats the session's last one.

The service reads nothing from a program session while it handles a request,
so it watches the connection meanwhile: when the program goes away, the session
ends at once and a grep in progress is aborted, rather than when the request
finishes.

The Node API is synchronous except for its waits, so the C library blocks on the
reply as it blocks on an in-process search today. Search work stays in the
service's bounded search pool and does not compete with other clients' searches.

## Index reuse

The index key keeps what decides the file set: canonical root, repository
context, exclusion semantics and symlink policy. Content indexing, watching,
warmup and the content cache budget become capabilities the selected index must
offer at least as the request needs them; a request that needs a capability the
covering index lacks gets a new index, which also offers the capabilities of the
indexes at the same root it can replace, and consolidation moves compatible
sessions once it is ready, as it does for broader coverage today. An index that
a snapshot session keeps is never replaced, since snapshot sessions keep their
binding, so the new index does not copy its capabilities. Treating
warmup and the cache budget as properties of whichever request came first would
leave agents on the weaker index of an application that disabled them. A budget
left to the core is compared by the bounds of the core's size-based budget.
Search mode and the frecency database are index properties chosen by the
request that creates the index; later requests accept them unless they name a
different frecency database. Home and root permission are checked per request
against the canonical root.

Frecency scores are stored on each file but added to a result only while it is
scored, so whether they count is a per-request search option. A program that
asks for ranking without frecency gets the order an index without a frecency
database gives, while agents on the same index keep theirs; an empty query then
lists entries in path order, and grep searches files by modification time
alone, newest first. Keeping ranking in the index key instead would
hold a second scan, watcher and cache per directory only to change the order of
results.

## Build identity

The identity becomes a hash of the crate version, the commit and the enabled
features, computed at build time. Hashing the executable separated a release
from the same release re-signed inside an application bundle. Development
builds from a dirty tree include a marker so they never join a release service.

The `fff-service` build script computes it, so the service executable and every
library that links the client carry the same value when built from one tree.
The features are the walker backend, which the executable and the library
forward to `fff-service`. The dirty marker is a hash of the names and contents
of the workspace source files that differ from the commit, tracked and
untracked: two development builds with different changes do not join each
other's service either, while two builds of the same tree, such as the
executable and the C library, still agree. It hashes files rather than git's
diff output, which varies with git's configuration and with the abbreviation
length of object names in a clone, so the same tree built on different machines
and targets has one identity. A release that stamps its version into the
manifests before building therefore builds every artifact of every target after
the same stamping. A client that starts a service
passes the identity it expects; an executable of another identity refuses to
start and the client reports its reason instead of waiting for an endpoint that
never appears.

## Packaging

Each platform package carries `libfff_c` and `fff-mcp` at its root. The Node
package resolves both from one location, the library `FFF_C_LIB` names with the
executable beside it, its platform package or a development build, never one
from each, and maps a path inside an `.asar` archive to its
`.asar.unpacked` counterpart when that file exists. Its CommonJS entry is the
same module as its ESM entry: `require`
loads the ES module, which needs Node 20.19 or 22.12 and later, and avoids two
copies of the native state in one process. `make stage-node-platform` copies
one build of both into the host's platform package.

The fork publishes these packages under its own scope, `@sakiori/fff-node` and
`@sakiori/fff-bin-<platform>`, to npmjs.org, so they can never be mistaken for
upstream releases. The Node package resolves its platform package by the fork's
names. Only the platform packages the fork builds carry its names; the Windows
`fff-bin-*` directories stay upstream's, and the version script gives each
consumer the platform packages of its own scope. A consumer that imports the
upstream name keeps its imports with a package-manager alias. The release
pipeline builds them for macOS arm64 and x64 on the macOS runner and for Linux
x64 and arm64, glibc and musl, with cargo-zigbuild, in the same jobs and from
the same stamped tree as the `fff-mcp` archives, so that a Node application and
the agents that install `fff-mcp` from the same release share one service.
Where a target has both, the platform package carries the archive's `fff-mcp`
file. `libfff_c` and `fff-mcp` come from separate cargo invocations on every
target, since one invocation would unify `fff-mcp`'s dependency features into
the library. On musl they also differ in linking: the executable links musl
statically so that it runs on any Linux, and a statically linked musl target
cannot produce a shared library, which a Node.js on musl must load dynamically.
The identity hashes neither the link flag nor dependency features beyond the
walker backend; the build compares the identity each invocation reports, and
the packing job compares those of all targets. The glibc builds target glibc
2.28, the floor of Node.js's own Linux builds.

The platform packages follow upstream's pattern: each declares `os`, `cpu` and
on Linux `libc`, and the Node package lists them as optional dependencies pinned
to its own version, so npm and pnpm install only the running platform's one.
The package still tells glibc from musl on Linux at run time to pick it, and
reports an unsupported platform when no package matches.

The packages go to npmjs.org in the tag's pipeline by trusted publishing, with
provenance, the platform packages before the Node package that names them, and
before the archives and the GitLab Release that marks the whole release
published. npm never replaces a version, so a retried job skips a package
published with the same tarball and fails on one published with another.

## Verification

Service tests open program sessions in disposable namespaces and check every
request against the in-process results for the same directory, cursor
continuation and invalidation on rebind, also after a grep on the new index, an
MCP session and a program session sharing one index, capability-based reuse in
both arrival orders, separation by symlink policy, one frecency database
spelled two ways, searches while the index builds, scan state while a search
holds the index, a watched request beside a snapshot index, and session release
when the program's connection drops, also while it waits on a request. The Node
package's tests run against a service in a disposable namespace. A
build-identity test checks that a copied and re-signed executable joins the
original's service.
