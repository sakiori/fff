## ADDED Requirements

### Requirement: One service for independent clients
The system SHALL automatically connect compatible clients belonging to the same
OS user to one service, including when clients start concurrently.

#### Scenario: Concurrent startup
- **WHEN** multiple clients start against an unused service namespace
- **THEN** exactly one service per executable build owns the namespace and each client can initialize

#### Scenario: Large index at startup
- **WHEN** a client starts for a directory whose initial scan takes longer than the host's startup timeout
- **THEN** initialization completes without waiting for the scan
- **AND** tool calls wait a bounded time for the index and otherwise ask the caller to retry

### Requirement: Scope is independent of index coverage
The system SHALL restrict every search, fallback, count, and page to the client's
canonical scope before ranking or pagination and interpret paths relative to it.

#### Scenario: Parent and descendant clients
- **WHEN** clients search a directory and its descendant with compatible policies
- **THEN** they reuse index coverage while the descendant only receives its files

#### Scenario: Changes after the first search
- **WHEN** a watched client receives results from a tool call
- **THEN** its filesystem watcher is ready to observe subsequent changes, or its failure is reported

#### Scenario: Watcher unavailable
- **WHEN** the filesystem watcher for an index cannot start
- **THEN** the index keeps serving searches without live updates and service status reports the watcher state

#### Scenario: Empty descendant created after startup
- **WHEN** an included empty descendant is created or restored after the initial scan
- **THEN** the watcher updates directory coverage and compatible clients can share it
- **AND** excluded descendants retain independent coverage, including inherited ignore files and hidden-directory policy

#### Scenario: Repeated directory capacity exhaustion
- **WHEN** directory additions exceed incremental capacity during a rescan cooldown
- **THEN** recovery remains pending until an allowed rescan restores coverage
- **AND** recovery does not require another filesystem event or bypass rate limiting
- **AND** any admitted rescan satisfies the pending request

#### Scenario: Broader scope arrives later
- **WHEN** a broader compatible scope is requested after a descendant
- **THEN** coverage expands and clients with watching enabled retain their original scopes

#### Scenario: Existing index grows to cover a descendant
- **WHEN** a watched client holds its own index and a compatible broader index later covers its scope through a watcher update or a rescan
- **THEN** the client moves to the broader index without another client connecting, its old cursors become invalid, and the unused index is released

#### Scenario: Scope removed while bound
- **WHEN** a client's directory is removed while the client is connected
- **THEN** its tool calls report the scope as unavailable without affecting other clients
- **AND** they succeed again once the directory is restored

#### Scenario: Snapshot client
- **WHEN** a broader scope arrives while a descendant has watching disabled
- **THEN** the descendant keeps its original index snapshot

#### Scenario: Conflicting indexing policies
- **WHEN** roots differ in repository or exclusion semantics or indexing options
- **THEN** reuse does not silently omit files or change either client's policy

#### Scenario: Changes during a scan
- **WHEN** files or directories are created, removed or edited while an index is scanning
- **THEN** the index reflects them once its watcher is reported ready, without an additional rescan
- **AND** edited content remains findable by content search

#### Scenario: Rescan requested during a scan
- **WHEN** rescans are requested while a scan of the same index is running
- **THEN** only one scan runs at a time and the requests are satisfied by one follow-up scan

### Requirement: Independent completion of concurrent searches
The system SHALL return a search whose own work has completed without waiting
for an unrelated concurrent search, using a bounded pool shared by all sessions.

#### Scenario: Another search remains active
- **WHEN** one client's search has completed its work while another client's search is still running on the same index
- **THEN** the first search returns without waiting for the second

#### Scenario: Parallel execution preserves results
- **WHEN** a search runs concurrently with others
- **THEN** its results, order, offsets, scope, pagination, cancellation and time budgets match a search running alone

### Requirement: Independent session state
The system SHALL keep pagination and connection lifetime independent per client.

#### Scenario: Interleaved searches
- **WHEN** a client pages through one search after running a different search
- **THEN** the earlier cursor remains valid while the index binding is unchanged

#### Scenario: Client disconnects
- **WHEN** one client disconnects or its parent exits
- **THEN** other clients continue searching and unused indexes are released

### Requirement: Bounded service lifecycle
The system SHALL release unused service resources after a configurable idle
period, end services that clients can no longer reach, recover from stale
discovery state, and reject incompatible protocols.

#### Scenario: Last client leaves
- **WHEN** no search sessions remain for the idle period
- **THEN** the service exits without signalling unrelated processes
- **AND** status requests during that period do not extend it

#### Scenario: Unreachable service
- **WHEN** a service's namespace directory, discovery file or socket is removed or taken over by a replacement service
- **THEN** the service finishes its current sessions and exits, even when idle exit is disabled
- **AND** the replacement service stays reachable

#### Scenario: Indeterminate reachability
- **WHEN** a service cannot read its namespace entries or its check does not finish
- **THEN** the service keeps running and checks again later

#### Scenario: Exit after the namespace was replaced
- **WHEN** a service stops or exits idle after a replacement service started in a new namespace directory at the same path
- **THEN** the replacement's discovery file and socket stay in place and clients can connect to it

#### Scenario: Unobservable parent
- **WHEN** a client cannot observe its parent process and receives no MCP traffic for the idle period
- **THEN** the client exits and releases its session

### Requirement: Observable sharing
The system SHALL expose diagnostics sufficient to verify active sessions,
canonical scopes, and the indexes they share.

#### Scenario: Inspect active service
- **WHEN** service status is requested
- **THEN** it reports the active scopes and distinct indexes without starting scans

#### Scenario: Explicit shutdown
- **WHEN** shutdown is requested while a client is connected
- **THEN** the service refuses shutdown and the client remains usable

#### Scenario: Side-by-side builds
- **WHEN** a client of a different executable build starts while an older service is running
- **THEN** it uses a service of its own build, the older service keeps its clients, and status lists both

### Requirement: Private service namespace
The system SHALL reject namespace directories and state files accessible to
unprivileged users other than the service user, and namespace parents writable
by them. Local IPC SHALL require the private namespace token; Unix connections
SHALL verify the peer's user, and Windows pipes SHALL use an explicit private ACL.

#### Scenario: Public discovery file
- **WHEN** a discovery file grants access to another unprivileged user
- **THEN** the client refuses to trust its endpoint or token

#### Scenario: Inheritable public permissions
- **WHEN** a namespace directory would give state files public access through inheritance
- **THEN** the service refuses to use that directory

#### Scenario: Unsafe namespace parent
- **WHEN** a parent of the namespace directory is writable by another unprivileged user and is not a root-owned sticky directory
- **THEN** the service refuses to use that directory
