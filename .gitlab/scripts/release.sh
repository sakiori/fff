#!/bin/sh
# The last step of a release: creates the GitLab Release of the tag with the notes of
# `git intake release notes` (release-notes.md) and the links publish.sh wrote
# (release-links.json). Once it exists, git intake counts the release as published. A release
# that exists already is left alone when it links exactly these files, and is an error otherwise.
#
# usage: .gitlab/scripts/release.sh   (in a tag pipeline, with glab and jq on the PATH)
set -eu

tag=${CI_COMMIT_TAG:?not a tag pipeline}
: "${CI_API_V4_URL:?}" "${CI_PROJECT_ID:?}" "${CI_JOB_TOKEN:?}" "${CI_COMMIT_SHA:?}"
[ -s release-notes.md ] || { echo "release-notes.md is missing" >&2; exit 1; }
[ -s release-links.json ] || { echo "release-links.json is missing" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

status=$(curl --silent --show-error --retry 4 --header "JOB-TOKEN: $CI_JOB_TOKEN" --output "$tmp/release.json" \
  --write-out '%{http_code}' "$CI_API_V4_URL/projects/$CI_PROJECT_ID/releases/$tag") ||
  { echo "cannot tell whether the release $tag exists" >&2; exit 1; }
case "$status" in
  404)
    # glab sends the job token as a JOB-TOKEN header, which the Releases API accepts.
    GLAB_ENABLE_CI_AUTOLOGIN=true glab release create "$tag" --no-update --ref "$CI_COMMIT_SHA" \
      --name "$tag" --notes-file release-notes.md --assets-links "$(cat release-links.json)"
    echo "released $tag"
    ;;
  200)
    expected=$(jq -c '[.[] | {name, url}] | sort_by(.name)' release-links.json)
    present=$(jq -c '[(.assets.links // [])[] | {name, url}] | sort_by(.name)' "$tmp/release.json")
    if [ "$present" != "$expected" ]; then
      echo "the release $tag exists with other links than its files'; nothing is changed" >&2
      exit 1
    fi
    echo "the release $tag exists already with exactly these links"
    ;;
  *)
    echo "cannot tell whether the release $tag exists (HTTP $status)" >&2
    exit 1
    ;;
esac
