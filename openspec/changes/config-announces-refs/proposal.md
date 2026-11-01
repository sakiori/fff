## Why

A program that follows refs through a watch subscription is not told when a
branch's upstream or a remote changes: `git branch -u`, `git branch
--unset-upstream`, `git remote set-url` and `git config branch.<name>.merge`
change only the repository's config, which the watcher read only to learn the
ref storage format. An application that shows a branch's upstream or its
ahead and behind counts therefore keeps showing the old tracking until another
ref changes. Likewise, `git worktree add`, `move`, `remove` and `prune` change
only the repository's `worktrees` admin dirs, which nothing watched, so an
application listing a repository's worktrees and the branches checked out in
them never learned of a worktree added or removed in a terminal.

## What changes

- A change of a followed repository's common dir `config` is a refs event for
  every followed work tree sharing it, scoped and coalesced as refs events are;
  the ref storage format is still read again.
- A change of a work tree's own `config.worktree`, used with
  `extensions.worktreeConfig`, is a refs event for that work tree.
- Every config key counts: the watcher does not compare configs to tell which
  keys changed.
- A worktree added, moved, removed or pruned, seen as a change of the common
  dir's `worktrees`, of an admin dir in it or of an admin dir's `gitdir`, is a
  refs event for every followed work tree sharing the common dir. The watcher
  watches `worktrees` and each admin dir in it without recursion while refs
  are followed; files a worktree's own commands write there are no event.

## Capabilities

### New capabilities

None.

### Modified capabilities

- `service-clients`: refs events for branch tracking and remote config, and
  for the set of linked worktrees.

## Impact

- Core: the ref watches' classification of a batch. Config needs no new
  watch; the linked worktrees cost one watch for `worktrees` and one per admin
  dir in it on backends that watch directory by directory.
- Consumers that subscribe with refs receive an event for any config change of
  the repository, as rare as config changes are, and read the refs and the
  tracking they show again. No protocol, C or Node interface changes.
