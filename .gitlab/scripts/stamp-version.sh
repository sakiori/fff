#!/bin/sh
# Stamps a release or rehearsal version into the fff-mcp package of crates/fff-mcp/Cargo.toml
# and Cargo.lock in the working tree only, so the binary reports it. The CI passes
# RELEASE_VERSION of `git intake release version` with RELEASE_UPSTREAM as build metadata,
# 1.3.0+fff-0.11.0. The stamp is never committed.
#
#   .gitlab/scripts/stamp-version.sh <x.y.z>[-rehearsal.<YYYYMMDD>][+<metadata>]
set -eu
cd "$(dirname "$0")/../.."

version=${1:?usage: stamp-version.sh <version>}
# A release's version or a rehearsal's, with optional build metadata; nothing else reaches the
# sed replacement.
if ! printf '%s\n' "$version" | grep -Eqx '(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-rehearsal\.[0-9]{8})?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?'; then
  echo "not a release version: $version" >&2
  exit 1
fi

manifest=crates/fff-mcp/Cargo.toml
committed=$(sed -n '/^\[package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' "$manifest")
if ! printf '%s\n' "$committed" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+'; then
  echo "$manifest holds the version '${committed:-(none)}', not a plain X.Y.Z" >&2
  exit 1
fi

# The package's version line in the manifest, and the line after `name = "fff-mcp"` in
# Cargo.lock. Each file must change in exactly one line.
sed "/^\[package\]/,/^\[/s/^version = \"$committed\"$/version = \"$version\"/" "$manifest" > "$manifest.stamped"
sed "/^name = \"fff-mcp\"$/{n;s/^version = \"$committed\"$/version = \"$version\"/;}" Cargo.lock > Cargo.lock.stamped
for file in "$manifest" Cargo.lock; do
  if [ "$(diff "$file" "$file.stamped" | grep -c '^>')" != 1 ]; then
    rm -f "$manifest.stamped" Cargo.lock.stamped
    echo "$file does not hold the version $committed of fff-mcp exactly once" >&2
    exit 1
  fi
done
mv "$manifest.stamped" "$manifest"
mv Cargo.lock.stamped Cargo.lock
echo "stamped version $version"
