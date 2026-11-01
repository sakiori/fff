## ADDED Requirements

### Requirement: Structured program sessions
The system SHALL let a program open a session on the shared service for a
directory and receive structured search results that match what an in-process
index of the same files returns for the same query and options.

#### Scenario: File, directory and mixed search
- **WHEN** a program searches files, directories or both with a query and a page
- **THEN** it receives items with scope-relative paths, their scores and the total match count

#### Scenario: Ranking without frecency
- **WHEN** a program searches with frecency excluded on an index that tracks frecency for other clients
- **THEN** its results are ranked as by an index without a frecency database, and other clients' ranking is unchanged
- **AND** an empty query lists entries in path order

#### Scenario: Glob
- **WHEN** a program requests a glob page
- **THEN** it receives the matching files of its scope

#### Scenario: Content grep across pages
- **WHEN** a program greps with a plain or regex pattern, smart case, a per-file match limit, a page size and a time budget
- **THEN** it receives matches with line numbers, line content and byte match ranges, the number of files searched and a cursor
- **AND** passing the cursor back continues after the last file of the previous page

#### Scenario: Cursor of a replaced index
- **WHEN** a program's session moved to another index and the program greps with a cursor the old index issued
- **THEN** the grep fails as stale, also after the program grepped on the new index

#### Scenario: Invalid regex
- **WHEN** a regex pattern does not compile and the service falls back to a literal search
- **THEN** the result carries the regex error beside the literal matches

#### Scenario: Scope boundary
- **WHEN** a program's session shares an index that covers a broader directory
- **THEN** every result, count and page is limited to the program's directory

### Requirement: Readiness, refresh and repository state
The system SHALL let a program wait for its index, observe scanning, request a
rescan and learn whether its directory lies in a git work tree.

#### Scenario: Waiting for the initial scan
- **WHEN** a program waits for its index with a timeout
- **THEN** the wait ends when the index is ready, its watcher state is settled and no scan runs or waits to run, or reports that the timeout elapsed

#### Scenario: Search while the index builds
- **WHEN** a program searches before its index has finished the initial scan
- **THEN** the search answers at once from what the index holds so far, limited to the program's directory

#### Scenario: Scan state during a long search
- **WHEN** a program asks for scan progress or the scanning state while another client's search holds the index
- **THEN** the answer does not wait for that search

#### Scenario: Rescan
- **WHEN** a program requests a rescan
- **THEN** the shared index rescans once, coalesced with any rescan already running, and every session on it sees the result

#### Scenario: Reported changes
- **WHEN** a program writes, creates or deletes files in its directory and reports their paths
- **THEN** the service applies them to the session's index as its watcher would, honouring the index's ignore rules and updating its content index, without a rescan
- **AND** a search after the reply sees them, also on an index without a watcher
- **AND** a path outside the program's directory refuses the request with its own error kind, also when it leaves through a symlink
- **AND** a path spelled in another case on a filesystem that ignores case updates the indexed entry instead of adding a second one

#### Scenario: Directory outside git
- **WHEN** a program asks for its repository state in a directory outside any git work tree
- **THEN** the answer reports no work tree

### Requirement: Session lifetime follows the program
The system SHALL end a program's session when the program closes it or its
connection ends, and release indexes no session uses.

#### Scenario: Program exits without closing
- **WHEN** a program's process ends while its session is open
- **THEN** the service ends the session and other clients of the same index are unaffected

#### Scenario: Program exits during a request
- **WHEN** a program's process ends while the service works on its request, such as a readiness wait or a grep
- **THEN** the service ends the session without finishing the request and stops a grep in progress

#### Scenario: Service ends under a program
- **WHEN** the service of a program's session ends
- **THEN** the program's next call fails with an error instead of ending the program, and a wait for the scan reports the error
- **AND** the call after it opens a new session, starting the service again

#### Scenario: Service stops answering
- **WHEN** the service of a program's session is alive but does not answer a call
- **THEN** the call fails after the session's response timeout plus the time the call asks for itself
- **AND** the session's next call opens a new session instead of failing for good, within that call's own limit

### Requirement: Node package backed by the shared service
The Node package SHALL implement `FileFinder` through structured sessions on the
shared service of its own release, starting that service when none runs.
`FileFinder.create` and every method that reaches the service SHALL return a
Promise and SHALL NOT block the event loop while the service works or fails to
answer.

