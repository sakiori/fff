## ADDED Requirements

### Requirement: Ref events for watch subscriptions
A program that subscribes to its session's directory on the shared service SHALL
be able to ask for refs. Such a subscription SHALL receive a refs event naming
the work tree of a repository inside its directory, or containing it, when that
repository's refs change. Refs events SHALL coalesce per work tree like
repository events. Subscriptions that do not ask for refs SHALL NOT receive refs
events, and their other events SHALL be unchanged.

#### Scenario: Branches and tags
- **WHEN** a program subscribed with refs and a branch or tag of the repository is created, moved, renamed or deleted, also one in a namespace such as `team/topic`
- **THEN** the subscription receives a refs event naming the repository's work tree

#### Scenario: Packed refs
- **WHEN** a program subscribed with refs and `git pack-refs` packs the refs, or a branch that only `packed-refs` holds is deleted
- **THEN** the subscription receives a refs event naming the repository's work tree

#### Scenario: Fetch
- **WHEN** a program subscribed with refs and a fetch updates a remote-tracking ref
- **THEN** the subscription receives a refs event naming the repository's work tree

#### Scenario: Below the work tree
- **WHEN** a program subscribed with refs to a directory below the work tree and a branch is created
- **THEN** the subscription receives a refs event naming the work tree by its absolute path

#### Scenario: Other refs
- **WHEN** a program subscribed with refs and only `refs/stash`, `refs/notes`, `FETCH_HEAD`, `ORIG_HEAD`, a reflog or a `.lock` file changes
- **THEN** the subscription receives no refs event

#### Scenario: Not asked for
- **WHEN** a program subscribed without refs, with or without repository state, and refs change
- **THEN** the subscription receives no refs event and no file event for the refs

#### Scenario: Node watch with refs
- **WHEN** a Node program calls `watch` with `refs: true` and a branch is created
- **THEN** its callback receives an event of kind `refs` whose path is the work tree

### Requirement: HEAD changes are refs events
A subscription that asks for refs SHALL also receive a refs event naming a work
tree when its HEAD moves. A HEAD change SHALL remain a repository event for
subscriptions that ask for repository state.

#### Scenario: Switching branches
- **WHEN** a program subscribed with refs and `git switch` checks out an existing branch
- **THEN** the subscription receives a refs event naming the work tree

#### Scenario: Detached HEAD
- **WHEN** a program subscribed with refs and a commit is made on a detached HEAD
- **THEN** the subscription receives a refs event naming the work tree

### Requirement: Refs of linked worktrees
A linked worktree's branches, tags and remote-tracking refs SHALL be those of
its main repository's common dir, and its HEAD SHALL be its own.

#### Scenario: A branch of the main repository
- **WHEN** a program subscribed with refs in a linked worktree and a branch is created in its main repository
- **THEN** the subscription receives a refs event naming the linked worktree

#### Scenario: The linked worktree's HEAD
- **WHEN** a program subscribed with refs in a linked worktree and the linked worktree switches to another branch or detaches its HEAD
- **THEN** the subscription receives a refs event naming the linked worktree

#### Scenario: The main checkout's HEAD
- **WHEN** a program subscribed with refs in a linked worktree of a repository with loose and packed refs and the main checkout switches branches
- **THEN** the subscription receives no refs event for it

### Requirement: Reftable repositories
Refs events SHALL reach subscriptions for repositories that store their refs in
the reftable format as for those with loose and packed refs.

#### Scenario: Reftable refs and HEAD
- **WHEN** a program subscribed with refs in a reftable repository and a branch or tag is created, moved or deleted, its HEAD moves or a fetch updates a remote-tracking ref
- **THEN** the subscription receives a refs event naming the work tree

#### Scenario: Repositories git refuses
- **WHEN** a program subscribed with refs to a directory whose repository git's library refuses for any reason but the reftable format, or git refuses for its owner, or that lies in a linked worktree whose admin dir was pruned
- **THEN** the subscription receives no refs event for that repository or one around it

#### Scenario: Reftable linked worktree
- **WHEN** a program subscribed with refs in a linked worktree of a reftable repository and a branch is created in the main repository, or the linked worktree's HEAD moves
- **THEN** the subscription receives a refs event naming the linked worktree

### Requirement: Refs followed within the subscription's directory
An index SHALL follow the refs only of the repositories whose work tree lies
inside a refs subscription's directory or contains it, and only while such a
subscription exists. A repository found before its refs exist SHALL be
followed once they appear.

#### Scenario: Another repository of a broad index
- **WHEN** a program subscribed with refs to one repository of an index holding several, and a branch is created in another
- **THEN** the subscription receives no refs event and the index watches nothing of the other repository's refs

#### Scenario: Refs created after the repository
- **WHEN** a program subscribed with refs and a repository whose git dir has no refs yet, as during a clone, then gains them
- **THEN** the subscription receives a refs event naming that repository's work tree

### Requirement: Event stream restarts
Where the watcher restarts an event stream to change the watched paths, as
FSEvents on macOS does, changing them SHALL lose no change of a file of the
index, refs subscriptions coming and going SHALL NOT make the index rescan, and
a possibly lost change of git state SHALL be recovered: the git status read
again, the watches restored and the repositories announced.

#### Scenario: Refs of a main repository outside the index
- **WHEN** on macOS a program subscribes with refs in a linked worktree whose main repository lies outside the index, or the last such subscription ends
- **THEN** the index does not rescan and the subscription hears the main repository's branches

#### Scenario: Git dirs outside the index at startup
- **WHEN** on macOS an index's first walk meets repositories whose git dirs lie outside it
- **THEN** the index scans once

#### Scenario: A restarted stream of git dirs outside the index
- **WHEN** on macOS the watched git dirs outside the index change after its first walk began
- **THEN** subscriptions receive a repository event for each repository whose git dir or common dir lies outside the index, and a refs event where they asked for refs; the index's git status is read again where its own git dir lies outside it, and ref stores and logs that appeared meanwhile are watched
