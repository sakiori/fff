# Downstream maintenance

`sakiori-oss/fff` is a downstream fork of
[fff](https://github.com/dmtrKovalenko/fff) that follows upstream `main`. This
file holds the rules for maintaining it that the repository itself does not
show. What a change does and why is in its commit; requirements are in
`openspec/`, and the Node package is documented in `packages/fff-node/README.md`.

## Merging into `main`

`main` changes only through merge requests and is never force-pushed or
rewritten. The project merges by fast-forward, with _Squash commits_ ticked by
default.

- A downstream change is one merge request from a branch off `main`, carrying
  one logical change. It lands squashed, as the project ticks by default, as one
  commit on `main`, with the merge request's title as its subject and its
  description as its body, so the description says what changed and why. Do not
  squash or rewrite the branch by hand first. A merged branch is not reused.
- A downstream merge request lands through the merge queue: once it is ready, a
  Maintainer assigns it to the `sakiori-merge` service account
  (`glab mr update <id> --ready --assignee sakiori-merge`), keeping the
  project's squash default; only a Maintainer queues one, since the bot merges
  it as a Maintainer. The bot takes the queue one merge request at a time: it
  rebases each onto `main`, waits for its pipeline and merges it. Apart from the
  two kinds below, never merge a merge request yourself, set auto-merge on it,
  or rebase one that is queued: a merge outside the queue sends every queued
  merge request back to a new pipeline. When the bot cannot merge one, it
  assigns it back with a comment that says why; after a conflict, rebase it onto
  `main`, the only rewrite allowed, and assign it again.
- Two kinds stay out of the queue, and a Maintainer merges them with the queue
  held: unassign the queued merge requests first, and assign them again once the
  merge has landed.
  - A merge request from a fork outside the group, since GitLab runs its
    pipeline in this project only when a Maintainer starts it: the Maintainer
    runs it on the merge request rebased onto the current `main` and merges it
    once it passes.
  - The upstream update's merge request, from `git-intake/update`, merged
    **without squashing and without rebasing**: its merge commit carries
    upstream's history and the sync record git intake reads. git intake opens it
    with squashing off; leave it unticked. Hold the queue from the moment it is
    ready. If `main` has moved since its last pipeline, catch it up with
    `git intake sync` and `git intake finish` in a clone, as
    [Upstream updates](#upstream-updates) shows, instead of a rebase, and let
    that pipeline pass.

Verify a change against the current `main` before queuing it, and update
[Downstream changes](#downstream-changes) when it adds or removes one. The macOS
runner takes protected refs only, so the native suite and `make test-node` run
on macOS after the merge, in the pipeline of the push to `main`; a failure
there is fixed forward on `main`.

## Upstream updates

[git intake](https://gitlab.com/sakiori-oss/git-intake) takes upstream in,
configured in `.git-intake.toml`. The daily schedule's `git-intake-sync` job
opens the update's merge request, or catches an open one up with `main`. A
conflict fails the job; resolve it in a clone against the intended behavior of
both sides:

```bash
git intake sync       # meets the same conflict, in an update worktree
git intake continue   # after resolving and staging it
git intake verify     # optional: the install and checks of .git-intake.toml
git intake finish     # pushes the update to the merge request
```

`mise.toml` pins the Zig version that builds zlob, which must match the version
upstream pins in its workflows and `flake.nix`; the `ci-scripts` job checks it.
When an update changes upstream's pin, change `mise.toml` and the Zig version
and checksums in `.gitlab/scripts/install-tools.sh` in the same update.

## Releases

A release is the pipeline of a protected tag `v<base>-sakiori.<YYYYMMDD>.<n>` on
a commit of `main`; git-intake's README, under "Downstream releases", describes
the scheme. The schedule's `release-tag` job cuts the tag; nobody picks a version or pushes a release tag
by hand, and running the schedule by hand cuts an off-cycle release once `main`
has a commit the last release lacks; a tip that already carries a release gets
no new tag. Rollback is
forward: a revert on `main` and an off-cycle release. A pipeline started by hand
on `main` is a rehearsal that publishes nothing; run one after changing the
build.

A failed release is finished by retrying its failed jobs, never cut again.
Retry only the failed npm job: npm never replaces a version, so packages
published from builds that then run again cannot be completed. If that
happens, leave the version and cut an off-cycle release once `main` moves on.
A failure the tagged commit itself causes cannot be retried: fix it on `main`;
the next tag is the release and the failed tag stays.

### New npm packages

npm accepts a trusted publisher only for a package that exists, so the first
version of a new package is published with a token:

1. On npmjs.com, create a granular token with _Bypass two-factor
   authentication_ checked, _Read and write_ access to the `@sakiori` scope
   (publish and stage, not stage only), expiring within days.
2. Store it as `NPM_TOKEN`: protected, masked and hidden, environment `npm`.
3. Release: when the tip of `main` already carries a release, land a change
   first, then run the schedule by hand.
4. In each new package's settings, add a GitLab CI/CD trusted publisher:
   namespace `sakiori-oss`, project `fff`, CI file `.gitlab-ci.yml`,
   environment `npm`, _Allowed actions_ `npm publish`. Under _Publishing
   access_, choose _Require two-factor authentication and disallow bypass 2fa
   tokens_.
5. Release again within two days: a trusted publisher that no publish has used
   by then expires, and only a new one replaces it. `npm view
   @sakiori/fff-node@<version> _npmUser` names the trusted publisher, not the
   token's account.
6. Delete `NPM_TOKEN` and revoke the token.

## Project settings

- Merge method _Fast-forward merge_; _Squash commits when merging_
  _Encourage_, never _Require_, which git intake refuses; squash commit template
  `%{title}`, a blank line, `%{description}`; _Pipelines must succeed_ on, _Skipped pipelines are considered successful_ off, automatic
  rebase and merge trains off.
- `main` protected: push No one, merge Maintainers. No rule matches
  `git-intake/update`, and merge request pipelines get no protected variables
  or runners.
- Tags `v*-sakiori.*` protected, created by Maintainers; a release tag is never
  moved or deleted.
- Package registry visible to everyone; pipeline variables allowed to no one;
  the group refuses duplicate generic packages.
- `GITLAB_TOKEN`: the group release bot's personal access token (`api`,
  `write_repository`), protected, masked and hidden, environment `git-intake`.
  Replace it before it expires.
- One daily pipeline schedule on `main`, owned by the release bot.
- Project members are Maintainers or above: npm's trusted publisher binds the
  project and the environment `npm`, not a protected ref.
- Instance runners stay on: the npm job runs on GitLab.com's shared runners,
  the only ones trusted publishing accepts.

## Runners

Jobs run on the group's Linux runner (`sakiori-oss-linux`), which also runs the
update's merge request pipelines with upstream's code, so it is unprivileged
and holds no credentials. The darwin builds and the macOS tests run on the
forks' macOS runner (`sakiori-oss-macos`), set up as the _macOS runner_ section
of git-intake's `RELEASING.md` describes, except for its two-hour maximum job
timeout, which `macos-tests` needs; its executor runs this project's jobs only for pushes and hand-started pipelines on `main` and for release tags. A
merge request from a fork outside the group finds no runner; a Maintainer runs
its pipeline here.

## Downstream changes

- A shared index service: the MCP clients of one user that search the same
  directory share one index (`fff-mcp --help` lists the service options).
- Programs search through that service with the `fff-service` library, the C
  library's `fff_create_service_instance` and `@sakiori/fff-node`.
- Watch subscriptions report file changes and, on request, repository refs, among
  them the HEADs of the repository's linked worktrees.
- `fff-mcp` makes no upstream update check.
- The index follows directories that are rewalked or replaced while it watches
  them, and reports rewalked directories exactly.

The repository is MIT-licensed; keep upstream's `LICENSE` unchanged.
