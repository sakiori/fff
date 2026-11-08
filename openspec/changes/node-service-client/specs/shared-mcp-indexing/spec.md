## ADDED Requirements

### Requirement: Index reuse follows what a request needs
The system SHALL reuse an index for a request whenever the index holds the files
the request's policy includes and offers every capability the request needs.
Options that decide which files an index holds (repository context, exclusion
semantics, extra ignore rules and the ignore sources a request turns off,
following symlinks) SHALL match. Capabilities (content indexing,
watching, warmup, content cache budget) MAY exceed the request and SHALL NOT
fall short of it. Search mode and the frecency database belong to the index; a
request that names its own frecency database only shares indexes that use the
same database, however its path is spelled, and a request may exclude frecency
from its ranking.
Permission to index a home or filesystem root SHALL be checked per request and
SHALL NOT separate indexes.

#### Scenario: Paths-only request beside a content index
- **WHEN** a request without content indexing arrives for a directory whose index has content indexing
- **THEN** the request uses that index

#### Scenario: Content request beside a paths-only index
- **WHEN** a request that needs content indexing arrives for a directory whose index has none
- **THEN** the service builds a content index and moves compatible sessions to it once it is ready
- **AND** releases the paths-only index when no session uses it

#### Scenario: Agent beside an application's lighter index
- **WHEN** a request with warmup or a larger content cache budget arrives for a directory whose index was created without warmup or with a smaller budget
- **THEN** the service builds an index offering both requests' capabilities and moves compatible sessions to it once it is ready

#### Scenario: Watched request beside a snapshot index
- **WHEN** a watched request arrives for a directory whose only index serves snapshot requests
- **THEN** the service builds a watched index offering what the watched request needs, without the snapshot index's other capabilities
- **AND** the snapshot sessions keep their index

#### Scenario: Different symlink policy
- **WHEN** two requests for the same directory differ in following symlinks
- **THEN** they use separate indexes

#### Scenario: Different ignore options
- **WHEN** two requests for the same directory differ in their extra ignore rules, or in turning off Git integration or ignore files
- **THEN** they use separate indexes

#### Scenario: Extra ignore rules inside a broader index
- **WHEN** a request with extra ignore rules arrives for a directory inside the directory of an existing index
- **THEN** the service builds an index of the request's own directory, since the rules are relative to it, and does not use the broader index
- **AND** inside a repository that index applies the repository's ignore rules above its directory, as an index of the repository does there

#### Scenario: Index without Git integration
- **WHEN** a request turns off Git integration
- **THEN** its session reports no repository, and its watch subscriptions receive no repository or refs events

#### Scenario: Snapshot request
- **WHEN** a request with watching disabled arrives where a watched index exists
- **THEN** the request may use the watched index, and keeps its binding when broader coverage arrives later

## MODIFIED Requirements

### Requirement: One service for independent clients
The system SHALL automatically connect compatible clients belonging to the same
OS user to one service, including when clients start concurrently. Clients of
the same build identity, compiled into the executable from its version, commit
and features, SHALL use the same service regardless of the executable's path or
of signatures applied after the build.

#### Scenario: Concurrent startup
- **WHEN** multiple clients start against an unused service namespace
- **THEN** exactly one service per build identity owns the namespace and each client can initialize

#### Scenario: Large index at startup
- **WHEN** a client starts for a directory whose initial scan takes longer than the host's startup timeout
- **THEN** initialization completes without waiting for the scan
- **AND** tool calls wait a bounded time for the index and otherwise ask the caller to retry

#### Scenario: Namespace with a long path
- **WHEN** the socket path in a client's namespace exceeds the platform's limit
- **THEN** the service listens in a private per-user runtime directory under a name derived from the namespace, and clients of that namespace reach it
- **AND** the service skips a runtime directory that is missing, a symlink, open to other users or owned by another user, and refuses to start only when none is usable, naming each refusal
- **AND** the service renews its socket's times so that age-based cleaners of shared temporary directories keep it

#### Scenario: Re-signed copy of a release
- **WHEN** a client runs from a copy of a release that an application bundle re-signed, while a client of the same release installed elsewhere uses a service
- **THEN** both clients use that service

### Requirement: Observable sharing
The system SHALL expose diagnostics sufficient to verify active sessions,
canonical scopes, and the indexes they share.

#### Scenario: Inspect active service
- **WHEN** service status is requested
- **THEN** it reports the active scopes, whether each session is an MCP or a program session, and the distinct indexes without starting scans

#### Scenario: Explicit shutdown
- **WHEN** shutdown is requested while a client is connected
- **THEN** the service refuses shutdown and the client remains usable

#### Scenario: Side-by-side builds
- **WHEN** a client of a different build identity starts while an older service is running
- **THEN** it uses a service of its own build identity, the older service keeps its clients, and status lists both
