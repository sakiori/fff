# Downstream maintenance

`sakiori-oss/fff` is a downstream fork of
[fff](https://github.com/dmtrKovalenko/fff) that follows upstream `main`. This
file holds the maintenance rules the repository does not show. The Node
package is documented in `packages/fff-node/README.md`.

## Merging into `main`

`main` changes only through merge requests and is never force-pushed or
rewritten. Merges fast-forward; _Squash commits_ is ticked by default.

- A downstream change is one merge request from a branch off `main` with one
  logical change. It lands as one squashed commit: the title is its subject
  ([Merge request titles](#merge-request-titles)), the description its body,
  saying what changed and why. Do not squash or rewrite the branch by hand; a
  merged branch is not reused.
- Verify the change against the current `main` before queuing it, and update
  [Downstream changes](#downstream-changes) when it adds or removes one.
- Only a Maintainer queues it, leaving _Squash commits_ ticked:
  `glab mr update <id> --ready --assignee sakiori-merge`. The bot rebases each
  queued merge request onto `main`, waits for its pipeline and merges it, one
  at a time.
- Never merge a merge request or set auto-merge on one yourself, and never
  rebase a queued one; the next two bullets are the only exceptions. One the
  bot cannot merge is assigned back with a comment that says why; after a
  conflict, rebase it onto `main`, the only rewrite allowed, and queue it
  again.
- A merge request from a fork outside the group stays out of the queue: a
  Maintainer unassigns the queued ones, runs its pipeline rebased onto `main`,
  merges it once it passes, then queues the held ones again.
- The upstream update (`git-intake/update`) stays out of the queue and lands
  **without squashing or rebasing**: its merge commit carries upstream's
  history and git intake's sync record, so leave _Squash commits_ unticked.
  Land it with `git intake land` (`GITLAB_TOKEN`: a Maintainer's token, `api`
  scope): it waits for the pipeline and, when `main` moved, pauses the queue
  while it catches the update up. `git intake land --resume` only ends a pause
  that an interrupted run left.

The native suite and `make test-node` run on macOS only after the merge, in
`main`'s pipeline; a failure there is fixed forward on `main`.

## Upstream updates

[git intake](https://gitlab.com/sakiori-oss/git-intake) 0.5.3 or later takes
upstream in, as `.git-intake.toml` configures. The daily `git-intake-sync` job
opens the update's merge request or catches it up with `main`. A conflict
fails the job; resolve it in a clone, against the intended behavior of both
sides:

```bash
git intake sync       # meets the conflict in an update worktree
git intake continue   # after resolving and staging it
git intake verify     # optional: the install and checks of .git-intake.toml
git intake finish     # pushes the update to its merge request
```

When upstream changes the Zig version it pins (workflows, `flake.nix`), change
`mise.toml` and the version and checksums in `.gitlab/scripts/install-tools.sh`
in the same update; the `ci-scripts` job checks `mise.toml` against upstream's
pin.

### Upstream changes to the index

The fork replaced how the index keeps itself current. Upstream code under
`crates/fff-core/src/` in `watcher/`, `walk/`, `scan.rs`, `shared.rs`,
`file_picker.rs` or `rescan_*.rs` no longer fits by text, and a clean merge
there proves nothing. For each upstream commit that touches them:

- Name the behavior it changes and carry it into the fork's model. Never add a
  second path beside the fork's, or bring back what the fork removed: a full
  rescan for lost events, a changed ignore file or a large batch, the rescan
  throttle that drops requests, and a `rescan` event sent when a rescan
  starts.
- Keep the invariants:
  - One loop per index (`index_loop.rs`), reached only through its
    `IndexWriter`, owns its scans, changed paths and rescans. Only the loop
    applies changes of the tree to the index, starts a scan, and replaces or
    removes the index; a message for another index or scan is dropped.
  - Every change, also one made during a scan, becomes an entry of one
    changed-path set (`intake.rs`, `changed_paths.rs`) and is checked again
    where it happened (`reconcile.rs`). A full rescan follows only a program's
    request, a change the index could not take in, or a recovery from a panic
    or a lost watcher; requests join the one pending rescan.
  - A full rescan sends subscribers one `rescan` event, once the rebuilt index
    is in place; a first scan sends none.
  - Only the watcher's own thread starts, changes and stops its streams. A
    scan that installs a watcher walks once the watcher is installed, and a
    stopped watcher stays stopped for its index.
  - Watch callbacks and repository and ref listeners run on one thread of
    their own, never on the loop, a walk or the watcher's thread.
  - The ref watches' state is a leaf lock: nothing else, the picker's lock
    included, is locked while it is held. An index, a watcher or a listener is
    dropped only outside every lock.
- When upstream fixes what the fork fixed another way, keep the fork's fix and
  take upstream's tests where they apply.
- When a behavior cannot be carried without a design change, stop and report
  it; do not land the update.
- Before landing: `git intake verify`; `make test-rust` and `make test-mcp`
  on macOS without `FFF_TEST_POLL_WATCHER_MS`, so FSEvents runs (the merge
  request's `native-tests` job covers Linux); and an independent review of the
  update's diff in these paths.

## Releases

A release is the pipeline of a protected tag `v<x.y.z>` on `main`, versioned by
git intake under `semver` (git-intake's README, "Semver releases") from the
largest change since the previous release, the fork's and upstream's: `!` or a
`BREAKING CHANGE:` footer raises the major, `feat` the minor, `fix`, `perf` and
`revert` the patch; other types ask for none. An upstream subject that is no
Conventional Commit counts as a patch; an upstream release outside the caret
range of the previous one raises the major. The dated releases
`v<base>-sakiori.<YYYYMMDD>.<n>` before `v1.0.0` stay releases. The tag and
notes name the upstream release it carries, and `fff-mcp --version` shows it
as build metadata: `1.3.0+fff-0.11.0`.

- The schedule's `release-tag` job cuts the tag when the commits ask for a
  release; nobody picks a version or pushes a release tag. Running the schedule
  by hand cuts an off-cycle release.
- No release is cut while the group's compute minutes cannot run
  `publish-npm`: the `compute-quota` job fails and says how many remain; run
  the schedule by hand once there are enough.
- Rollback is forward: a revert on `main` and an off-cycle release.
- A pipeline started by hand on `main` is a rehearsal,
  `<x.y.z>-rehearsal.<YYYYMMDD>`, that publishes nothing; run one after
  changing the build.
- A failed release is finished by retrying its failed jobs, never cut again.
  When the npm job failed, retry only that job: npm never replaces a version,
  so a release whose builds ran again after some packages were published
  cannot be completed. Then leave the version and cut an off-cycle release
  once `main` moves on. A failure the tagged commit causes is fixed on `main`:
  the next tag is the release, and the failed tag stays.
- A failed dated release is not finished once a semver release is published:
  its scripts would move npm's `latest` back.

### Merge request titles

A downstream merge request's title is a Conventional Commit,
`<type>[(<scope>)][!]: <description>`, by commitlint's conventional
configuration: types `build`, `chore`, `ci`, `docs`, `feat`, `fix`, `perf`,
`refactor`, `revert`, `style`, `test`; a breaking change takes `!` or a
`BREAKING CHANGE: <what breaks>` footer at the end of the description; the
description does not start with a capital (`fix: Update …` fails); at most
100 characters. Retitle GitLab's `Revert "…"` as `revert: …`.

The `mr-title` job checks every squashed merge request, ignoring draft
prefixes; `make mr-title-check TITLE='…'` runs it locally. A pipeline checks
the title and description it started with, and marking ready starts none: run
the pipeline again once both are final, before queuing.

### npm versions

`@sakiori/fff-node` and its platform packages are published at the release's
version, the Node package pinning the platform packages exactly. A version
above npm's `latest` goes to `latest`, a lower one to `superseded`.

The package's contract is its public API, reported by API Extractor into
`packages/fff-node/etc/fff-node.api.md`, plus the manifest's `exports`,
`engines`, `os` and `cpu`. After changing public types, run `make node-api` and
commit the report; `make node-api-check` fails while it is stale. API Extractor
and TypeScript are pinned exactly; update either in its own merge request and
review the report diff.

The contract check compares the commit a merge request lands with the
previous release (`git intake release version --json`). A removed or changed
declaration needs a major change; any other difference, a changed manifest
field included, at least a minor one. A member moved to another declaration
counts as removed. Whether an `engines`, `os` or `cpu` change breaks consumers
is the author's call: mark it breaking when it does. To fix a failure, mark
the change it names:

- squashed merge request: retitle it (`feat: …`, or mark it breaking) and run
  the pipeline again;
- upstream update: on `git-intake/update`, run `make node-api` if stale and
  push a commit on top that marks it, `feat(node): …` or `feat(node)!: …`; git
  intake keeps it through catch-ups;
- other unsquashed merge request: push such a commit, or tick _Squash commits_
  and mark it in the title or description.

`release-tag` runs the check on `main`'s tip and cuts no tag when it fails:
land a merge request that marks the change, then run the schedule by hand.

`make node-contract-check` checks `HEAD` as unsquashed; set
`CI_MERGE_REQUEST_SQUASH_ON_MERGE=true`, `CI_MERGE_REQUEST_TITLE` and
`CI_MERGE_REQUEST_DESCRIPTION` for a squashed one. It needs git intake on the
`PATH`, the full history, `origin/main` and the release tags
(`git fetch origin '+refs/tags/v*:refs/tags/v*'`). The `node-contract` job
runs it in every merge request, with `make sync-js-api-check`,
`make node-api-check` and `make test-ci-scripts`.

### New npm packages

npm takes a trusted publisher only for an existing package, so a new package's
first version is published with a token:

1. On npmjs.com, create a granular token: _Bypass two-factor authentication_
   checked, _Read and write_ on the `@sakiori` scope (publish and stage),
   expiring within days.
2. Store it as `NPM_TOKEN`: protected, masked and hidden, environment `npm`.
3. Release; when no commit since the last release asks for one, land one that
   does, then run the schedule by hand.
4. In each new package's settings, add a GitLab CI/CD trusted publisher:
   namespace `sakiori-oss`, project `fff`, CI file `.gitlab-ci.yml`,
   environment `npm`, _Allowed actions_ `npm publish`; under _Publishing
   access_, _Require two-factor authentication and disallow bypass 2fa tokens_.
5. Release again within two days, or the unused trusted publisher expires and
   has to be added again. `npm view @sakiori/fff-node@<version> _npmUser` then
   names the publisher, not the token's account.
6. Delete `NPM_TOKEN` and revoke the token.

## Project settings

- _Fast-forward merge_; _Squash commits when merging_ _Encourage_ (git intake
  refuses _Require_); squash template `%{title}`, blank line, `%{description}`;
  _Pipelines must succeed_ on; _Skipped pipelines are considered successful_,
  automatic rebase and merge trains off.
- `main` protected: push No one, merge Maintainers. Nothing protects
  `git-intake/update`; merge request pipelines get no protected variables or
  runners.
- Tags `v*` protected, created by Maintainers, never moved or deleted.
- Package registry visible to everyone; pipeline variables allowed to no one;
  the group refuses duplicate generic packages.
- No CI/CD variable named `LINUX_RUNNER` or `GIT_STRATEGY` in the project or
  group: either would override the workflow rules' choice of runner or Git
  strategy.
- `GITLAB_TOKEN`: the group release bot's personal access token (`api`,
  `write_repository`), environment `git-intake`. `COMPUTE_QUOTA_TOKEN`: a
  `read_api` token of a dedicated group Owner, since only Owners see compute
  usage, environment `compute-quota`. Both protected, masked and hidden;
  replace them before they expire.
- One daily pipeline schedule on `main`, owned by the release bot.
- Project members are Maintainers or above: npm's trusted publisher binds the
  project and environment `npm`, not a protected ref.
- Instance runners stay on for the npm job alone, as trusted publishing
  accepts only GitLab.com's shared runners; the group keeps them off for its
  other projects, so its compute quota goes to npm publishing.

## Runners

All Linux runners are unprivileged.

- `sakiori-oss-linux-protected`: protected refs only (release tags and `main`,
  the schedule's included), a fresh clone per job.
- `sakiori-oss-queue`: the merge queue's pipelines, on slots of their own;
  unprotected.
- `sakiori-oss-linux`: every other pipeline, forks' and upstream updates'
  included; it holds no credentials.
- `sakiori-oss-macos`: the darwin builds and macOS tests, for pushes and
  hand-started pipelines on `main` and release tags only; set up as _The macOS
  runner_ in git-intake's `RELEASING.md` describes, except for its two-hour
  maximum job timeout, which `macos-tests` needs.

A merge request from a fork outside the group finds no runner: a member with
at least the Developer role reads the change and runs its pipeline here.

## Downstream changes

- A shared index service: the MCP clients of one user that search the same
  directory share one index (`fff-mcp --help` lists the options). Programs use
  it through the `fff-service` library, the C library's
  `fff_create_service_instance` and `@sakiori/fff-node`.
- Watch subscriptions report file changes and, on request, repository refs,
  the HEADs of linked worktrees included.
- `fff-mcp` makes no upstream update check.
- The index follows directories rewalked or replaced while it watches them,
  and reports rewalked directories exactly.
- One writer per index applies its changes in order. Watcher events, reported
  changes, lost events and changed ignore rules are checked again where they
  happened, by Git's stat data, instead of rescanning the index. A change
  below a directory the index never saw walks that directory whole.
- A full rescan waits for the running scan and, when the watcher asks for it,
  a fixed interval after the last full scan; it sends one `rescan` event once
  the rebuilt index is in place. A first scan sends none.
- A lost watcher is replaced by a rebuild, at once or one interval after the
  last such rebuild started; until the rebuild is in place it reads as
  starting and its subscriptions stay open. A stopped watcher stays stopped
  for its index.

The repository is MIT-licensed; keep upstream's `LICENSE` unchanged.
