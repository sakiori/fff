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
  one logical change. Merge it squashed, as the project ticks by default, so it
  lands on `main` as one commit, with the merge request's title as its subject
  and its description as its body, so the description says what changed and why.
  Do not squash or rewrite the branch by hand first. When `main` has moved on,
  use GitLab's _Rebase_. A merged branch is not reused.
- The upstream update's merge request, from `git-intake/update`, is merged
  **without squashing and without rebasing**: its merge commit carries
  upstream's history and the sync record git intake reads. git intake opens it
  with squashing off; leave it unticked. When `main` has moved on, the next
  `git intake sync` catches it up instead of a rebase.

Verify a change against the current `main` before merging it, and update
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
by hand, and running the schedule by hand cuts an off-cycle release. Rollback is
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

1. On npmjs.com, create a granular token with read and write access to the
   `@sakiori` scope, allowed to publish without a two-factor prompt, expiring
   within days.
2. Store it as `NPM_TOKEN`: protected, masked and hidden, environment `npm`.
3. Release, off-cycle if needed.
4. In each new package's settings, add a GitLab CI/CD trusted publisher:
   namespace `sakiori-oss`, project `fff`, CI file `.gitlab-ci.yml`,
   environment `npm`. Then require two-factor authentication and disallow
   tokens.
5. Delete `NPM_TOKEN` and revoke the token.

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
- Watch subscriptions report file changes and, on request, repository refs.
- `fff-mcp` makes no upstream update check.

The repository is MIT-licensed; keep upstream's `LICENSE` unchanged.
