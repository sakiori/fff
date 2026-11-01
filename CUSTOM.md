# Downstream maintenance

This document describes how downstream changes to fff are maintained and how
they differ from upstream. Update it whenever a feature changes downstream
behavior, configuration, compatibility, or maintenance requirements.

## Branches

- `main` is the fork's branch and carries the downstream changes on top of
  upstream. It changes only through merge requests and is never force-pushed.
- Feature branches start from `main` and contain one logical change each.

Completed features are squash-merged into `main` through a merge request,
producing one commit per feature. The project merges by fast-forward; if `main`
moved on since the feature branch started, GitLab's rebase brings the branch up
to date before the merge. The commit describes the resulting behavior and why it
changed. Subsequent work starts from the updated `main`; a previously
squash-merged feature branch is not reused.

## Upstream updates

The upstream fff repository is not a remote of the fork. `.git-intake.toml`
describes it by URL and branch, and upstream updates are made with git-intake
in follow mode, which merges upstream `main` into `main`.

`git intake status` shows the last synced upstream commit and how far `main` is
behind, and `git intake log` lists the upstream commits still pending.
`git intake start fff=<rev>` merges upstream up to the chosen revision, a
commit of upstream `main` or an upstream tag (see _Choosing a target_ in the
[git-intake](https://gitlab.com/sakiori-oss/git-intake) README), in a
separate update worktree; without `=<rev>` it takes the tip it fetches. Resolve
conflicts against the intended behavior of both upstream and downstream
changes, stage the resolutions and run `git intake continue`.
`git intake verify` runs the install command and the checks listed in the
configuration; the combined result must pass them before the update is ready.
`git intake finish` opens the merge request into `main`.

`mise.toml` pins the Zig version that builds zlob. It is downstream-only and
must match the Zig version upstream pins in its workflow setup-zig steps and in
`flake.nix`; when an update changes upstream's pin, change `mise.toml` in the
same update.

Merge the update's merge request by fast-forward with _Squash commits_
unticked, so that the verified merge commit itself lands on `main`, keeping
upstream ancestry and the sync record that `status` and the next update read.
If `main` moved on and GitLab asks for a rebase, do not rebase: a rebase or a
squash replaces the verified commit, and `finish` refuses it. Run
`git intake abort`, update the local `main` and start the update again. Once
the merge request is merged, run `git intake finish` again to complete the
update.

## Feature delivery

Implement and verify each feature on its feature branch. Keep product
requirements and consequential design decisions alongside the affected code,
and update this document with the downstream behavior that is actually delivered.

Before integration, verify the feature against the current `main`, including
relevant upstream compatibility checks. Squash integration records the feature
as a single change; it does not replace functional verification.
