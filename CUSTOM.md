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
  description as its body, so the description says what changed and why. The
  title is a Conventional Commit, which versions the next release
  ([Merge request titles](#merge-request-titles)). Do not squash or rewrite the
  branch by hand first. A merged branch is not reused.
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
- A merge request from a fork outside the group stays out of the queue, since
  GitLab runs its pipeline in this project only when a Maintainer starts it. The
  Maintainer holds the queue by unassigning the queued merge requests, runs the
  pipeline on the merge request rebased onto the current `main`, merges it
  once it passes, and assigns the held ones again once the merge has landed.
- The upstream update's merge request, from `git-intake/update`, stays out of
  the queue too and lands **without squashing and without rebasing**: its merge
  commit carries upstream's history and the sync record git intake reads. git
  intake opens it with squashing off; leave it unticked. `git intake land`, with
  `GITLAB_TOKEN` holding a Maintainer's token with the `api` scope, brings it to
  `main`: it waits for its pipeline, and when `main` moved meanwhile it pauses
  the merge queue `.git-intake.toml` names, catches the update up and waits for
  the new pipeline; then it merges it and returns the paused merge requests in
  their order. `git intake land --resume` returns them after an interruption.

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

`.git-intake.toml` sets `versioning = "semver"`, which git intake before 0.5.2
refuses as an unknown key, and the contract check needs 0.5.3: run git intake
0.5.3 or later in a clone.

## Releases

A release is the pipeline of a protected tag `v<x.y.z>` on a commit of `main`.
git intake versions it under `semver`, as git-intake's README describes under
"Semver releases": the next version follows the previous release by the largest
change among the commits since it, the fork's own and upstream's. A breaking
change, `!` before the colon or a `BREAKING CHANGE:` footer, raises the major
version, `feat` the minor, `fix`, `perf` and `revert` the patch, and other
types ask for no release; an upstream commit whose subject is no Conventional
Commit counts as a patch, and an update to an upstream release outside the
caret range of the one the previous release carries raises the major. The first
semver release is `v1.0.0`; the dated releases
`v<base>-sakiori.<YYYYMMDD>.<n>` before it stay releases. A release's tag
records the upstream release it carries, its notes name it, and `fff-mcp
--version` reports it as build metadata, `1.3.0+fff-0.11.0`.

The schedule's `release-tag` job cuts the tag when the commits since the
previous release ask for a release, and otherwise passes without one; nobody
picks a version or pushes a release tag by hand. Running the schedule by hand
cuts an off-cycle release once `main` has such a commit. Nor does a schedule
cut one while the group's compute minutes cannot run `publish-npm`: its
`compute-quota` job fails and says how many remain, and once minutes are bought
or the monthly quota resets, running the schedule by hand cuts the release. Rollback is forward: a
revert on `main` and an off-cycle release. A pipeline started by hand on `main`
is a rehearsal at the version `<x.y.z>-rehearsal.<YYYYMMDD>` that publishes
nothing; run one after changing the build.

A failed release is finished by retrying its failed jobs, never cut again.
Retry only the failed npm job: npm never replaces a version, so packages
published from builds that then run again cannot be completed. If that
happens, leave the version and cut an off-cycle release once `main` moves on.
A failure the tagged commit itself causes cannot be retried: fix it on `main`;
the next tag is the release and the failed tag stays. A dated release whose
pipeline failed is not finished once the first semver release is published:
its scripts publish to npm's `latest` unconditionally and would move it back.

### Merge request titles

A downstream merge request lands squashed with its title as the subject, so the
title decides the change it asks for. It is a Conventional Commit,
`<type>[(<scope>)][!]: <description>`, of the types of commitlint's
conventional configuration: `build`, `chore`, `ci`, `docs`, `feat`, `fix`,
`perf`, `refactor`, `revert`, `style` and `test`. A breaking change takes `!`
before the colon or a `BREAKING CHANGE: <what breaks>` footer at the end of the
description, which becomes the commit's body. GitLab's _Revert_ button titles a
merge request `Revert "…"`: retitle it `revert: …`. The configuration's other
rules hold too, among them `subject-case`, which refuses a description in
sentence case, start case, Pascal case or upper case (`fix: Update …`), and
`header-max-length`, which limits the title to 100 characters.

The `mr-title` job checks the title in every merge request that lands squashed;
`make mr-title-check TITLE='…'` runs the same check locally. A failure names the
rules the title breaks in commitlint's lines above it. Both checks read the
title without GitLab's draft prefixes, `Draft:`, `[Draft]` and `(Draft)`, which
GitLab drops when the merge request is marked ready. A pipeline checks the title
and description it started with, and marking a merge request ready starts no
pipeline, while the queue merges one already on top of `main` on its last
pipeline: once the title and description are final, run the merge request's
pipeline before it is queued. git intake's update
merge requests land without squashing and are not checked: git intake reads
their upstream commits, and a subject that is no Conventional Commit counts as
a patch.

### npm versions

`@sakiori/fff-node` and its platform packages are published at the release's
version; the platform packages take it at pack time, and the Node package pins
them exactly. npm drops build metadata, so the packages carry no upstream
release. A version above the one npm's `latest` names is published to `latest`,
a lower one, as when an earlier failed release is finished after a later one,
to `superseded`.

The package's contract is its public API, which API Extractor reports into the
committed `packages/fff-node/etc/fff-node.api.md`, and the manifest's `exports`,
`engines`, `os` and `cpu`. An unexported type the API reaches fails the report.
After changing the package's public types, run `make node-api` and commit the
report; `make node-api-check` fails while it is out of date. API Extractor and
TypeScript are pinned exactly, since they write the report: an update of either
lands in a merge request of its own, whose report diff is reviewed, since a
report they format differently reads as removed declarations.

The contract check guards the version against the contract. It builds the
commit the merge request lands: for a squashed one, the head's tree with the
title and description on the merge base with `main`; for one that lands
unsquashed, the head. `git intake release version --json` on that commit gives
its previous release and the change of the next one, and the check compares the
commit's contract with the previous release's. A declaration of the previous
report that the new report lacks, removed or changed, needs a major change; any
other difference, an added declaration or a changed manifest field, needs at
least a minor one. Members count together with the declaration that contains
them, so a member moved to another interface counts as removed. Whether a
change of `engines`, `os` or `cpu` breaks consumers is the author's call: mark
it breaking when it does. A previous release without a report compares nothing.

The check names the change it needs. To fix a failure:

- In a squashed merge request, retitle it `feat: …` for an addition, or mark the
  breaking change with `!` in the title or a `BREAKING CHANGE:` footer in the
  description, and run the pipeline again.
- In an upstream update, check out its branch `git-intake/update`, run `make
  node-api` when the report is out of date, and push a commit on top of the
  update's merges that marks the change, such as `feat(node): …` or `feat(node)!:
  …`. git intake keeps such a commit through catch-ups.
- In another merge request that lands unsquashed, push a commit that marks the
  change, or tick _Squash commits_ and mark it in the title or description.

A merge request's pipeline compares with the release before it; when the
schedule cuts a release between that pipeline and the merge, the merge lands
against an older comparison. So the schedule's `release-tag` job runs the check
on the tip of `main`, as for an unsquashed merge request, before it cuts a tag,
and cuts none when it fails: land a merge request that marks the change it
names, then run the schedule by hand. The tag pipeline does not run the check,
since a release tag is never moved or deleted.

`make node-contract-check` runs the check on `HEAD`, as for an unsquashed merge
request; set `CI_MERGE_REQUEST_SQUASH_ON_MERGE=true`, `CI_MERGE_REQUEST_TITLE`
and `CI_MERGE_REQUEST_DESCRIPTION` to check a squashed one. It needs git intake
on the `PATH`, the full history, `origin/main` and the release tags (`git fetch
origin '+refs/tags/v*:refs/tags/v*'`).

The `node-contract` job runs in every merge request: `make sync-js-api-check`,
which checks the packages' copies of the shared TypeScript API, `make
node-api-check`, the tests of the release scripts (`make test-ci-scripts`) and
the contract check.

### New npm packages

npm accepts a trusted publisher only for a package that exists, so the first
version of a new package is published with a token:

1. On npmjs.com, create a granular token with _Bypass two-factor
   authentication_ checked, _Read and write_ access to the `@sakiori` scope
   (publish and stage, not stage only), expiring within days.
2. Store it as `NPM_TOKEN`: protected, masked and hidden, environment `npm`.
3. Release: when the commits since the last release ask for none, land a
   change that does first, then run the schedule by hand.
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
- Tags `v*` protected, created by Maintainers; a release tag is never moved or
  deleted.
- Package registry visible to everyone; pipeline variables allowed to no one;
  the group refuses duplicate generic packages.
- `GITLAB_TOKEN`: the group release bot's personal access token (`api`,
  `write_repository`), protected, masked and hidden, environment `git-intake`.
  Replace it before it expires.
- `COMPUTE_QUOTA_TOKEN`: a personal access token (`read_api`) of a dedicated
  account that is an Owner of the group, because GitLab shows compute usage to
  Owners only; protected, masked and hidden, environment `compute-quota`.
  Replace it before it expires.
- One daily pipeline schedule on `main`, owned by the release bot.
- Project members are Maintainers or above: npm's trusted publisher binds the
  project and the environment `npm`, not a protected ref.
- Instance runners stay on: the npm job runs on GitLab.com's shared runners,
  the only ones trusted publishing accepts, and is the only job that selects
  them. The group keeps them off for its other projects, so the group's compute
  quota is spent on npm publishing alone.

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
