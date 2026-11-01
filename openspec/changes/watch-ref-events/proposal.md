## Why

A program that follows a repository through a watch subscription learns when
its index, HEAD or an operation in progress changes, but not when a branch, a
tag or a remote-tracking ref is created, moved or deleted: those changes stay
in `.git/refs`, `packed-refs` or reftable tables, which the watcher deliberately
leaves out. An application that shows a repository's branches and history
therefore misses a `git branch`, `git tag`, `git fetch` or `git pack-refs` run
in a terminal, and in a linked worktree it misses every change of the branches
it shares with its main repository.

## What changes

- A watch subscription can ask for refs. It then also receives a refs event,
  naming the work tree, when a branch, tag or remote-tracking ref of a
  repository inside its directory, or containing it, is created, moved or
  deleted, loose, in `packed-refs` or in reftable tables, and when a work tree's
  HEAD moves, which stays a repository event as well.
- Branches of a linked worktree are read from its main repository's common dir,
  so a change there reaches the subscriptions of every work tree sharing it; a
  HEAD reaches only its own work tree's.
- Refs events coalesce per work tree like repository events, and lost detail
  still becomes one rescan.
- Subscriptions that do not ask for refs receive nothing new. An index watches
  the refs only of the repositories related to a refs subscription's directory,
  and only while one is subscribed.
- Where changing the watched paths restarts the watcher's event stream, as on
  macOS, the index's tree keeps a stream of its own that never restarts, so
  refs subscriptions cost no rescan, and git state lost by the other stream is
  announced again.
- The C library's watch options and the Node package's `watch` take the new
  option and deliver the new event kind.

## Capabilities

### New capabilities

None.

### Modified capabilities

- `service-clients`: refs events for watch subscriptions that ask for them.

## Impact

- Core: a ref listener with a scope beside the repository state listener, and
  watches of the refs of the repositories it follows while one is subscribed;
  watch changes go to the backend as one update each, and with FSEvents to a
  stream of the paths outside the index's tree.
- Service protocol: the watch request gains `refs` and the event kind `Refs`;
  the protocol version changes.
- C library: `FffWatchOptions` version 3 appends `refs`, `fff_watch_args_ex2`
  takes it, and the kind `FFF_WATCH_EVENT_REFS` (8) is delivered only when
  asked for. Earlier options, kinds and layouts are unchanged.
- Node package: `watch` takes `refs` and delivers events of kind `refs`.
