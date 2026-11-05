## Why

A program that follows a repository's refs through a watch subscription is not
told when a linked worktree the index does not follow switches branches. The
switch moves only that worktree's HEAD, in its admin dir in the common dir. The
index's ref watches receive that file and drop it, so such a program has to
watch the same directories a second time to hear that one file.

## What changes

- The HEAD of a linked worktree the index does not follow is state the
  repository's work trees share: a change of it is a refs event for every
  followed work tree sharing the common dir, scoped and coalesced as refs events
  are. In a reftable repository, where the file is a stub, the worktree's own
  table list stands for it.
- The other files of that admin dir, its index, logs and locks among them, stay
  dropped by their prefix. It stays no repository of the index, and its git state
  no repository event.
- A worktree a subscription's directory is related to names itself to that
  subscription, also in its repository event. Any other subscription of the
  repository hears the event as for a worktree the index does not follow, whatever
  other subscriptions follow. A worktree the index ignores counts as not followed,
  as the index tells nothing of it.
- Where a backend watches directory by directory, the watch of an admin dir added
  after the watch began is followed by one refs event, as its HEAD may have moved
  before, and an admin dir removed and added again is watched anew, also when both
  happen before the watcher looks and the last worktree took `worktrees` along.

## Capabilities

### New capabilities

None.

### Modified capabilities

- `service-clients`: refs events for the HEAD of a worktree the index does not
  follow.

## Impact

- A view of a main checkout hears a switch in each of its worktrees whether they
  lie ignored below it, beside it or open below it, and whether or not an index of
  their own serves another view.
- Core only; the protocol, the C library and the Node package's API do not
  change, and subscriptions that do not ask for refs receive nothing new.
- No new watch for loose and packed refs: FSEvents' recursive watch of
  `worktrees` and each admin dir's watch elsewhere deliver the file already. A
  reftable repository costs one more watch per linked worktree where a backend
  watches directory by directory.
- A consumer hears one refs event per followed work tree of the repository for
  each move of such a HEAD, and reads the worktrees it cares about from there;
  the event does not say which worktree moved.
