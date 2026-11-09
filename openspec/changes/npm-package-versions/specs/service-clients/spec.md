## ADDED Requirements

### Requirement: Package versions
The Node package and its platform packages SHALL be published at the version of
the fork release that builds them, a semantic version without a prerelease part
or build metadata, which git intake computes under `semver` from the
Conventional Commits since the previous release and the upstream releases the
release carries. The `fff-mcp` archives and the version `fff-mcp` reports SHALL
carry the same version, `fff-mcp` with the upstream release of the build as
build metadata.

#### Scenario: Ranges take later releases
- **WHEN** a consumer depends on `^<major>.<minor>.<patch>` of the Node package and a later fork release publishes on the same major
- **THEN** the range includes the later release's version, whatever upstream release the later fork release carries

#### Scenario: First semver release
- **WHEN** the first fork release after the dated releases `v0.11.1-sakiori.*` is cut
- **THEN** its tag is `v1.0.0`, and the packages are published at `1.0.0`

#### Scenario: Version of fff-mcp
- **WHEN** `fff-mcp` of the release `v1.3.0`, which carries upstream `v0.11.0`, reports its version
- **THEN** it reports `1.3.0+fff-0.11.0`

#### Scenario: Rehearsal version
- **WHEN** a pipeline started by hand on `main` rehearses a release
- **THEN** it packs and checks the packages at git intake's rehearsal version of that commit, and publishes nothing

#### Scenario: Release of a superseded version
- **WHEN** a release publishes a version below the version the npm dist-tag `latest` names, as when an earlier failed release is finished after a later one
- **THEN** the packages are published without moving `latest`

### Requirement: Merge request titles
A downstream merge request that lands squashed SHALL have a Conventional Commit
as its title, since its title is the subject git intake reads to version the
next release.

#### Scenario: Title that is no Conventional Commit
- **WHEN** a merge request that lands squashed is titled `Update the README`
- **THEN** its checks fail and name the Conventional Commit form

#### Scenario: Update merge request
- **WHEN** git intake's update merge request, which lands without squashing, carries upstream commits whose subjects are not Conventional Commits
- **THEN** the title check does not apply, and git intake counts each such commit as a patch change

### Requirement: Package contract
The Node package's contract SHALL be its public API, as an API report committed
in the repository, together with its manifest's `exports`, `engines`, `os` and
`cpu`. A merge request's checks SHALL fail when the committed report is out of
date.

#### Scenario: Out-of-date API report
- **WHEN** a change alters the Node package's public types without updating the committed API report
- **THEN** the merge request's checks fail

### Requirement: Contract change marked by the commits
A merge request's checks SHALL fail when the contract of the commit the merge
request lands differs from the contract of that commit's previous release by
more than the change git intake computes for that commit. A report that lacks a
declaration of the previous release's report, removed or changed, SHALL need a
major change, and any other difference at least a minor change.

#### Scenario: Addition under a fix
- **WHEN** a merge request titled `fix: …` adds a method to the Node package's API, and nothing since the previous release asks for a minor or major change
- **THEN** its checks fail and say that the change needs at least `feat`

#### Scenario: Removal without a breaking change
- **WHEN** a merge request removes or changes a declaration of the previous release's API report, and nothing since the previous release is marked breaking or moved the upstream to an incompatible release
- **THEN** its checks fail and say that the change needs `!` or a `BREAKING CHANGE` footer

#### Scenario: Change undone before a release
- **WHEN** one merge request adds a declaration and a later one removes it again before any release
- **THEN** the later merge request's contract equals the previous release's in that declaration, and needs no breaking change for it

#### Scenario: Update that changes the contract
- **WHEN** git intake's update merge request brings upstream commits that change the contract by more than their messages mark
- **THEN** its checks fail until a commit on top of the update's merges marks the change

#### Scenario: Previous release without a report
- **WHEN** the previous release of the landed commit predates the API report
- **THEN** the contract check compares nothing, since the first semver release is `v1.0.0`