#### Scenario: Error codes
- **WHEN** a call of the Node package fails
- **THEN** its result carries a stable `code` beside the error text: `lostService`, `staleCursor`, `unavailable`, `scopeUnavailable`, `invalidRequest`, `outsideScope`, `retry`, `unsupportedPlatform`, `libraryUnavailable`, `destroyed` or `failed`
- **AND** the code of a failure the service or the native library reports follows from the kind of error they return, not from its text

#### Scenario: Unresponsive service
- **WHEN** the service is alive but does not answer while a program creates an instance or calls one
- **THEN** the event loop keeps running, and the call resolves to a failure naming the timeout after a bounded time
- **AND** calls queued behind it on the same instance fail with the same error at once
- **AND** once the service answers again, the instance's next call succeeds

#### Scenario: Many instances on an unresponsive service
- **WHEN** more instances than the package has worker threads wait on a service that does not answer
- **THEN** the host's file system, DNS, crypto and zlib calls keep their usual latency and its timers keep firing

#### Scenario: Health check of a lost service
- **WHEN** a program asks for an instance's health while the instance loses its service
- **THEN** the check fails with the error of the first call that lost the service, within one call's limit

#### Scenario: Overlapping calls
- **WHEN** a program starts many calls on one instance and on several instances without waiting for each
- **THEN** every call resolves to the result it gets alone

#### Scenario: Destroy with calls in flight
- **WHEN** a program destroys an instance while calls on it are pending
- **THEN** those calls finish, and calls made after the destroy fail naming the instance as destroyed
- **AND** the native instance is freed, also when a call ahead of the destroy lost the service

#### Scenario: Application and agent share an index
- **WHEN** an application creates a `FileFinder` for a directory that an agent's `fff-mcp` of the same release already searches with compatible options
- **THEN** service status lists one index for that directory with both sessions

#### Scenario: Two applications share an index
- **WHEN** two processes create a `FileFinder` for the same directory
- **THEN** one index serves both

#### Scenario: Method without a service equivalent
- **WHEN** a program calls `watch`, `trackQuery` or `getHistoricalQuery`
- **THEN** the call fails with an error that names the method as unavailable over the shared service

#### Scenario: Service unavailable
- **WHEN** the service cannot be started or reached
- **THEN** `FileFinder.create` fails with the service's reason instead of building a private index

#### Scenario: Large result
- **WHEN** a program lists tens of thousands of files in one page
- **THEN** the package decodes the whole page in one pass rather than per item through the native interface

#### Scenario: Unsupported platform
- **WHEN** the package runs on a platform the fork publishes no platform package for
- **THEN** `FileFinder.create` fails with an error naming the supported platforms

#### Scenario: Loading inside an Electron archive
- **WHEN** the package is loaded from an application archive whose native files are unpacked beside it
- **THEN** it loads the native library and starts the service executable from the unpacked location
- **AND** CommonJS `require` of the package works
- **AND** when only one of the two files is unpacked, `FileFinder.create` fails naming the missing one

### Requirement: Published packages
The fork's release SHALL publish to npmjs.org the Node package and platform
packages that carry the native library and the service executable of the same
build for macOS on arm64 and x64 and Linux on x64 and arm64 with glibc and with
musl.

#### Scenario: Fork names
- **WHEN** the release publishes the packages
- **THEN** they are named `@sakiori/fff-node` and `@sakiori/fff-bin-<platform>`, and the Node package resolves the fork's platform package

#### Scenario: Install from npm
- **WHEN** a consumer installs a release of the Node package from npmjs.org with npm or pnpm
- **THEN** only the platform package that matches the running platform's operating system, CPU and, on Linux, C library is installed with it, at the same version
- **AND** the package loads the native files of that platform package

#### Scenario: Platform without a package
- **WHEN** the Node package runs on a platform none of its platform packages matches
- **THEN** `FileFinder.create` fails with `unsupportedPlatform`

#### Scenario: Matching parts
- **WHEN** a consumer installs a release of the Node package
- **THEN** the platform package it resolves holds the native library and service executable built from the same commit as the package's bindings
