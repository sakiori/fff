#!/usr/bin/env bash
# Builds the release files of one target with upstream's release settings (the ci profile, zlob):
# native/<platform package>/ with libfff_c and fff-mcp, and for the musl and darwin targets the
# archive dist/fff-mcp-v<version>-<target>.tar.gz (fff-mcp and LICENSE at its root) with its
# .sha256. libfff_c and fff-mcp take separate cargo invocations, so libfff_c has the same
# features everywhere, and both must report one build identity, which native/<target>.build-id
# records for pack-npm.sh. Linux targets are cross-built with cargo-zigbuild, darwin targets
# natively on macOS. The version is crates/fff-mcp/Cargo.toml's, which stamp-version.sh sets;
# the archive's name leaves out its build metadata, as the release tag does.
#
#   .gitlab/scripts/build-release.sh <target>
set -euo pipefail
cd "$(dirname "$0")/../.."

fail() {
  echo "$*" >&2
  exit 1
}

# The oldest glibc the gnu target's files run with, that of Node.js's own Linux builds.
GLIBC_FLOOR=2.28

target=${1:?usage: build-release.sh <target>}
[[ "$target" == *-apple-darwin ]] || command -v readelf > /dev/null || fail "readelf is missing; the checks of the built files need it"
: "${RUSTUP_TOOLCHAIN:?RUSTUP_TOOLCHAIN names the toolchain}"
version=$(sed -n '/^\[package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' crates/fff-mcp/Cargo.toml)
[[ -n "$version" ]] || { echo "cannot read the version from crates/fff-mcp/Cargo.toml" >&2; exit 1; }

# Every target fills a platform package; the musl and darwin targets also make an archive.
archive=false
case "$target" in
  x86_64-unknown-linux-musl) archive=true package=fff-bin-linux-x64-musl ;;
  aarch64-unknown-linux-musl) archive=true package=fff-bin-linux-arm64-musl ;;
  x86_64-unknown-linux-gnu) package=fff-bin-linux-x64-gnu ;;
  aarch64-unknown-linux-gnu) package=fff-bin-linux-arm64-gnu ;;
  aarch64-apple-darwin) archive=true package=fff-bin-darwin-arm64 ;;
  x86_64-apple-darwin) archive=true package=fff-bin-darwin-x64 ;;
  *) echo "no release build for $target" >&2; exit 1 ;;
esac

# RUSTFLAGS replaces the target rustflags of .cargo/config.toml instead of adding to them, so the
# flags a target needs are set here. config.toml turns crt-static off for musl, which libfff_c
# needs as a shared object loaded by a musl Node.js; fff-mcp links musl statically instead. The
# macOS link arguments are config.toml's.
cargo_target=$target
case "$target" in
  *-unknown-linux-musl)
    build=(zigbuild)
    exe_flags="-C target-feature=+crt-static"
    lib_flags="-C target-feature=-crt-static"
    library=libfff_c.so
    ;;
  *-unknown-linux-gnu)
    build=(zigbuild)
    cargo_target=$target.$GLIBC_FLOOR
    exe_flags=""
    lib_flags=""
    library=libfff_c.so
    ;;
  *-apple-darwin)
    build=(build)
    exe_flags="-C link-arg=-undefined -C link-arg=dynamic_lookup"
    lib_flags=$exe_flags
    library=libfff_c.dylib
    export MACOSX_DEPLOYMENT_TARGET=13
    ;;
esac

# Build paths of the job stay out of the binaries' panic locations.
remap="--remap-path-prefix=$PWD=fff --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=cargo"
rustup target add --toolchain "$RUSTUP_TOOLCHAIN" "$target"

# Builds the packages named after the flags into the target directory and sets build_id to the
# build identity fff-service's build script computed for them. Called outside $(...), so every
# failure exits the script.
build_crates() {
  local flags="$1" target_dir="$2" messages ids
  shift 2
  messages=$(mktemp)
  if ! RUSTFLAGS="${RUSTFLAGS:-} $flags $remap" cargo "${build[@]}" --locked --profile ci \
    --target "$cargo_target" --target-dir "$target_dir" "${@/#/--package=}" \
    --no-default-features --features zlob --message-format=json-render-diagnostics >"$messages"; then
    rm -f "$messages"
    fail "the build of $* failed"
  fi
  ids=$(grep -o '\["FFF_SERVICE_BUILD_ID","[0-9a-f]\{64\}"\]' "$messages" | cut -d '"' -f 4 | sort -u || true)
  rm -f "$messages"
  [[ -n "$ids" && "$ids" != *$'\n'* ]] || fail "the build of $* reported no single build identity"
  build_id=$ids
}

