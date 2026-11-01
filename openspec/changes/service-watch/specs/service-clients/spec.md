## ADDED Requirements

### Requirement: Watch subscriptions over the shared service
A program SHALL be able to subscribe to the changes of files under its
session's directory on the shared service, with an optional pattern and ignore
rules, and to end the subscription. The service SHALL deliver the changes its
own watcher sees for that directory, in batches, with each file's path, the
kind of change (created, modified, removed, renamed with the previous path, or
rescan) and nothing for files the index ignores. A subscription SHALL see every
change made after subscribing returns. A subscription SHALL keep the service
alive like a session and SHALL end when the program ends it, destroys its
instance or exits.

A program that asks for repository state SHALL also receive an event when the
Git state of a repository inside its directory, or containing it, changes (its
index, HEAD or an operation in progress such as a merge), naming that
repository's root. Programs that do not ask for it SHALL NOT receive it.

When the service cannot deliver every change, because a consumer is slower
than its changes, the kernel lost events, the index was rescanned, or the
session moved to another index, the subscription SHALL receive one rescan
event instead and stay open. When the connection to the service is lost, the
subscription SHALL reconnect, starting the service when none runs, and deliver
a rescan event once it is subscribed again.

#### Scenario: A file changes
- **WHEN** a program subscribed to a directory and a file under it is written
- **THEN** the subscription receives a modified event with that file's path

#### Scenario: Ignored files
- **WHEN** a file the index's ignore rules exclude is written
- **THEN** the subscription receives no event for it

#### Scenario: Repository state
- **WHEN** a program subscribed with repository state and a file is staged with `git add` in a repository under its directory
- **THEN** the subscription receives a repository event naming that repository's root

#### Scenario: Nested repositories
- **WHEN** a program subscribed with repository state at the root of a repository nested inside another repository or a broader directory, and a file is staged in the nested repository
- **THEN** that subscription receives a repository event naming the nested repository's root, a subscription with repository state at the enclosing directory receives one naming the same root, and a change of the enclosing repository reaches no subscription of the nested one

#### Scenario: Slow consumer
- **WHEN** a program does not read its events while thousands of files change
- **THEN** the service keeps running for its other clients, and the subscription later receives a rescan event and stays open

#### Scenario: Service restarts
- **WHEN** the service exits while a program is subscribed
- **THEN** the subscription reconnects to a new service and receives a rescan event

#### Scenario: End of a subscription
- **WHEN** a program ends its subscription or destroys its instance
- **THEN** it receives no further events, and the service no longer counts the subscription as a session

#### Scenario: Node watch
- **WHEN** a Node program calls `watch` on a `FileFinder` created with watching enabled
- **THEN** the call resolves once the subscription is ready, its callback receives the batches on the event loop, and the subscription does not keep the process alive
- **AND** a batch that arrives before `watch` resolves describes earlier changes and may be dropped

#### Scenario: Watch without watching
- **WHEN** a Node program calls `watch` on a `FileFinder` created with watching disabled
- **THEN** the call fails with the code `unavailable`

## MODIFIED Requirements

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
- **WHEN** a program calls `trackQuery` or `getHistoricalQuery`
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
