## Why

A fork release is versioned `<base>-sakiori.<YYYYMMDD>.<n>`, and `@sakiori/fff-node`
and its platform packages are published at that version. It is a semantic
versioning prerelease, which breaks the npm packages:

- A range matches a prerelease only on the same `major.minor.patch`, so
  `^0.11.1-sakiori.20261105.1` never takes `0.11.2-sakiori.*`. Once the release
  base moves, a consumer that installed with a range stays on the old line
  without a warning, and consumers pin exact versions instead.
- The version's `major.minor` is upstream's, while the package's contract is
  the fork's own: the Node API over the shared service. The fork cannot mark a
  breaking change of that API, and an upstream minor release changes the
  package's line even when the package did not change.

## What changes

- The fork releases under git intake's `semver` versioning: a release is the tag
  `v<x.y.z>`, computed from the Conventional Commits since the previous release
  and from the upstream releases it carries, starting at `v1.0.0`. The dated
  releases before it stay releases.
- One version for the whole release: the tag, the `fff-mcp` archives, the npm
  packages and the version `fff-mcp` reports, which also names the upstream
  release the build carries as build metadata.
- A downstream merge request's title, the subject of the commit it lands as, is
  a Conventional Commit, and the merge request's checks refuse one that is not.
- The package's contract is guarded: an API report of the Node package is
  committed, and the merge request's checks fail when it is out of date, or
  when the contract changed since the previous release by more than the change
  git intake computes for the commit the merge request lands: an addition needs
  at least a minor change, a removed or changed declaration a major one.
- Each platform package carries the Node package's version, which pins them
  exactly, as before.
- A release publishes to the npm dist-tag `latest` only a version above the
  current `latest`, so finishing an older release later does not move it back.

## Capabilities

### New capabilities

None.

### Modified capabilities

- `service-clients`: the published packages' versions and the guard of their
  contract.

## Impact

- Consumers move once from exact `0.11.1-sakiori.*` pins to `^1.0.0`; `1.0.0`
  sorts above every earlier version, so tools that pick the newest version
  offer it. mise installs `fff-mcp` as `gitlab:sakiori-oss/fff@1.0.0`.
- Release tags: the protected tag pattern covers `v<x.y.z>`.
- Release pipeline: the version job reads git intake's `semver` version;
  stamping, packing, publishing and the rehearsal use it.
- Merge request pipelines: the title check, the Node package's API report and
  the contract check. The contract check needs git intake with the previous
  release and the change in `release version --json`.
- An upstream update that changes the contract by more than its commits mark
  fails the check in its merge request until a commit on top of the update's
  merges marks the change.
- A dated release whose pipeline failed is not finished after the first semver
  release is published: its scripts publish to `latest` unconditionally.
