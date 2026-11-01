#!/bin/sh
# Uploads a signed release's files in dist/ to the project's generic package registry, package
# fff-mcp, version <tag>, and writes the GitLab Release's asset links to release-links.json for
# release.sh. Each link is named as its file, which mise's GitLab backend matches, and has the
# direct asset path /binaries/<file>, the permanent URL
# <project>/-/releases/<tag>/downloads/binaries/<file>.
#
# A retried job uploads only the files the package lacks and refuses a file the package holds
# with other content; nothing is replaced. Every API response goes to a file before it is
# parsed, so a failed request stops the job instead of reading as an empty answer.
#
# usage: .gitlab/scripts/publish.sh   (in a tag pipeline, with jq on the PATH)
set -eu

tag=${CI_COMMIT_TAG:?not a tag pipeline}
: "${CI_API_V4_URL:?}" "${CI_PROJECT_ID:?}" "${CI_JOB_TOKEN:?}"

project="$CI_API_V4_URL/projects/$CI_PROJECT_ID"
package="$project/packages/generic/fff-mcp/$tag"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Writes the response to <file>; on an HTTP error, shows it and fails.
api() { # <file> <curl arguments>...
  out=$1
  shift
  curl --fail-with-body --silent --show-error --retry 4 --header "JOB-TOKEN: $CI_JOB_TOKEN" --output "$out" "$@" || {
    status=$?
    cat "$out" >&2 2>/dev/null || true
    echo >&2
    return "$status"
  }
}

cd dist
files=""
for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl x86_64-apple-darwin aarch64-apple-darwin; do
  archive="fff-mcp-$tag-$target.tar.gz"
  files="$files $archive $archive.sha256 $archive.sigstore.json"
done
files="$files SHA256SUMS SHA256SUMS.sigstore.json"
for file in $files; do
  [ -e "$file" ] || { echo "dist/ lacks $file" >&2; exit 1; }
done

# The files the package already holds, by name, with their SHA-256.
api "$tmp/packages.json" --get "$project/packages" --data package_type=generic \
  --data package_name=fff-mcp --data-urlencode "package_version=$tag"
id=$(jq -r --arg tag "$tag" '[.[] | select(.name == "fff-mcp" and .version == $tag)] | first | .id // empty' \
  "$tmp/packages.json")
held='[]'
if [ -n "$id" ]; then
  api "$tmp/files.json" --get "$project/packages/$id/package_files" --data per_page=100
  held=$(jq -c '[.[] | {name: .file_name, sha256: .file_sha256}]' "$tmp/files.json")
fi

links='[]'
for file in $files; do
  sum=$(sha256sum "$file" | cut -d' ' -f1)
  stored=$(printf '%s' "$held" | jq -r --arg name "$file" '[.[] | select(.name == $name) | .sha256] | unique | join(" ")')
  if [ -z "$stored" ]; then
    api "$tmp/upload.json" --upload-file "$file" "$package/$file"
    echo "uploaded $file"
  elif [ "$stored" != "$sum" ]; then
    echo "the package $tag holds $file with other content; nothing is replaced" >&2
    exit 1
  else
    echo "$file is uploaded already"
  fi
  case "$file" in
    *.tar.gz) type=package ;;
    *) type=other ;;
  esac
  links=$(printf '%s' "$links" | jq -c --arg name "$file" --arg url "$package/$file" --arg type "$type" \
    '. + [{name: $name, url: $url, direct_asset_path: ("/binaries/" + $name), link_type: $type}]')
done
cd ..
printf '%s\n' "$links" > release-links.json
jq -r '.[].name' release-links.json
