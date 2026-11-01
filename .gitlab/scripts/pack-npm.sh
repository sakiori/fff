#!/usr/bin/env bash
# Packs @sakiori/fff-node and its platform packages at RELEASE_VERSION into dist-npm/, from the
# native files the builds left in native/. Every target must report one build identity, so the
# Node package of a release joins the service of the release's fff-mcp archives. The platform
# packages declare os, cpu and on Linux libc; the Node package lists them as optional
# dependencies at exactly the release, so an install brings only the matching one. Naming
# targets packs only their platform packages, as for a local check; a release packs all.
#
#   RELEASE_VERSION=<version> .gitlab/scripts/pack-npm.sh [<target>...]
set -euo pipefail
cd "$(dirname "$0")/../.."

# <target> <package> <os> <cpu> <libc or ->
PLATFORMS=(
  "aarch64-apple-darwin fff-bin-darwin-arm64 darwin arm64 -"
  "x86_64-apple-darwin fff-bin-darwin-x64 darwin x64 -"
  "aarch64-unknown-linux-gnu fff-bin-linux-arm64-gnu linux arm64 glibc"
  "aarch64-unknown-linux-musl fff-bin-linux-arm64-musl linux arm64 musl"
  "x86_64-unknown-linux-gnu fff-bin-linux-x64-gnu linux x64 glibc"
  "x86_64-unknown-linux-musl fff-bin-linux-x64-musl linux x64 musl"
)
OUT=dist-npm

fail() {
  echo "$*" >&2
  exit 1
}

: "${RELEASE_VERSION:?RELEASE_VERSION names the version to pack}"

# The platforms to pack: all, or those of the targets named.
selected=()
for entry in "${PLATFORMS[@]}"; do
  read -r target _ <<<"$entry"
  if [[ $# -eq 0 ]] || [[ " $* " == *" $target "* ]]; then selected+=("$entry"); fi
done
[[ ${#selected[@]} -gt 0 && ( $# -eq 0 || ${#selected[@]} -eq $# ) ]] || fail "not release targets: $*"

identity=""
targets=()
for entry in "${selected[@]}"; do read -r target _ <<<"$entry"; targets+=("$target"); done
for target in "${targets[@]}"; do
  [[ -s "native/$target.build-id" ]] || fail "the build of $target left no build identity"
  id=$(cat "native/$target.build-id")
  [[ -z "$identity" || "$id" == "$identity" ]] ||
    fail "the build of $target has the build identity $id, another target's $identity"
  identity=$id
done
echo "every target has the build identity $identity"

# npm ci checks the workspace against its lockfile, so it runs before any version changes.
(cd packages && npm ci --workspace fff-node --ignore-scripts --no-audit --no-fund)
make sync-js-api-check

names=()
for entry in "${selected[@]}"; do
  read -r _ platform _ <<<"$entry"
  names+=("$platform")
  node scripts/stage-node-platform.mjs "native/$platform" "packages/$platform"
  node scripts/set-npm-version.mjs "packages/$platform" "$RELEASE_VERSION"
done
node scripts/set-npm-version.mjs packages/fff-node "$RELEASE_VERSION"
(cd packages/fff-node && npm run build)

rm -rf "$OUT"
mkdir -p "$OUT"
for dir in "${names[@]}" fff-node; do
  npm pack --ignore-scripts --pack-destination "$OUT" "./packages/$dir" >/dev/null
done

# The packed manifests, as the registry receives them.
# shellcheck disable=SC2016 # JavaScript template literals
node -e '
  const { execFileSync } = require("node:child_process");
  const [out, version, chosen, ...platforms] = process.argv.slice(1);
  const manifest = (name) =>
    JSON.parse(
      execFileSync("tar", ["-xzOf", `${out}/sakiori-${name}-${version}.tgz`, "package/package.json"], {
        encoding: "utf8",
      }),
    );
  const sorted = (deps) => JSON.stringify(Object.entries(deps ?? {}).sort());
  const problems = [];
  const node = manifest("fff-node");
  if (node.version !== version) problems.push(`@sakiori/fff-node is at ${node.version}`);
  const expected = {};
  for (const entry of platforms) {
    const [target, name, os, cpu, libc] = entry.split(" ");
    expected[`@sakiori/${name}`] = version;
    if (!chosen.split(" ").includes(target)) continue;
    const packed = manifest(name);
    const want = { os: [os], cpu: [cpu], ...(libc === "-" ? {} : { libc: [libc] }) };
    if (packed.name !== `@sakiori/${name}`) problems.push(`${name} is packed as ${packed.name}`);
    if (packed.version !== version) problems.push(`@sakiori/${name} is at ${packed.version}`);
    for (const field of ["os", "cpu", "libc"]) {
      if (JSON.stringify(packed[field]) !== JSON.stringify(want[field])) {
        problems.push(`@sakiori/${name} declares ${field} ${JSON.stringify(packed[field])}, not ${JSON.stringify(want[field])}`);
      }
    }
  }
  if (sorted(node.optionalDependencies) !== sorted(expected)) {
    problems.push(`@sakiori/fff-node has the optional dependencies ${sorted(node.optionalDependencies)}, not ${sorted(expected)}`);
  }
  const required = Object.keys(node.dependencies ?? {}).filter((name) => name.startsWith("@sakiori/"));
  if (required.length > 0) problems.push(`@sakiori/fff-node requires ${required.join(", ")}`);
  if (problems.length > 0) {
    console.error(problems.join("\n"));
    process.exit(1);
  }
' "$OUT" "$RELEASE_VERSION" "${targets[*]}" "${PLATFORMS[@]}"

# Each platform package holds both native files, fff-mcp executable for everyone.
for platform in "${names[@]}"; do
  tarball="$OUT/sakiori-$platform-$RELEASE_VERSION.tgz"
  library=$(node -p "require('./packages/$platform/package.json').main")
  listing=$(tar -tvzf "$tarball")
  grep -Eq "^-rwxr-xr-x .* package/fff-mcp\$" <<<"$listing" ||
    fail "$tarball holds no executable package/fff-mcp: $listing"
  awk -v file="package/$library" '$NF == file { found = 1 } END { exit !found }' <<<"$listing" ||
    fail "$tarball holds no package/$library: $listing"
done

if command -v sha256sum >/dev/null; then
  (cd "$OUT" && sha256sum ./*.tgz)
else
  (cd "$OUT" && shasum -a 256 ./*.tgz)
fi
