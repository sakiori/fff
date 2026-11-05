## ADDED Requirements

### Requirement: HEADs of worktrees the index does not follow
A subscription that asks for refs SHALL receive a refs event when the HEAD of any
linked worktree of its repository moves: naming that worktree where the
subscription's directory is related to it and the index tells of it, and otherwise
a followed work tree of the repository related to its directory, as for other ref
state the worktrees share. What other subscriptions follow SHALL NOT change what it
hears. The other files of the admin dir of a worktree it does not follow SHALL
cause no event.

#### Scenario: A switch in another worktree
- **WHEN** a program subscribed with refs in a linked worktree, and another linked worktree no refs subscription's directory is related to switches to an existing branch
- **THEN** the subscription receives a refs event naming its own work tree, and no repository event

#### Scenario: A commit on another worktree's detached HEAD
- **WHEN** a program subscribed with refs, and a linked worktree the index does not follow commits on a detached HEAD
- **THEN** the subscription receives a refs event naming its own work tree

#### Scenario: Busy files
- **WHEN** a program subscribed with refs, and the index, logs or a `.lock` file of a linked worktree the index does not follow change, as `git add` or `git status` write them
- **THEN** the subscription receives no refs event

#### Scenario: A worktree the subscription covers
- **WHEN** a program subscribed with refs to a directory related to a linked worktree the index follows, and its HEAD moves
- **THEN** the subscription receives a refs event naming that worktree alone, and a repository event for it where asked for

#### Scenario: A narrow subscription beside one that covers the worktree
- **WHEN** a program subscribed with refs to a directory below a main checkout that holds no worktree, another subscribed to the main checkout, and a worktree open below the checkout switches branches
- **THEN** the first subscription receives a refs event naming the main checkout, as it does without the other, and the other one naming the worktree

#### Scenario: A view of a main checkout
- **WHEN** a program subscribed with repository state and refs to a main checkout, and a linked worktree switches branches that lies ignored below the checkout, beside it or open below it, whether or not another subscription holds an index of that worktree
- **THEN** the subscription receives a refs event naming the main checkout, or for a worktree open below it that worktree

#### Scenario: A worktree the index ignores
- **WHEN** a program subscribed with refs in a main checkout, and a linked worktree in a directory the checkout ignores switches branches, also after a change of its index made the watcher aware of it
- **THEN** the subscription receives a refs event naming the main checkout

#### Scenario: A worktree added later
- **WHEN** a program subscribed with refs, and a linked worktree is added after the subscription began, then switches branches
- **THEN** the subscription receives a refs event for each switch, also when the switch came before the watcher saw the worktree, and when a worktree of that name was removed just before it was added, also as the last one, which takes `worktrees` along

#### Scenario: Reftable
- **WHEN** a program subscribed with refs in a reftable repository, and a linked worktree the index does not follow switches branches, also one whose tables appeared after the watcher saw its admin dir
- **THEN** the subscription receives a refs event naming its own work tree

#### Scenario: The main checkout's HEAD stays its own
- **WHEN** a program subscribed with refs in a linked worktree of a repository with loose and packed refs, and the main checkout switches branches
- **THEN** the subscription receives no refs event for it