# The outputs are removed first, so a file a failed or partial build left is never packed.
exe="target/$target/ci/fff-mcp"
rm -f "$exe"
build_crates "$exe_flags" target fff-mcp
identity=$build_id
[[ -f "$exe" ]] || fail "the build left no $exe"
# A separate invocation keeps fff-mcp's features out of libfff_c; on musl its flags differ too.
lib_dir=target
[[ "$exe_flags" == "$lib_flags" ]] || lib_dir=target/cdylib
lib="$lib_dir/$target/ci/$library"
rm -f "$lib"
build_crates "$lib_flags" "$lib_dir" fff-c
[[ "$build_id" == "$identity" ]] || fail "libfff_c has the build identity $build_id, fff-mcp $identity"
[[ -f "$lib" ]] || fail "the build left no $lib"
echo "build identity $identity"

name="fff-mcp-v${version%%+*}-$target"
stage="target/stage/$name"
rm -rf "$stage"
mkdir -p "$stage"
cp "$exe" LICENSE "$lib" "$stage/"
if [[ "$target" == *-apple-darwin ]]; then
  codesign --force --sign - "$stage/fff-mcp"
  codesign --force --sign - "$stage/$library"
fi

case "$target" in
  # A musl binary has no program interpreter and needs no shared library.
  *-musl)
    headers=$(readelf --program-headers --dynamic "$stage/fff-mcp")
    if grep -Eq 'INTERP|\(NEEDED\)' <<<"$headers"; then
      echo "the $target binary is not statically linked" >&2
      exit 1
    fi
    echo "the $target binary is statically linked"
    # libfff_c is a shared object that needs musl's libc only, as a musl Node.js loads it.
    headers=$(readelf --file-header --dynamic --version-info "$stage/$library")
    needed=$(sed -n 's/.*(NEEDED).*\[\(.*\)\]$/\1/p' <<<"$headers")
    grep -Eq '^ *Type: +DYN' <<<"$headers" || fail "the $target $library is not a shared object"
    [[ -n "$needed" ]] || fail "the $target $library needs no libc"
    if grep -Evq '^(libc\.so|libc\.musl-[a-z0-9_]+\.so\.1|ld-musl-[a-z0-9_]+\.so\.1)$' <<<"$needed" ||
      grep -q 'GLIBC_' <<<"$headers"; then
      fail "the $target $library needs more than musl's libc: $(echo "$needed" | tr '\n' ' ')"
    fi
    echo "the $target $library is a shared object that needs musl's libc only"
    ;;
  # Every glibc symbol version the files need is at most the floor.
  *-gnu)
    for file in "$stage/fff-mcp" "$stage/$library"; do
      newest=$( (readelf --version-info "$file" | grep -o 'GLIBC_[0-9.]*' | sed 's/^GLIBC_//'; echo "$GLIBC_FLOOR") | sort -V | tail -n 1)
      [[ "$newest" == "$GLIBC_FLOOR" ]] || {
        echo "$(basename "$file") needs glibc $newest, newer than $GLIBC_FLOOR" >&2
        exit 1
      }
    done
    echo "the $target files need glibc $GLIBC_FLOOR at most"
    ;;
esac

# A binary the build machine can run must report the version it was built as; an Apple
# silicon Mac runs the x86_64 one through Rosetta.
host=$(rustc --print host-tuple 2>/dev/null || rustc -vV | sed -n 's/^host: //p')
run=()
if [[ "$target" == x86_64-apple-darwin && "$host" == aarch64-apple-darwin ]] && arch -x86_64 /usr/bin/true 2>/dev/null; then
  run=(arch -x86_64)
fi
if [[ "$target" == "$host" || "$target" == "${host%-gnu}-musl" || ${#run[@]} -gt 0 ]]; then
  # bash 3.2 on macOS counts an empty array as unbound under set -u.
  reported=$(${run[@]+"${run[@]}"} "$stage/fff-mcp" --version)
  [[ "$reported" == "fff-mcp $version ("* ]] || {
    echo "the binary reports '$reported', not version $version" >&2
    exit 1
  }
  echo "$reported"
fi

mkdir -p native
echo "$identity" >"native/$target.build-id"
rm -rf "native/$package"
mkdir -p "native/$package"
cp "$stage/fff-mcp" "$stage/$library" "native/$package/"
chmod 755 "native/$package/fff-mcp"
echo "staged native/$package"

if [[ "$archive" == true ]]; then
  # Archive entries carry no owner of the build machine, and on macOS no extended attributes.
  if tar --version | grep -q 'GNU tar'; then
    owner=(--owner=0 --group=0 --numeric-owner)
  else
    owner=(--uid 0 --gid 0 --uname '' --gname '' --no-xattrs --no-mac-metadata)
  fi
  mkdir -p dist
  COPYFILE_DISABLE=1 tar "${owner[@]}" -C "$stage" -czf "dist/$name.tar.gz" fff-mcp LICENSE
  if command -v sha256sum >/dev/null; then
    (cd dist && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")
  else
    (cd dist && shasum -a 256 "$name.tar.gz" >"$name.tar.gz.sha256")
  fi
  echo "built dist/$name.tar.gz"
fi
