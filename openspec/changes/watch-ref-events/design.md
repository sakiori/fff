## Context

The core watches a repository's git dir and its `logs` without recursion and
reports the work tree when its status may have changed. Refs live in the
repository's common dir, which for a linked worktree is the main repository's
git dir, often outside the index: below `refs/`, in nested directories per
namespace and remote, and in `packed-refs`, or, in a reftable repository, in
tables that `reftable/tables.list` names. HEAD lives in each work tree's own
git dir, or in its reftable tables. On backends that watch directory by
directory, as inotify on Linux, every directory under `refs/` costs one watch.
FSEvents on macOS watches one stream, which notify restarts from "now" on
every change of the watched paths. libgit2 cannot open reftable repositories.

## Decisions

### Watches only for what a listener follows

A ref listener names a scope, its subscription's directory. The index follows
the refs of its own repository and of the registered repositories whose work
tree lies inside a scope or contains it, and watches only theirs, only while a
listener is subscribed: the subscription adds the watches before it returns,
so the service can reply `ready` with them in place, and dropping the last one
removes them. Watching every repository of a broad index would cost one watch
per ref directory of each for a subscriber that asked for one; an index whose
consumers never ask behaves as before.

A watch the git state watches or the tree's recursive watch share, such as a
repository's git dir watched for its state, is neither added nor removed by
the ref watches, since a backend keeps one watch per path and removing it would
silence the git state too. Every path the ref watches add is recorded before
the backend is asked, so one whose recursive watch failed halfway is removed
with the rest.

### Ref storage by the common dir's config

The format is `extensions.refStorage` in the common dir's config, read with
libgit2's config reader, which reads a repository it cannot open. A reftable
repository is followed through `reftable/` of its common dir and of each linked
worktree's git dir, whose `tables.list` git replaces by a rename for every ref
update, HEAD included. Where libgit2 finds no repository for the index, its
own discovery, with git's environment, decides whether one is there, so its
ceilings, filesystem boundary and stop at a `.git` that names no git dir hold;
only a repository it refuses whose common dir stores reftable refs counts,
and only when git's ownership rule accepts it, as git 2.47 decides it, since
libgit2 checks ownership only after the format check that refused it and by a
looser rule. Repositories refused for anything else keep their refs to
themselves.

The common dir itself is watched without recursion, unless another watch
covers it, for `packed-refs` and to see `refs/` or `reftable/` appear: a
repository registered before its ref store exists, as during a clone, is
followed once it appears, and its work tree is announced then, since refs
written before the watch went unseen. A changed config makes the ref watches
read the format again. Events of such a common dir are only ref signals; they
never register its repository's git state.

### One event per work tree

A refs event names the work tree, as a repository event does, rather than the
ref that changed. `packed-refs` and reftable tables cannot be attributed to a
ref without reading and comparing them, and a consumer that shows branches
reads them anyway. Events of one work tree coalesce into one queued event. A
change in a common dir names every followed work tree sharing it; a HEAD names
only its own. A namespace directory's own modification is no ref change, as
it changes with every lock file git writes in it; its creation and removal are,
since a namespaced branch's directory may appear before its watch does.

### Two FSEvents streams

FSEvents restarts a stream from "now" whenever its paths change. A restart of
the stream holding the index's tree loses file changes, which only a rescan
recovers, a full scan of the index each time a refs listener comes or goes or
the first walk meets a git dir outside the index. The watcher therefore keeps
a second stream for paths outside the tree: the tree's stream is set up once
with the root, and every later change, which only ever concerns git dirs and
ref stores outside the tree, goes to the other one. Its restart can lose only
git state, so one after the first walk began is recovered before the change
that caused it returns: the own git status is read again where the own git dir
lies outside the tree, ref stores and logs that appeared are watched, and the
repositories whose git dir or common dir lies outside the tree are announced
as repository and refs events, which consumers answer by reading that state
again. The own git dir and the own
repository's outside ref watches join that stream at its setup, and ref
watches stay there once added, so refs listeners restart nothing; each change
still goes as one update without the paths the stream watches already. Other
backends keep one stream and drop ref watches with the last listener.

### Off the service's runtime

Subscribing and unsubscribing change the watcher's watches and take the
index's locks, so the service runs them on blocking threads, never on its async
runtime. A listener being removed may still be running, so when a subscription
moves to another index, the rescan that replaces what came before goes in with
a new epoch of its queue, and events of the old index's listeners are refused.
