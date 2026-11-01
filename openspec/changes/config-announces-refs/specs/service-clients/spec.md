## ADDED Requirements

### Requirement: Branch tracking config is ref state
A subscription that asks for refs SHALL receive a refs event naming a work
tree when the config holding its branches' upstreams and remotes changes: its
repository's shared config, for every work tree sharing it, or the work tree's
own `config.worktree`. The events of one git command SHALL be coalesced
within the watcher's debounce window: one event per work tree in practice,
never zero.

#### Scenario: Upstream set or removed
- **WHEN** a program subscribed with refs and `git branch -u` or `git branch --unset-upstream` changes a branch's upstream
- **THEN** the subscription receives a refs event naming the work tree, at most one per write of the config, as these commands write it twice

#### Scenario: Remote URL
- **WHEN** a program subscribed with refs and `git remote set-url` changes a remote
- **THEN** the subscription receives exactly one refs event naming the work tree

#### Scenario: Linked worktree
- **WHEN** a program subscribed with refs in a linked worktree and its main repository's config changes, or its own `config.worktree` does
- **THEN** the subscription receives a refs event naming the linked worktree

#### Scenario: Another config key
- **WHEN** a program subscribed with refs and `git config` changes a key unrelated to branch tracking
- **THEN** the subscription receives exactly one refs event naming the work tree

### Requirement: The set of linked worktrees is ref state
A subscription that asks for refs SHALL receive a refs event naming each
followed work tree of a repository when one of its linked worktrees is added,
moved, repaired, locked, unlocked, removed or pruned. Commands run inside a
linked worktree that change none of these SHALL produce neither such an event
nor a repository event for another work tree. Events of one command SHALL be
coalesced within the watcher's debounce window.

#### Scenario: Worktree added
- **WHEN** a program subscribed with refs to a main checkout and `git worktree add` adds a linked worktree, below the checkout or beside it
- **THEN** the subscription receives a refs event naming the main checkout, at most one per write of the worktree's admin dir

#### Scenario: Worktree moved or repaired
- **WHEN** a program subscribed with refs to a main checkout and `git worktree move` moves a linked worktree, or `git worktree repair` rewrites its location, also unchanged
- **THEN** the subscription receives a refs event naming the main checkout

#### Scenario: Worktree locked or unlocked
- **WHEN** a program subscribed with refs to a main checkout and `git worktree lock` or `unlock` changes a linked worktree's lock
- **THEN** the subscription receives a refs event naming the main checkout

#### Scenario: Worktree removed or pruned
- **WHEN** a program subscribed with refs to a main checkout and `git worktree remove` removes a linked worktree, or `git worktree prune` prunes one whose directory went
- **THEN** the subscription receives a refs event naming the main checkout

#### Scenario: Work inside another worktree
- **WHEN** a program subscribed with refs and repository state to a main checkout and a command such as `git status`, a commit on a detached HEAD, or a merge, cherry-pick or revert stopped at a conflict and aborted runs in a linked worktree outside the subscription's directory
- **THEN** the subscription receives no refs event and no repository event for it

#### Scenario: Worktree directory deleted outside git
- **WHEN** a linked worktree's directory is deleted without git, so that git would call it prunable
- **THEN** nothing is announced from the main repository until `git worktree prune` runs; a subscription in that worktree ends as its directory went

#### Scenario: Worktrees added in an index of a linked worktree on macOS
- **WHEN** on macOS a program subscribed with refs and repository state in a linked worktree whose main repository lies outside the index, and worktrees are added from the main checkout
- **THEN** the subscription receives refs events naming its work tree and no repository event, as no watcher stream restarts
