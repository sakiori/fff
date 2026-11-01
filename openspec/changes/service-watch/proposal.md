## Why

Programs that use the shared service through the Node package cannot learn that
files of their directory changed: `watch` is unavailable over the service,
although the service's own watcher already sees every change. An application
that needs change notifications, such as a client that keeps the Git status of
a project current, would have to start a second recursive watcher of a tree the
service already watches. On Linux that watcher also registers ignored folders
such as `node_modules` and runs into the inotify limits.

## What changes

The shared service delivers watch events to programs:

- A program subscribes for its session's directory with an optional pattern and
  ignore rules and receives the changes the service's watcher sees, in batches,
  with the same event kinds as the in-process watcher.
- On request, a subscription also receives an event when the Git state of a
  repository inside the directory changes, so that staging, commits and checkouts
  are seen although files under `.git` are never delivered as file events.
- Lost detail, from a slow consumer, kernel event loss, a rescan or a move to
  another index, becomes one rescan event; a lost connection reconnects and
  delivers a rescan event.
- The C library's watch functions, which the Node package already used before it
  moved to the service, work for service-backed instances, and the Node
  package's `watch` resolves once the subscription is ready.

## Capabilities

### New capabilities

None.

### Modified capabilities

- `service-clients`: watch subscriptions over the shared service; `watch` is no
  longer unavailable in the Node package.

## Impact

- Service protocol: a watch request beside the program session, and its event
  frames. The protocol version changes; a client and a service of one release
  share one build identity, so no negotiation is added.
- Core: a subscription hook for Git state changes beside the existing coverage
  hook.
- C library: the existing watch functions accept service-backed instances, with
  one additional event kind delivered only when repository state was requested.
  Existing callers and kinds are unchanged.
- Node package: `watch` returns a Promise of its result.
