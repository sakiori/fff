# Downstream maintenance

This document describes how downstream changes to fff are maintained and how
they differ from upstream. Update it whenever a feature changes downstream
behavior, configuration, compatibility, or maintenance requirements.

## Branches

The fork is the public project `sakiori-oss/fff` on GitLab.com,
<https://gitlab.com/sakiori-oss/fff>.

- `main` is the fork's branch and carries the downstream changes on top of
  upstream. It changes only through merge requests and is never force-pushed.
- Feature branches start from `main` and contain one logical change each.

Completed features are squash-merged into `main` through a merge request,
producing one commit per feature. The project merges by fast-forward; if `main`
moved on since the feature branch started, GitLab's rebase brings the branch up
to date before the merge. The commit describes the resulting behavior and why it
changed. Subsequent work starts from the updated `main`; a previously
squash-merged feature branch is not reused. Upstream updates are the exception:
they are merged without squash (see [Upstream updates](#upstream-updates)).

## Upstream updates

The upstream fff repository is not a remote of the fork. `.git-intake.toml`
describes it by URL and branch, and upstream updates are made with
[git-intake](https://gitlab.com/sakiori-oss/git-intake) in follow mode, which
merges upstream `main` into `main` and publishes the update as a merge request
from the branch `git-intake/update`. The configuration uses named checks and a
`[release]` section, which need git-intake 0.4.1 or later, locally as in CI;
the pipeline installs the release `GIT_INTAKE_VERSION` in `.gitlab-ci.yml` names.

A daily pipeline schedule on `main` runs `git intake sync` in the job
`git-intake-sync`. Without an open update merge request it merges the upstream
tip `main` lacks and opens the merge request; with one open it catches it up
with `main` by a catch-up merge when `main` moved on, and otherwise changes
nothing. The merge request's own pipeline runs the checks. A conflict fails the
job; then run `git intake sync` in a clone, which meets the same conflict in an
update worktree, resolve it against the intended behavior of both upstream and
downstream changes, stage the resolutions, run `git intake continue` and
`git intake finish`, which pushes the result to the merge request.

Merge the update's merge request by fast-forward with _Squash commits_
unticked, so that its merge commits land on `main`, keeping upstream ancestry
and the sync records that `status` and the next update read; git-intake refuses
to build on a squashed or rebased update. Do not use GitLab's rebase button
either: when `main` moved on, the next `git intake sync`, scheduled or run by
hand, catches the merge request up instead.

`git intake status` shows the last synced upstream commit and how far `main` is
behind, and `git intake log` lists the upstream commits still pending. An update
to a chosen revision is made by hand: `git intake start fff=<rev>` merges
upstream up to a commit of upstream `main` or an upstream tag (see _Choosing a
target_ in the git-intake README) in a separate update worktree,
`git intake verify` runs the install command and the checks listed in the
configuration, polling the watchers (see [Verification](#verification)), and
`git intake finish` opens the merge request into `main`.

`mise.toml` pins the Zig version that builds zlob. It is downstream-only and
must match the Zig version upstream pins in its workflow setup-zig steps and in
`flake.nix`; the `ci-scripts` job of merge request pipelines checks that. When
an update changes upstream's pin, change `mise.toml` in the same update,
together with the Zig version and the checksums of its release archives in
`.gitlab/scripts/install-tools.sh`, which refuses a Zig version other than the
one `mise.toml` pins.

## Feature delivery

Implement and verify each feature on its feature branch. Keep product
requirements and consequential design decisions alongside the affected code,
and update this document with the downstream behavior that is actually delivered.

Before integration, verify the feature against the current `main`, including
relevant upstream compatibility checks. Squash integration records the feature
as a single change; it does not replace functional verification.

Merge request pipelines run on the group's Linux runner: `cargo fmt`,
clippy with zlob, `make lint-mcp`, shellcheck over the pipeline's scripts and
the Zig pin check, the full test suite with native watchers (see
[Verification](#verification)) and `make test-node`. When a change touches the
build (manifests, build scripts, `.cargo/config.toml`, `.gitlab/scripts/`, the
pipeline, `mise.toml` or the toolchain file), they also build the Linux release
files. The macOS runner takes protected refs only, so the native suite and
`make test-node` run on macOS in the pipeline of every push to `main`, after
the merge; a failure there is fixed forward on `main`, and a release cannot
pass it.

## Releases

The fork releases `fff-mcp`, the downstream MCP executable, as archives on its
GitLab Releases, and the Node package `@sakiori/fff-node` with its platform
packages on npmjs.org. Releases are cut and checked with git-intake's release
commands; nothing but the pipeline of a release tag publishes.

A release is the protected tag `v<base>-sakiori.<YYYYMMDD>.<n>` on a commit of
`main`, its version the tag without the `v`. `<base>` is the version of the
newest upstream release (an upstream tag `v*`) that the commit contains, with
its patch number raised by one: `main` holds that release and later commits, so
the version sorts after the upstream release it contains and before the next
one. `<YYYYMMDD>` is the UTC day the tag is cut and `<n>` counts that day's
releases from 1. A release is published once its GitLab Release exists, which
the pipeline creates last.

- A daily pipeline schedule on `main` runs `git intake release tag` in the job
  `release-tag`: it tags the tip of `main` with the next release unless a
  release already points at it. When the release at the tip is not published,
  because its pipeline is running or failed, it creates nothing and fails. An
  off-cycle release is the schedule run by hand.
- The tag's pipeline starts with `git intake release check`, which refuses a
  tag not of release form, not on the commit, not on `main`'s downstream line,
  with another base, sorting before an earlier release, or already published;
  nothing is built before it passes. The same job writes the release notes
  with `git intake release notes`: the subjects of the fork's own commits and
  the upstream updates since the previous release.
- `git intake release version` gives the version every build stamps into
  `crates/fff-mcp/Cargo.toml` and `Cargo.lock` in its working tree only, so
  `fff-mcp --version` reports it. The builds use upstream's release profile
  `ci` and zlob. On the group's Linux runner cargo-zigbuild builds `fff-mcp`
  and `libfff_c` for `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl`, `fff-mcp` linked statically against musl and
  `libfff_c` dynamically, as Node.js on musl loads it, and for
  `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` against glibc
  2.28, which is also the oldest glibc Node.js's own Linux builds run on. The
  macOS runner builds both for `aarch64-apple-darwin` and
  `x86_64-apple-darwin`, for macOS 13 or later, and signs them ad hoc; Apple's
  SDK may be used only on Apple hardware, so the darwin targets are not
  cross-compiled. The same runner runs the native test suite and
  `make test-node`, and nothing is published before they pass.
- Every file of a release has one build identity, so the Node package of a
  release starts and joins the same service as the `fff-mcp` that mise installs
  from that release's archives. Every target builds `libfff_c` and `fff-mcp` in
  separate cargo invocations, so `libfff_c` has the same features on every
  target, and the build checks that both report the same identity. A platform
  package holds the same `fff-mcp` file as the target's archive, and the packing
  job checks that all targets report one identity.
- The packing job packs `@sakiori/fff-node` and the six platform packages
  `@sakiori/fff-bin-darwin-arm64`, `-darwin-x64`, `-linux-x64-gnu`,
  `-linux-arm64-gnu`, `-linux-x64-musl` and `-linux-arm64-musl` at the
  release's version. Each platform package declares `os`, `cpu` and on Linux
  `libc`, and the Node package lists them all as optional dependencies at
  exactly that version; the packing job fails otherwise.
- The signing job writes `SHA256SUMS` over the archives
  `fff-mcp-v<version>-<target>.tar.gz` of the musl and darwin targets, each
  holding the `fff-mcp` binary and `LICENSE` at its root with a `.sha256` beside
  it, and signs every archive and `SHA256SUMS` with cosign, keyless: the job's
  GitLab ID token obtains a short-lived Sigstore certificate naming this
  project's CI configuration and the tag, recorded in Sigstore's public
  transparency log, and each signature is a `.sigstore.json` bundle.
- The npm publish job publishes the platform packages, then the Node package,
  to npmjs.org with the dist-tag `latest` and provenance, by trusted publishing
  from GitLab.com's shared runners. The publish job then uploads the archives,
  their `.sha256` files and bundles, `SHA256SUMS` and its bundle to the generic
  package `fff-mcp`, version `v<version>`, and the release job creates the
  GitLab Release last, with the notes and a link to every file at
  `https://gitlab.com/sakiori-oss/fff/-/releases/v<version>/downloads/binaries/<file>`.
- A failed release is finished, not cut again: retry its failed jobs. A
  retried npm publish job skips packages published with the same tarball and
  fails on a package published with another one, since npm never replaces a
  version; retry only the failed publish job, not the builds or the packing
  before it. If npm packages were published and the builds or the packing ran
  again, their tarballs differ and the version can no longer be completed:
  leave it and release the next one with an off-cycle release, which
  `git intake release tag` cuts once `main` moved on. A retried publish job
  uploads only the files the package lacks and refuses one it holds with other
  content; a retried release job succeeds when the Release exists with exactly
  its links. Rollback is forward, by a revert and an off-cycle release.
- A pipeline started by hand on `main` ("Run pipeline", or
  `glab ci run --branch main`) is a rehearsal: the builds, the macOS tests and
  the packing of a release with the version `<base>-sakiori.<YYYYMMDD>.0`,
  which no release tag takes, and nothing signed or published. Its npm job only
  reads npmjs.org: it reports per package whether a release would publish it or
  skip it, and fails on a package published with another tarball. Rehearse
  after a change to the build, since merge request pipelines cannot build on
  the macOS runner.

Install a release of `fff-mcp` with mise's GitLab backend, which picks the
archive of the running platform by its target triple and needs no token:

```sh
mise use -g gitlab:sakiori-oss/fff@<version>
```

`<version>` is the tag without its `v`, for example
`0.11.1-sakiori.20301101.1`; `mise ls-remote gitlab:sakiori-oss/fff` lists
the releases. Configure MCP clients as described in
[Building and running](#building-and-running), with the `fff-mcp` that mise
installs.

Install the Node package from npmjs.org, pinning a release:

```sh
npm install --save-exact @sakiori/fff-node@<version>
pnpm add --save-exact @sakiori/fff-node@<version>
```

The install brings only the platform package of the running platform. A
project that imports `@ff-labs/fff-node` keeps its imports with an alias:
`npm install --save-exact @ff-labs/fff-node@npm:@sakiori/fff-node@<version>`.
`npm audit signatures` checks the packages' registry signatures and provenance
attestations.

Check an archive against its checksum and its signature, which must name this
project's pipeline for the release's tag:

```sh
tag=v<version>
file="fff-mcp-$tag-<target>.tar.gz"
base="https://gitlab.com/sakiori-oss/fff/-/releases/$tag/downloads/binaries"
curl --fail --location --remote-name "$base/$file" \
  --remote-name "$base/$file.sha256" --remote-name "$base/$file.sigstore.json"
sha256sum -c "$file.sha256"   # or: shasum -a 256 -c "$file.sha256"
cosign verify-blob --bundle "$file.sigstore.json" \
  --certificate-identity "https://gitlab.com/sakiori-oss/fff//.gitlab-ci.yml@refs/tags/$tag" \
  --certificate-oidc-issuer https://gitlab.com \
  "$file"
```

The double slash belongs to the identity: it separates the project from the
path of its CI configuration. `SHA256SUMS` and `SHA256SUMS.sigstore.json` are
checked the same way, and `sha256sum -c SHA256SUMS --ignore-missing` then
checks any archive of the release.

### Project settings

The release process relies on these settings, which the project's owners apply:

- The group `sakiori-oss`: under _Packages and registries_, duplicate generic
  packages not allowed, so no file of a published version is uploaded again.
  The group's release bot user, a Maintainer of the
  group with two-factor authentication and completed identity verification,
  without which GitLab.com runs none of its pipelines.
- The project is public, with its package registry visible to everyone, so
  release links download without a token. Auto DevOps is off.
- _Merge requests_: merge method _Fast-forward merge_; _Squash commits when
  merging_ set to _Encourage_ (`default_on`), never _Require_: an update merge
  request is merged with squash unticked, every other one squashed;
  _Pipelines must succeed_ on and _Skipped pipelines are considered
  successful_ off; automatic rebase before merge and merge trains off.
- `main` is protected: _Allowed to push and merge_ No one, _Allowed to merge_
  Maintainers, force push off. No protected-branch rule matches
  `git-intake/update`, so its merge request pipelines get no protected
  variable.
- Tags `v*-sakiori.*` are protected, _Allowed to create_ Maintainers; the bot
  user creates them, and a release tag is never moved or deleted.
- _CI/CD > Variables_: _Allow merge request pipelines to access protected
  variables and runners_ off, and _Minimum role to use pipeline variables_
  _No one allowed_, so no pipeline variable overrides what the release jobs and
  the macOS runner check. `GITLAB_TOKEN` holds a personal access token of the
  bot user with the scopes `api` and `write_repository` and an expiry of at most
  a year, replaced before it expires; it is protected, masked and hidden, and
  scoped to the environment `git-intake`, which only the schedule's jobs name.
  Its pushes, unlike the job token's, start pipelines.
- _Build > Pipeline schedules_: one daily schedule on `main`, owned by the bot
  user, which runs `git-intake-sync` and `release-tag`.
- Members of the project are Maintainers or above. npm's trusted publisher
  binds the project, the CI file and the environment `npm`, not a protected
  ref, so a member who could push a branch could publish from it.
- `NPM_TOKEN` exists only while a new npm package is bootstrapped (below).
- Instance runners on: the npm publish job runs on GitLab.com's shared
  runners. Group runners on for the rest (below).

### New npm packages

npm configures a trusted publisher only for a package that exists, so the
first version of every new package, such as a new platform package, is
published with a token:

1. On npmjs.com, create a granular access token with read and write access to
   the packages of the `@sakiori` scope, allowed to publish without a
   two-factor prompt, expiring within days.
2. Store it in the project as `NPM_TOKEN`: protected, masked and hidden, scoped
   to the environment `npm`, which only the npm publish job names. The job
   writes it to npm's user configuration only when it is set.
3. Release, off-cycle if needed. npm tries trusted publishing first and falls
   back to the token for packages without a trusted publisher; the job
   publishes with provenance either way.
4. In each new package's settings on npmjs.com, add a trusted publisher for
   GitLab CI/CD: namespace `sakiori-oss`, project `fff`, top-level CI file path
   `.gitlab-ci.yml`, environment `npm`. Then set the package to require
   two-factor authentication and disallow tokens.
5. Delete the `NPM_TOKEN` variable and revoke the token.

### Runners

Every job but the macOS jobs and the npm publish job runs on the group's Linux
runner, a group runner of `sakiori-oss` with the tag `sakiori-oss-linux` and
_Run untagged jobs_ off, in a Docker container of the image the job names. It
also runs the merge request pipelines of `git-intake/update`, which carry
upstream's code, so it is not privileged and holds no credentials of its own,
and a tag pipeline clones afresh instead of reusing the working copy those
jobs leave. A merge request from a fork outside the group finds no runner in
that fork; a Maintainer runs its pipeline in this project. The npm publish
job runs on GitLab.com's shared runners, the only ones npm's trusted
publishing accepts.

The darwin builds and the macOS tests run on the macOS runner of the
`sakiori-oss` forks, on Apple silicon. Each job runs in a fresh macOS virtual machine that
[Tart](https://tart.run/) clones from the image `MACOS_IMAGE` in
`.gitlab-ci.yml` names and deletes after the job, so nothing a job runs
persists or reaches a later job, and the virtual machine cannot reach the
host's services or local network. It is set up as the _macOS runner_ section
of git-intake's `RELEASING.md` describes (Tart, Softnet, Cirrus Labs'
gitlab-tart-executor behind a wrapper owned by root, the image pulled ahead),
with these settings:

- A project runner enabled in `sakiori-oss/fff` and the other forks whose
  releases build on macOS, then locked to them, with the tag
  `sakiori-oss-macos`, _Run untagged jobs_ off, _Protected_ on, and a maximum
  job timeout of two hours.
- Before any virtual machine starts, the executor's wrapper refuses every job
  of this project but one on a protected ref
  (`CI_COMMIT_REF_PROTECTED` is `true`) that is either the branch `main` in a
  pipeline from a push or started by hand (`CI_PIPELINE_SOURCE` `push`, `web`
  or `api`), or a release tag matching
  `^v[0-9]+\.[0-9]+\.[0-9]+-sakiori\.[0-9]{8}\.[1-9][0-9]?$`. It also refuses
  a job that sets a variable of gitlab-tart-executor's (`TART_EXECUTOR_*`),
  clones for this project only a Cirrus Labs base image pinned by digest, and
  runs one job at a time. Every other project's jobs it refuses or allows by
  that project's own rules.

While the runner is offline, the macOS jobs wait, and the release with them;
GitLab fails a job no runner picks up, which is retried once the runner is back.

## Downstream behavior

### Shared MCP service

Each MCP stdio process connects to a shared service for the current OS user.
The service owns file indexes, content caches and filesystem watchers. The stdio
processes retain only transport and parent-lifetime handling. The service
client, its protocol and the namespace checks live in the `fff-service` library
crate, so programs other than `fff-mcp` can reach the service the same way.

Compatible clients share an index for the same directory or covered descendants.
Canonical paths resolve aliases. A repository index can serve its subdirectories;
a broader non-repository scope can replace narrower coverage without changing
existing clients' search scopes. A client moves to broader coverage once that
index has finished scanning. This also happens when an existing index gains
coverage later, because its watcher indexed a new directory or a rescan
included one: the service consolidates on that change instead of waiting for
another client, and releases the index that is no longer used. When a client's
directory is removed, its tool calls report the scope as unavailable until the
directory is restored. Built-in directory exclusions apply at any depth with
both walker backends, so a parent index serves a descendant with the same files
a standalone index would contain.

An index is reused for whatever a request needs rather than for equal options.
Options that decide which files it holds must match: separate repository
contexts and different `--follow-symlinks` settings keep separate indexes.
Content indexing, watching, warmup and the content cache budget
(`--max-cached-files`) are capabilities an index may offer beyond the request: a
client without content indexing uses a content index, a `--no-watch` client
uses a watched index, and a client with a smaller cache budget uses an index
with a larger one. A budget left to the core caches between the core's smallest
and largest size-based budgets, so it serves a request up to the smallest one
and is served by an explicit budget of at least the largest. A client that
needs a capability the covering index lacks gets an index of its own, which
also offers every capability of the indexes at the same root that it can
replace, those no `--no-watch` client keeps; once it is ready, compatible clients move to it as they move to
broader coverage, and the index they leave is released. The frecency database
belongs to the index and is chosen by the client that creates it; a client that
names one only shares indexes that use the same one, and both clients send it
in one spelling, absolute with its directory resolved. Permission
to index the home directory or the filesystem root is checked for each client
and does not separate indexes. Clients using `--no-watch` keep their original
binding when broader coverage arrives.

Watched indexes track empty directories created or restored after the initial
scan, including nested directories. The core notifies subscribers whenever the
set of indexed directories changes. Compatible descendant clients can reuse
that coverage without waiting for a file to be created. Directory exclusions
still apply through the original root's traversal policy. An update rejected
because incremental capacity is exhausted requests a rescan; a request delayed
by rate limiting stays pending and runs when the cooldown ends, and any
admitted rescan satisfies it. A file that disappears before it is indexed does
not trigger a rescan. When the watcher walks a directory that is already
indexed, which happens after an event for the directory itself, subscribers
receive `Created` for files that were not indexed, `Modified` for indexed files
whose size or modification time changed, also to an older time, and nothing for
unchanged files; upstream reported every file of the walk as created and kept
the newest modification time it had seen. A walk leaves a file as it is when its
inode, size and modification time match the index, so the index holds what it
read from that very file; a file in a replaced directory is usually a new inode
and is read again. Filesystems such as ext4 and xfs reuse freed inode numbers
quickly, so a file regenerated within the same second with the same size can
keep the content the index read before until it changes again. Upstream read
every file of the walk again. On platforms without inode numbers every file of
the walk is read again. Files of a directory
created while a scan runs are indexed by that scan, so the walk that follows
does not report them again, as with the events the scan holds.

An indexed directory, the index's root included, that is reported as removed,
created or renamed and is a directory when the watcher applies the event may
have been replaced, as by `rm -r d && mkdir d` or `mv d d.bak && mv new d`. The
watcher walks it and replaces the entries below it in the same update as the
rest of the batch: a file present before and after the replacement stays
searchable throughout, files the new directory lacks leave the index, and a file
moved out of it in the same batch is reported as renamed. A file the batch's own
events name is reported as created or modified even when the walk finds it as
the index holds it, as when a scan read the directory after it was replaced. A replaced directory
that the ignore rules now exclude leaves the index, as it would after a scan.
A directory the walk cannot read is left out as a full scan leaves it out;
granting access to one again is an attribute change on it, which walks it again.
One removed while the walk runs gets no files, and its own entry leaves with its
removal event. A file replaced by a
directory, and a directory replaced by a file, leave the index with their
entries. Upstream emptied the subtree on a removal and indexed it again
afterwards, so searches in between found none of its files, and on a rename only
added the new directory's files, so the old ones stayed.

Changes made while an index scans are not lost. Every scan subscribes to
filesystem events before it walks the tree, holds the events that arrive during
the walk and applies them in order once the scan result is in place; a watched
index reports its watcher ready only after that. When more events are held than
one update batch accepts, they are discarded in favour of one recovery rescan
that rate limiting defers but never drops. At most one scan runs per index;
requests arriving meanwhile coalesce into one follow-up scan. Content edited
after the content index read a file stays searchable. These scan and watcher
changes are in the core crate and apply to every fff consumer, not only the
MCP service.

File, directory, mixed and glob searches take an `exclude_frecency` option in
`FuzzySearchOptions`, and grep and multi-grep take one in `GrepSearchOptions`.
With it, a search ranks as an index without a frecency database would, although
the index keeps the frecency scores other searches use: a query without fuzzy
text then lists its entries in path order, so pages stay stable, and grep
searches files by modification time alone. This is a core option, available to
every fff consumer.

Concurrent searches from different clients share one bounded search pool
without waiting for each other: a search that has finished its own work returns
even while another client's search is still running. Results, offsets and
pagination are the same as for a search running alone.

Scope boundaries apply before matching, ranking, counts and pagination in file,
directory, mixed, glob and content searches, including fallback searches.
Returned paths and query path constraints are relative to the client's
directory. Each client has independent cursors; cursors for different searches
coexist, and a changed index binding invalidates the old ones.

### Program sessions

Programs reach the shared service through the `fff-service` library. A program
session is opened like an MCP session, for a directory and index options, and
follows the same index reuse rules; service status marks each session as `mcp`,
`program` or `watch`. After the service accepts it, the connection carries one request
at a time as a big-endian 32-bit length followed by a MessagePack body, and the
service answers each with one frame. Requests are file, directory and mixed
fuzzy search, glob, grep and multi-grep, a readiness wait with a timeout of at
most an hour, whether the index is scanning, its scan progress, a rescan
coalesced with any running one, the git work tree containing the directory,
reported changes, and close. Reported changes name files the program changed,
absolute or relative to its directory; the service applies them to the
session's index through the core's watcher path, as `apply_reported_changes`
of `SharedFilePicker`, without a rescan. What a report guarantees:

- Each reported file that exists is in the index, added or updated, with its
  content indexed, when the reply arrives, unless the index's ignore rules
  exclude it; each that no longer exists is gone from it, with any indexed
  files below it. While the index scans, the changes wait with the scan's held
  events and are applied when it commits, after the reply.
- Report files, not directories. A reported new directory is walked only by a
  watched index, by its watcher and after the reply; an index without a
  watcher does not index it.
- A reported `.gitignore` or `.ignore` file requests a rescan, as the watcher's
  event does, and the new rules apply once it has run.
- Paths are normalised lexically, so `a/../b` names `b`, and then spelled as on
  disk: every component in its on-disk case, with a symlink kept as the link.
  Where the filesystem ignores case, a deleted path matches the indexed entry
  regardless of ASCII case only: a deleted file with a non-ASCII name, reported
  in another case, stays indexed until the watcher's own event or the next
  rescan removes it. Matching it as APFS does would need Unicode case folding
  and normalisation for a rare case the watcher already covers. A path that
  resolves outside the session's directory, also through a symlink, refuses the
  whole request (`OutsideScope`).
- An index that does not follow symlinks, which is the default, holds none, as
  its scan skips them: a reported symlink is not added, and a reported path that
  became a symlink is removed.

The readiness wait and the scanning state count a scan until its
post-scan phase ends and a requested rescan until it has run. Scan progress
counts the files of the session's directory once the index is ready, and the
files the shared index has scanned so far before. The scanning state and scan
progress never wait for searches or index updates: while those hold the index,
progress repeats the session's last count. Results are the core's:
paths relative to the session's directory, scores, totals and, for grep, line
numbers, line content, byte match ranges, the number of files searched, the
regex fallback error and the next cursor, which names the index that issued
it beside the core's file offset. Every search, grep included, can exclude
frecency. A grep with a cursor of another index than the session's current
one fails, also after the session grepped on the new index, and the program
searches again without it. Searches never wait for the initial scan, unlike
tool calls: they answer from what the index holds so far, nothing before the
walk is committed, as an in-process instance does. The
session ends when the program closes it or its connection ends, also when the
program exits without closing it or while the service works on its request,
which stops a grep in progress.

`fff_service::session::ProgramSession` is the blocking client: it connects to
the service of its own build in the configured namespace, or in the one
`FFF_MCP_SERVICE_DIR` names, or in the default namespace, and starts the
service from the `fff-mcp` executable it is given when none runs. Relative
paths of the directory, executable, log file and frecency database are
resolved against the program's working directory. Every call is bounded:
opening the session takes at most its connect timeout, 90 seconds by default,
and a call at most its response timeout, 30 seconds by default, beyond what it
asks for itself, a readiness wait's timeout or a grep's time budget; closing
waits at most five seconds. A call that times out or finds the connection
broken drops the connection, and the session's next call opens a new session
for the same directory and options, starting the service again if none runs,
within that call's own bound: one deadline covers reopening and the call. A
service that is busy cannot be told from one that hangs, so a grep that may
run longer than the response timeout passes a time budget.
A write to a service that died fails the call instead of raising `SIGPIPE` in
the host. A host program that starts a service collects it when it exits,
without leaving a zombie process.

A watch subscription is a session of its own: the program opens a connection
with a watch hello naming its directory, index options, a pattern, ignore rules
and whether it wants repository state and refs, and the service answers `ready` once the
session's index is ready and the subscription is registered with its watcher,
so every change after the reply is delivered. The hello waits for the index
without the handshake's limit, bounded by the caller's connect timeout. The
service then only sends frames, each a batch of at most 128 events or a last
one that ends the subscription with an error, and the program closing the
connection ends the subscription and its session. An index without a watcher
refuses the hello (`InvalidRequest`). A watch session binds only to an index
whose watcher runs or is starting, and moves only to one whose watcher is
ready. The service ends a subscription with an error when its directory is
removed or its index's watcher stopped, which it checks every second, and when
it cannot follow its session to another index. Events are those the
index's watcher delivers to in-process subscribers, with the pattern and
ignore rules relative to the subscription's directory, as
`SharedFilePicker::watch_within` of the core applies them: created, modified,
removed, renamed with the previous path, and rescan. Paths are relative to the
subscription's directory; a rescan has an empty path. A rename across the
directory's boundary arrives as a creation or a removal. A subscription that
asks for repository state also receives a repository event when the watcher
sees a change of the git state of a repository inside its directory or
containing it that affects file status (the index, HEAD, its log, or a merge,
cherry-pick or revert in progress), as the core's
`SharedFilePicker::subscribe_repository_state` reports it for the index's own
work tree and for every repository nested in the index that its ignore rules
keep; its path is that work tree, relative when inside the directory and
absolute when above it. Files under `.git` are never file events. A
subscription at the root of a nested repository has an index of its own, as
every session's index belongs to the repository containing its directory, so
it hears only that repository and the ones nested in it.

A subscription that asks for refs also receives a refs event, with the same
path, when a branch, tag or remote-tracking ref of such a repository is
created, moved or deleted, and when a work tree's HEAD moves, as by a switch or
a commit on a detached HEAD; a HEAD change stays a repository event as well.
The core's `SharedFilePicker::subscribe_ref_changes` reports them from a loose
ref under `refs/heads`, `refs/tags` or `refs/remotes`, `packed-refs` and HEAD,
or, in a repository whose `extensions.refStorage` is `reftable`, from the
`reftable/tables.list` that git replaces for every ref update. A change of the
common dir's `config`, where upstreams and remotes live, as `git branch -u`,
`git branch --unset-upstream` or `git remote set-url` make it, is a refs event
for every followed work tree sharing it, and a change of a work tree's own
`config.worktree` one for that work tree: consumers read branch tracking as ref
state. Every config key counts, since telling which keys changed would mean
keeping and comparing each config, and a consumer reads what it needs anyway.
`.lock` paths are never events; a command that writes config twice, as
`git branch -u`, `git branch --unset-upstream` and `git remote add` do, is
heard once when the watcher's debounce window merges the two writes, as it does
in practice, and twice otherwise, never not at all. A linked worktree added,
moved, removed or pruned is a refs event for every followed work tree sharing
the common dir, as the creation or removal of the common dir's `worktrees` or of
an admin dir in it, or a change of an admin dir's `gitdir`, shows it: which
worktrees exist and where is ref state to consumers, as is what they check out.
A lock taken or released by `git worktree lock` or `unlock`, its admin dir's
`locked`, counts too, and so does `git worktree repair` rewriting a `gitdir`
unchanged. A directory there touched by the files of its worktree, as its
`index` or `HEAD`, is no event, and a merge, cherry-pick or revert in progress
there is that worktree's own: `MERGE_HEAD`, `CHERRY_PICK_HEAD` and
`REVERT_HEAD` count as a repository's state only at its git dir's root. A
worktree directory deleted without git is not announced from the main
repository until `git worktree prune`; a subscription in it ends as its
directory went. Files of another worktree's admin dir that only the ref
watches deliver, as its `index`, `HEAD`, logs and locks while an agent works
there, are dropped by their path before a batch is applied. Repository and
refs events name a work tree in one spelling, without git's trailing separator.
Other refs, such
as `refs/stash`, `refs/notes`, `FETCH_HEAD` and `ORIG_HEAD`, reflogs, git's
`.lock` files and a namespace directory's own modification are not refs events,
and a branch or tag change is no repository event. A linked worktree keeps its
branches in its main repository's common dir, the one its git dir's
`commondir` file names, so a change there names every followed work tree that
shares it: the index's own and those of the repositories nested in it that its
ignore rules keep. HEAD names only its own work tree; a reftable repository's
linked worktree keeps its HEAD in tables of its own, while the main checkout's
lives with the shared refs. libgit2 opens no reftable repository, so where it
finds none for an index, its own discovery, with git's environment, ceilings
and filesystem boundary, decides whether one is there: only a repository it
refuses for its format, at the first `.git` it meets, whose common dir stores
refs as reftable and that git's ownership rule accepts is the index's own
repository for refs. That rule is git's own (`ensure_valid_ownership` of git
2.47): the `.git` file, the work tree and the git dir belong to the current
user, which for root also means the user `SUDO_UID` names, or the work tree's
real path is a safe directory of git's protected config, read as git reads it:
the system file unless `GIT_CONFIG_NOSYSTEM`, at `GIT_CONFIG_SYSTEM` or else
where libgit2 finds it, the file `GIT_CONFIG_GLOBAL` names or else the XDG and
home files, then `GIT_CONFIG_COUNT` entries and `GIT_CONFIG_PARAMETERS`; config
git could not read refuses the repository. An empty entry resets, `*` allows all, `~`,
`~user` and an absolute `%(prefix)/` are expanded, only absolute paths and `.`
count, and a trailing `/*` allows everything below it.
A linked worktree whose admin dir was pruned and a repository git refuses have
none. A subscription that does not ask for refs never receives one.

The core's ref listener names the subscription's directory as its scope, and
an index follows the refs of its own repository and of the registered
repositories whose work tree lies inside a scope or contains it, only while a
listener is subscribed: watching refs costs one watch per directory under
`refs` on backends that watch directory by directory, as on Linux. The watches
are in place from before the subscription is ready and, except with FSEvents,
go with the last listener: each followed common dir's `worktrees`, recursively with FSEvents,
where a watch added would restart a stream, and elsewhere with every admin dir
in it, each without recursion, and its `refs`, recursively, or
its `reftable`
and that of a reftable repository's linked worktrees, and the common dir
itself, where the git state watches or the tree's recursive watch do not cover
them, as for a linked worktree whose main repository lies outside the index.
Events of such a common dir are only ref signals and register no git state. A
watch that another one shares, as a nested repository's git dir watched for its
state, is left in place, and a path whose watch failed is still removed with
the rest. A repository registered before its `refs` or `reftable` exists, as
during a clone, is followed once it appears, and its work tree announced then;
a changed config also makes the index read the ref storage again. Upstream never
watches refs.

Every change of the watched paths the watcher makes goes to notify as one
update. FSEvents on macOS restarts a stream for each update, from that moment
on, so the watcher sends no path a stream already watches and keeps two
streams: one for the index's tree, set up once with its root, and one for the
paths outside it, which every later change goes to. A restart of the outside
stream can lose only the git state of repositories outside the tree, so one
after the index's first walk began is recovered before the change that caused
it returns, and nothing rescans: ref watches brought in line again and the
`logs` of every git dir outside the tree watched, for a ref store or log that
appeared meanwhile, in at most one round more than there are such paths, then a
full git status refresh where the own git dir lies outside the tree, as for a
linked worktree or a submodule, and each repository whose git
dir or common dir lies outside the tree announced with a repository and a refs
event. The outside stream starts with the own git dir and, where they lie
outside the tree, the own repository's ref watches, and ref watches stay there
once added, so refs listeners coming and going restart nothing in a linked
worktree whose main repository lies outside the index, and an index meeting
such git dirs during its walk scans once. Subscribing and unsubscribing run on
the service's blocking threads, never on its async runtime.

The core reads a repository's git state from its git dir: its `.git`
directory, or the one its `.git` file names, as a linked worktree's and a
submodule's do, which may lie outside the indexed tree. The index's own
repository's git dir and its `logs` are watched; walks skip `.git`, so each
walk also records the git dir of every repository it reaches, at the cost of
one metadata read per walked directory, and watches it and its `logs` where the
tree's watch does not cover it: always on backends that watch directory by
directory, as on Linux, and outside the tree on recursive ones. A change in a
`.git` directory names the repository whose git dir holds it, found among the
recorded ones or by its shape (a `HEAD` beside `objects` or `commondir`), so
the git dirs of linked worktrees and submodules inside a main repository's
`.git` name their own work trees. A `.git` entry that appears in a directory
the walk had already reached, as `git init` makes, gets the same watches at
once. Each recorded git dir keeps its identity (device, inode and, where the
file system records one, birth time): a git dir the walk meets for the first
time, or that is another directory than the recorded one, as after a fresh
clone or a worktree added anew, also within one batch, is watched again and
gets a repository event once its watches are in place, since its state may
have changed before them, as after a `git init` followed at once by
`git add`. A known one is not watched again, which the polling backend would
answer with a new snapshot that loses what changed since its last poll; the
index's own git dir is recorded when the watcher starts watching it, so a walk
reaching its work tree later, as after an event for the index's root, finds it
known. A
removal event of a recorded git dir, its work tree or a directory above them
forgets the record, also when the path is back by the time the batch is read,
so a file system that gives the new directory the old one's inode and records
no birth time cannot hide it; the identity only decides when no removal was
seen. A git dir's `logs`, which a fresh repository gains with the first update
of HEAD, is watched when it appears, and its repository then gets a repository
event. Repository events are collected from the whole
batch before it is applied and delivered whatever becomes of its index
update, also when it starts a rescan.

The core calls every subscriber of an index on one thread, so the service's
callback only queues: each subscription holds at most 4096 distinct paths,
coalescing later changes of a queued path (a created or moved-in file that is
modified afterwards stays created or renamed) and moving a replaced change to
the end of the queue, so no path reads as present after a later change made it
go, and a writer task sends them.
A rename frees its source's slot, so later changes of the source follow it,
and a removal of a queued rename's destination becomes a removal of its source
in the rename's place followed by the destination's, so saving as editors do,
moving the file aside and writing a new one, keeps its order.
When a program reads slower than its files change, the queue is replaced by one
rescan event, which also covers changes until it is sent, and the service keeps
serving every other client. A rescan event also follows a rescan a program
requests, the watcher's own rescans and a move of the session to another index,
whose watcher the subscription then follows; nothing of the old index follows
that rescan, also from a callback of it still running.

`fff_service::watch::Subscription` is the client. Starting it returns at once;
on a thread of the subscription it delivers `Ready` once the service answered
`ready`, then batches, or `Failed` when it cannot be opened or ends for good.
When its connection breaks, it reopens it with backoff from 100 milliseconds to
5 seconds, starting the service when none runs, as opening a session does, and
delivers a rescan event once subscribed again; a reconnect the service refuses,
rather than one that cannot reach it, ends it with `Failed`. Dropping it ends
the subscription; once the drop returns, nothing is delivered, unless it runs
inside the delivery itself.

### C library and Node package

`fff_create_service_instance` in the C library opens a program session for the
instance's directory and returns the handle that in-process instances use. The
create options choose the index as the corresponding MCP flags do; it also
takes the `fff-mcp` executable that starts the service, an optional namespace
and a timeout that replaces both bounds of the session. Calls on one
service-backed instance may come from any thread and are serialized;
`fff_destroy` must not overlap them. A failed call on a service-backed
instance carries its kind in `int_value`, one of the `FFF_ERROR_*` constants:
the instance lost its service (`FFF_ERROR_LOST_SERVICE`, also for the health
check at the first of its two calls that loses it), a grep cursor of an index
the session left (`FFF_ERROR_STALE_CURSOR`), a call without a service
equivalent (`FFF_ERROR_UNAVAILABLE`), a directory the service cannot serve
(`FFF_ERROR_SCOPE_UNAVAILABLE`), an invalid request
(`FFF_ERROR_INVALID_REQUEST`), a path outside the instance's directory
(`FFF_ERROR_OUTSIDE_SCOPE`), an index whose coverage changed during the call,
which a retry resolves (`FFF_ERROR_RETRY`), or another failure
(`FFF_ERROR_FAILED`). A directory the service cannot index counts as one it
cannot serve. The
kind follows from the error the session or the service returns, which the
program protocol already distinguishes. Search, glob, grep, scan, wait, scan progress, base path and health
functions work on both kinds of instance and return the same result structs;
the health check reports the git work tree from the session's repository state.
Query tracking and history, git status refresh and a restart in another
directory fail on a service-backed instance with an error naming the call as
unavailable over the shared service; a restart in the same directory is a
rescan. `fff_set_watch_callback`, `fff_watch`, `fff_watch_args` and
`fff_unwatch` open and end watch subscriptions of a service-backed instance's
directory, one connection each, and deliver their batches with absolute paths
through the instance's callback, from each subscription's thread; ids come from
the core's sequence, so they never collide with in-process subscriptions.
`fff_watch` returns at once; a batch of one event of kind
`FFF_WATCH_EVENT_READY` (7) follows once the subscription is ready, or of kind
`FFF_WATCH_EVENT_FAILED` (6) when it ends for good, whose
`fff_watch_events_get_error` and `fff_watch_events_get_error_code` tell why;
both name the instance's directory. The batch struct appends `error` and
`error_code` for them, as it appended `rename_sources` before. On an instance
created without `watch`, `fff_watch` fails with `FFF_ERROR_UNAVAILABLE`. Version 2 of `FffWatchOptions` appends `repository`
in the struct's former tail padding, and `fff_watch_args_ex` takes it for
bindings that pass flat arguments; repository events have the kind
`FFF_WATCH_EVENT_REPOSITORY` (5). Version 3 appends `refs` after it, still in
the padding, which the library reads only from a version 3 struct, and `fff_watch_args_ex2` takes both; refs events have the kind
`FFF_WATCH_EVENT_REFS` (8). An in-process instance refuses `repository` and
`refs` with `FFF_ERROR_UNAVAILABLE`. Version 1 and 2 options, the other kinds
and every layout stay as they are. Once `fff_unwatch` returns on a service-backed
instance, the callback is not called for that subscription again, unless
`fff_unwatch` runs inside the callback; `fff_destroy` ends the instance's
subscriptions before it returns. Searches and greps have `_ex` variants (`_ex2` for the greps) that
exclude frecency per call, for either kind of instance. On a service-backed
instance the integer offset of the grep functions continues only the
instance's last grep page. `fff_service_call` sends one program request as
JSON and returns the whole reply as one JSON document with the field names of
the result structs, so a binding decodes a result of any size in one pass;
its grep cursors name their index. String arguments of the service functions
that are not UTF-8 fail the call. The in-process functions are unchanged for
the other bindings.

The fork's Node package is `@sakiori/fff-node`. Its platform packages
`@sakiori/fff-bin-<platform>`, for macOS on arm64 and x64 and Linux on x64 and
arm64 with glibc or musl, each carry the native library and the `fff-mcp`
executable of one build at their root; the Windows `packages/fff-bin-*`
directories stay upstream's and are not published by the fork. As upstream's
packages do, the platform packages declare `os`, `cpu` and on Linux `libc`, and
the Node package lists them as optional dependencies at its own version, so an
install brings only the running platform's one; the package picks it by the
running platform, on Linux by glibc or musl. Releases publish the six platform
packages and the Node package to npmjs.org (see [Releases](#releases)).
`FileFinder.create` always opens a service-backed instance with the
executable of the platform package, starting the service when none of that
build runs, and returns the service's error when it cannot start or be reached
instead of building a private index. `create` and every method that reaches
the service (searches, glob, grep and multi-grep, scan state, waits, rescans,
the health check and `destroy`) return a Promise of the `Result` the method
returned before and run their native call on one of at most four worker
threads the package starts when needed, so a slow or unresponsive service
blocks neither the event loop of the host, typically a server, nor the libuv
thread pool its file system, DNS, crypto and zlib calls use. Idle workers do
not keep the process alive; a worker that dies fails its call and is replaced.
Workers load the library file the main thread resolved, including its
`.asar.unpacked` copy. Calls and replies use a message channel of their own,
not the worker's parent port, where Node posts messages of its own, such as
the modules a worker requires under `node --watch`. Workers start without the
host's Node.js flags, which could change how they read their source, as
`--input-type=module` does. The package does not
implement upstream's synchronous `FileFinderApi`, and `waitForScanBlocking` is
gone; `getBasePath`, `isDestroyed`, the unavailable methods and the static
helpers stay synchronous. Each instance queues its calls and sends one at a time, as the
session serializes them anyway, so an instance holds at most one worker; when
a call finds the service lost, the calls queued behind it fail with its error
at once, and the next call reconnects. No call takes longer than its bound
once a worker runs it, waiting in the instance's queue or for one of the four
workers aside: `create` 90 seconds, a call 30 seconds plus the time it asks
for, a grep's `timeBudgetMs` included, and `timeoutMs` replaces both. The
health check makes two calls, so it can take up to twice the bound. `destroy` lets earlier calls finish, refuses later ones and
always frees the native instance; `nativeInstanceCount` reports the instances
not yet freed.
Every failed `Result` of the package carries a `code` beside the `error` text,
one of the `ErrorCode` constants the package exports: `lostService`,
`staleCursor`, `unavailable`, `scopeUnavailable`, `invalidRequest`,
`outsideScope` and `retry` map the library's kinds, and `unsupportedPlatform`, `libraryUnavailable`, `destroyed`
and `failed` come from the package. The package exports its own `Result`,
`Failure`, `ok` and `err`, whose failures carry the code, in place of the
shared API's. It loads ffi-rs only after finding the platform's native files,
so importing it on a platform ffi-rs has no binary for still works and
`FileFinder.create` reports `unsupportedPlatform`.
`notifyChanged(paths)` reports files the program changed, so that a search
after it resolves sees them without the watcher or a rescan.
`fileSearch`, `directorySearch`, `mixedSearch`, `glob`, `grep` and `multiGrep`
take `excludeFrecency`. `watch(pattern?, callback, options?)` returns a
Promise of the `Result` that upstream's synchronous `watch` returned, which
resolves once the library reports the subscription ready, or to the error it
failed with. Starting and ending a subscription are native calls that need no
service and run on the calling thread, so a subscription waiting for its index
holds none of the package's workers. The unsubscribe function it resolves to is
synchronous: once it returns, the callback is not called again. A subscription
that ends for good afterwards delivers a `failed` event carrying `error` and
`code`. `destroy` ends every subscription of the instance, resolves pending
ones as destroyed and calls no callback once it is called. `options.repository` asks for
`repository` events and `options.refs` for `refs` events, whose kinds and options are the Node package's own
`ServiceWatchEvent` and `ServiceWatchOptions`, so the API shared with fff-bun
keeps its kinds. On an instance created with `disableWatch`, `watch` fails with
`unavailable`. The library's callback is upstream's process-wide ffi-rs
trampoline, but it lives on a worker thread of its own, which starts without
the host's flags as the call workers do and which the package unrefs: an ffi-rs threadsafe function keeps its thread's event loop alive, so
on the main thread a subscription would keep the process from exiting. That
worker reads and frees each batch and posts its events to the main thread,
where they reach the subscription's callback on the event loop; neither the
worker nor a ready subscription keeps the process alive, while a subscription
waiting to be ready does, as a call in flight does. The library never waits for
the callback, so the worker bounds what waits on the event loop: past 1024
events of one subscription that the main thread has not handled, it drops the
subscription's events and, once the main thread has handled the rest, delivers
one `rescan` in their place. When the worker exits, a new one takes over the
instances' callbacks, and then every subscription hears a `rescan`, and one not
ready yet fails. Tests make the worker exit through
`exitDeliveryWorkerForTest` of `dist/watch-delivery.js`, a second bundle entry
that shares its module state with the package's entry, which does not export
it.
Searches, greps, scan state and waits go through
`fff_service_call`; `waitForScan`, `waitForIndexReady` and `getScanProgress`
return the error of a service that went away. `trackQuery`,
`getHistoricalQuery` and `refreshGitStatus` return an error naming them as
unavailable over the shared service, `reindex` rescans the instance's own
directory, and `historyDbPath` is refused. `serviceDir`, or `FFF_MCP_SERVICE_DIR`, selects the namespace, and
`FFF_MCP_IDLE_TIMEOUT_SECS` the idle timeout of a service the package starts.
The package finds the library and the executable together once per process:
the library `FFF_C_LIB` names, with the executable beside it, or else both in
its platform package or a development build. It maps the directory holding
them from an `.asar` archive to its `.asar.unpacked` counterpart when that
exists, so Electron applications need no patches; it reports an error when
only one of the files is unpacked, and names the supported platforms on any
other. It ships as an ES module whose CommonJS `require` entry is the same
module, which needs Node 20.19 or 22.12 and later.

`make prepare-node` builds the executable and the library in separate cargo
invocations and stages them for the package's development
build, each under a temporary name renamed over the old file, as
`make stage-node-platform` does: on macOS, copying over a binary a running
service maps gets the new one killed; `make test-node` runs the package's tests, each against a service in a
disposable namespace with a temporary state directory, including overlapping
calls on one and on several instances, watch subscriptions (delivery on the
event loop, patterns and ignore rules, unsubscribing, repository state, the
end of subscriptions with `destroy`, `unavailable` with `disableWatch`, a
process that exits while subscribed, repository state below the work tree, refs
only when asked, a
callback that falls behind, which gets at most 1024 events and then a rescan, a
delivery worker that exits, a removed directory's `failed` event, delivery in
a program run under `node --watch` or with `--input-type=module`, and searches
that still run while six subscriptions wait on a stopped service), and a service stopped with `SIGSTOP`,
whose calls fail within their bound while a timer keeps firing and, with more
instances than workers, file reads keep their latency. `make stage-node-platform`
copies the same build into the host's platform package.

### Building and running

Run `make build-mcp` to build the downstream executable from a checkout in
`target/release`. That build uses the default walker and needs no Zig; builds
with zlob, as `make test-rust` uses, need the Zig version pinned in `mise.toml`;
with mise, run `mise trust` and `mise install` in the checkout once. Configure
each MCP client to launch that executable, passing its desired search directory
as the positional argument. The upstream install scripts and package manager
releases do not contain these downstream changes.

The upstream update notice is off. Upstream's `fff-mcp` asks GitHub for
upstream's latest release when its service starts and tells agents to install it
with upstream's script, which would replace the downstream build. The fork's
service makes no update request and adds no notice to its instructions or tool
results; `--no-update-check` is still accepted, so existing client
configurations keep working.

The first client starts the service automatically, detached from itself: in
its own process group, with `/` as its working directory, standard streams on
`/dev/null` and its startup log, only the variables of the client's environment
it uses (home, user, path, locale, time zone, temporary and XDG directories,
`FFF_` and `RUST_` settings, and nix-ld's `NIX_LD` and `NIX_LD_LIBRARY_PATH`
on NixOS; no `LD_*` or `DYLD_*`, which would load an application's libraries),
and none of the client's descriptors beyond the standard streams: the child
marks them close-on-exec before exec, with `close_range` on Linux, otherwise up
to the soft descriptor limit, or the system's per-process maximum when that is
unlimited, computed before the fork. So the service holds nothing of the
application that started its client, such as the pipe through which an
AppImage keeps its mount alive. A client whose executable lives inside the
running AppImage (`$APPDIR`) starts a copy in the namespace instead, since the
mount ends with the application: it installs the copy of its build once and
reuses it, retries a start the freshly written file briefly refuses, and
removes copies of other builds that no service runs and no client is
starting. Other transient locations, such as a mounted disk image, are not
detected. A client completes MCP
initialization as soon as its session is registered; the index keeps scanning in
the service. Tool calls wait up to 30 seconds for the initial scan and, for
watched sessions, the filesystem watcher, and otherwise ask the caller to retry.
If the watcher cannot start, the index keeps serving searches without live
updates and service status reports the watcher state. A frecency database that
cannot be opened produces a warning instead of a failed session. The existing
MCP tools and stdio configuration remain available. Service status lists each
index with the options it was built with.

- `--service-status` lists every service in the namespace with its active scopes,
  index bindings and process, and marks the service of the current build.
- `--service-stop` stops every service in the namespace that has no connected
  clients and fails if any service refuses.
- `--service-dir` or `FFF_MCP_SERVICE_DIR` selects a separate service namespace.
  The default namespace is `fff/service` in the user state directory:
  `XDG_STATE_HOME` or `~/.local/state` on Unix, `LOCALAPPDATA` on Windows.
- `--idle-timeout-secs` sets the service idle timeout when it starts. The default
  is one hour; zero disables idle exit. Only search sessions keep a service busy;
  status and stop requests do not. A client whose parent process cannot be
  observed exits after the same timeout without MCP traffic.

Namespaces and state files must be private to the current OS user. On Unix, the
namespace directory must be owned by the user with no group or other access, and
every parent directory must be owned by the user or root and not writable by
others unless it is a root-owned sticky directory. Symlinks are refused, state
files are opened relative to the verified directory, and both ends of a
connection check the peer's user. When the socket path in the namespace
would exceed the platform's limit (104 bytes on macOS, 108 on Linux, including
the terminator), as under a long `HOME`, the service listens in a private
per-user runtime directory instead, under a name derived from a hash of the
namespace path and the build. It tries `$XDG_RUNTIME_DIR/fff` on Linux when
that is set, then `/tmp/fff-<uid>`. A runtime directory is created with mode
0700 and must be owned by the user, private and not a symlink; one that is
not, or cannot be created, is skipped, and only when none is usable does the
service refuse to start, naming each refusal. The namespace's state, including
the endpoint that names the socket, stays in the namespace. The service renews
the times of that socket and its directory every hour, so age-based cleaners
of `/tmp` such as systemd-tmpfiles keep them. On Windows, protected ACLs also
allow SYSTEM and Administrators; directory and file ACLs are validated
independently, and the pipe receives an explicit ACL.

Local connections require the namespace token and a matching protocol version.
Each build runs its own service in the namespace, so a new build can start
while clients of an older build keep running; the older service exits once it
is idle. A build is identified by the `fff-service` version, the commit and the
walker backend, compiled in when it is built; a build from a tree with
uncommitted changes adds a hash of those changes. A copy of an executable that
was moved or re-signed therefore joins the original's service. A client that
starts a service checks that the executable is of its own build and reports
the mismatch otherwise. A client retries through the startup lock when a service shuts down
before accepting its session. A crashed service fails current clients visibly;
subsequent launches recover stale discovery state. A service that clients can no
longer reach, because its namespace directory, discovery file or socket was
removed or taken over by a replacement, finishes its current sessions and then
exits even when idle exit is disabled. An unreadable namespace or a hung check
leaves a service running. Whatever the reason for exiting, a service removes its
discovery file and socket only while they still lead to it, so a replacement's
files stay in place. Service startup errors are written to a log in the
namespace and reported by the launching client.

The last session using an index releases it; dropping an index cancels its scan
and watcher. An idle service holds no file index. Disconnecting or losing the
parent of one stdio process does not stop other clients. A service contains
multiple indexes when coverage or policies differ; sharing does not require
scanning the entire filesystem.

### Verification

`make test-mcp` runs the MCP and service client tests and the core search
regression suite. `make lint-mcp` checks these crates and their test targets
without relaxing lints.
The shared-service tests use isolated namespaces and cover concurrent startup,
scoped tools and fallbacks, watcher updates, consolidation when an index gains
coverage through a rescan or a directory its watcher indexes, removed scopes,
concurrent searches over one index, cursor isolation, disconnects, parent exit,
side-by-side builds, copied executables, handshake rejection, namespace permissions, stale discovery
recovery, unreachable services, shutdown races, idle cleanup and the absence of
an upstream update check. Program session tests compare every search with an
in-process index of the same directory and cover cursors across a move to a
broader index, sharing with an MCP session, a program that exits without
closing its session or while it waits for the index, one frecency database
spelled two ways, rescans and repository state, searches while the index
builds, scan state while a search holds the index, a watched request beside a
snapshot index, a build tag collision, a session that starts the service
itself, a call that times out and the reconnect after it, a reconnect to a
stopped service within the call's bound, watch subscriptions (changes of the
subscription's directory only, ignored and outside files, a rescan after a
program's rescan, repository state and refs only when asked, also below the
work tree, refs of a linked worktree's main repository, a moved HEAD,
a consumer that reads nothing while thousands of files change, which alone
gets a rescan while one that reads keeps every change, the move to a broader
index, a resubscription to a new service after the old one was killed, the end
of a subscription whose directory is removed or whose reconnect is refused,
the session's end with the subscription, repositories nested in another
repository or a plain folder, each heard by its own subscription and the
enclosing one, one initialised after the scan, one removed and created again,
and linked worktrees, one removed and added again, and a submodule, whose
`.git` is a file), and a namespace whose socket path
exceeds the platform limit. The core's watch subscription tests drive refs with
the git command, in repositories with loose and packed refs and in reftable
ones: branches, namespaced ones included, and tags created, moved, renamed and
deleted, a commit, a switch, a detached HEAD and a commit on it,
`git pack-refs`, a branch only `packed-refs` holds deleted, a fetch, an index of
a linked worktree whose main repository lies outside it, with the linked
worktree's own HEAD and the main checkout's, which it does not hear, a listener
subscribing again after the last one left, a listener scoped to one repository
of a broad index, a repository whose refs appear after it, an upstream unset
and set, a remote's URL and another config key changed, each heard at least
once and at most once per write of the config, the main repository's config and a worktree's own `config.worktree` heard
in a linked worktree, worktrees added below and beside the main checkout,
moved, removed and pruned, each heard by the main checkout at least once and at
most once per write of its admin dir, also with the native watcher on macOS,
a lock taken and released, while work in another worktree, including a merge,
a cherry-pick and a revert stopped at a conflict, gives the main checkout
neither refs nor repository events; worktrees added from the main checkout of
an index of a linked worktree on macOS bring that index refs events, spelled
one way, and no repository event, as no stream restarts; and file subscribers
hear none of it. After each step the tests write two new
sentinel files and wait for the second's event, so a step's late events never
satisfy the next. On macOS tests with the native watcher follow an index of a
linked worktree whose main repository lies outside it: a branch of the main
repository and the worktree's own HEAD are heard and the main checkout's HEAD
is not, and the main repository's branch tracking and remotes are heard at
least once and at most once per write of the config, ordered by the refs of another repository whose worktree is nested in the
index and arrive through the same stream; refs listeners coming and going
announce no restart, and the index rescans nothing at startup. A native unit
test restarts the outside stream after the walk with a repository met then and
checks the announcements, the git status refresh and the watches of a ref store
and a log that appeared. The watcher's
unit tests show which paths are refs, that a branch change is no repository
state change, that an outside common dir registers no git state and that on
FSEvents the tree's stream keeps only its setup paths; the ref watches' unit
tests show which paths a scope follows and removes, a ref store that appears
later, a reftable repository's watches, and that a pruned linked worktree, a
repository libgit2 refuses for another extension and, when run as root, a
reftable repository of another user have no own repository for refs, and how
git's rule decides ownership and matches safe directories: root's repository
is no other user's, `SUDO_UID` under root, `*`, an empty entry, real paths, `~`,
`%(prefix)/`, relative entries, `.` and the `/*` prefix, and which config files
and environment entries it reads, in which order. The
service's queue tests show that nothing of an index a subscription moved away
from follows its rescan. In debug builds, `FFF_MCP_TEST_RUNTIME_DIR` replaces
the runtime directories, so tests never write to the shared ones. In debug builds, indexes stay building
while the file named by `FFF_MCP_TEST_HOLD_READINESS` exists, and program searches
hold their index locked for writing while the file named by
`FFF_MCP_TEST_HOLD_SEARCH` exists. In debug builds, `FFF_MCP_TEST_BUILD_ID` gives
the executable another build identity, so one test binary stands in for two
releases. Each test stops
the services of its namespace when it ends, also when it fails. In debug
builds, watchers leave new directories unindexed while the file named by
`FFF_TEST_HOLD_DIR_INDEXING` exists, so the watcher consolidation test orders
its steps without timing.

In test and debug builds, watchers use notify's polling backend instead of the
OS event service when `FFF_TEST_POLL_WATCHER_MS` sets an interval in
milliseconds, or for roots a test registered with `watch::poll_watcher_for`;
release builds always use the native backend; the watcher's FSEvents unit
tests start the native backend for their roots whatever the variable says. The polled events take the same
debouncer and handler, so tests check how fff handles watcher events without
depending on how promptly the OS delivers them. The shared-service tests and the
downstream core tests of scans and overflow recovery always poll, the core's
ref tests on macOS, where FSEvents restarts its stream for their watches, and
`git intake verify` sets the variable for `make test-rust` and `make test-mcp`,
so upstream's watcher tests poll there too. Verify skips five tests by exact
name. The three `fs_delete_handler_test` tests
(`raw_notify_nonrecursive_detects_deletion`,
`raw_notify_multi_nonrecursive_detects_deletion` and
`debounced_nonrecursive_detects_deletion`) build notify's native watcher
themselves and test OS and notify event delivery, not fff code, so polling
cannot reach them. The two `watch_subscription_test` tests
`renaming_out_of_a_watched_subtree_still_notifies` and
`renaming_into_a_watched_subtree_still_notifies` assert that a rename arrives
as one paired event, which native backends deliver; a polling walk is not an
atomic snapshot, so it can report the two sides in separate batches; how fff
delivers a rename to subscribers of either side is checked with injected events
in the watch registry's unit tests. Native
event delivery is covered by the `native-tests` job of the fork's merge request
pipelines, which runs `make test-rust` and `make test-mcp` on Linux without the
variable and without skips, these tests included, by the `macos-tests` job,
which runs them the same way on macOS for every push to `main`, rehearsal and
release, and by upstream's CI. `make test-rust`
and `make test-mcp` pass `TEST_ARGS` to the test harness. In debug builds,
`SharedFilePicker::end_rescan_cooldown` lets a test end the rescan cooldown
instead of waiting for it.
