## Context

A fork release is cut by git intake on the tip of `main`. Under the dated
scheme its version names the upstream release it carries, which as a
prerelease cannot be followed by a range and whose `major.minor` follows
upstream instead of the fork's contract. git intake's `semver` versioning gives
a downstream that others depend on a semantic version of its own, computed from
the Conventional Commits since the previous release, its own and the
upstream's, and raised to a major whenever the upstream moves to an
incompatible release. The upstream release a release carries is recorded in
its tag and named in its notes.

## Decision

The fork releases under `semver`, with one version for every artifact, and the
Node package's contract is guarded against the commits' marks.

- **Versioning.** `.git-intake.toml` sets `versioning = "semver"` and keeps
  `labels = ["sakiori"]`, so the dated releases stay releases and bound the
  first semver one, `v1.0.0`. The `release-version` job's `RELEASE_VERSION` is
  stamped into `fff-mcp` with `RELEASE_UPSTREAM` as build metadata, and the npm
  packages are packed at `RELEASE_VERSION` alone, since npm drops build
  metadata.
- **Titles.** A downstream merge request lands squashed with its title as the
  subject, so the title decides the change it asks for. The merge request's
  checks validate the title with commitlint's conventional configuration when
  the merge request lands squashed. git intake's update merge requests land
  without squashing; their upstream commits are read as git intake reads
  them.
- **Contract.** API Extractor reports the Node package's public API into
  `packages/fff-node/etc/fff-node.api.md`, committed; an unexported type that
  the API reaches is an error, so every member it exposes is in the report.
  The contract is that report together with the manifest's `exports`,
  `engines`, `os` and `cpu`.
- **Contract check.** The check builds the commit the merge request lands: for
  a squashed merge request, a commit with the head's tree and the merge
  request's title and description on top of the merge base with `main`; for
  one that lands unsquashed, the head. git intake's `release version --json`
  on that commit gives its previous release and the change of its next
  release. The check compares the contract of the commit with the contract at
  the previous release's tag: a declaration of the previous report that the
  new report lacks, removed or changed, needs a major change; any other
  difference, an added declaration or a changed manifest field, needs at least
  a minor one. Declarations are compared together with the declaration that
  contains them, so a member moved from one interface to another counts as
  removed. Whether a change of `engines`, `os` or `cpu` breaks consumers is
  the author's call; the check requires at least a minor change. A previous
  release without a report compares nothing.
- **Platform packages** take the Node package's version at pack time, and the
  Node package pins them exactly, as before.
- **Dist-tag.** A version is published to `latest` only when it is above the
  version `latest` names; otherwise it goes to the dist-tag `superseded`.

## Alternatives

- **A version line of the npm packages' own**, committed as `major.minor` with
  the patch counted from release tags. It keeps the archives dated, but gives
  one release two versions, and the count repeats git intake's rules for the
  downstream's line approximately.
- **Changesets.** The npm ecosystem's common tool for monorepos, but an
  upstream update carries no changeset, while the upstream's commits already
  follow Conventional Commits, so the fork's main stream of changes would
  publish nothing until someone writes one. The fork has no package groups for
  its finer granularity to serve.
- **A contract check within the merge request alone**, requiring `feat` or `!`
  whenever the merge request changes the report. It needs no previous release,
  but a declaration added and removed again before a release asks for a major
  version that no consumer needs.

## Consequences

- Every fork release that carries a change is a new version, also when only
  native code changed.
- An upstream update that changes the contract by more than its commits mark
  fails the contract check in its merge request until a commit on top of the
  update's merges marks the change; git intake keeps such commits through
  catch-ups.
- The contract check needs the full history and the fork's release tags, and
  git intake's view of the publish remote.
- Tooling that installed by exact dated version keeps working; the first
  semver release sorts above every dated one.
