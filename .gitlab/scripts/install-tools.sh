#!/bin/sh
# Installs pinned tools into <dir>/bin, each download checked against its SHA-256, with no
# fallback. Hosts: x86_64 Linux (the Linux runners) and arm64 macOS (the macOS runner).
#
# usage: .gitlab/scripts/install-tools.sh <dir> <tool>...
#   tools: zig, node, cargo-zigbuild, cosign, jq, glab, shellcheck, git-intake, rustup
set -eu

dir=${1:?usage: install-tools.sh <dir> <tool>...}
shift
host="$(uname -s)-$(uname -m)"
case "$host" in
  Linux-x86_64) sha256() { sha256sum "$@"; } ;;
  Darwin-arm64) sha256() { shasum -a 256 "$@"; } ;;
  *) echo "the pinned tools are for x86_64 Linux and arm64 macOS, not $host" >&2; exit 1 ;;
esac
mkdir -p "$dir/bin"
dir=$(cd "$dir" && pwd -P)
root=$(cd "$(dirname "$0")/../.." && pwd -P)

unpinned() {
  echo "$tool is not pinned for $host" >&2
  exit 1
}

fetch() { # <url> <sha256> <file>
  curl --fail --silent --show-error --location --retry 4 --proto '=https' --tlsv1.2 --output "$3" "$1"
  echo "$2  $3" | sha256 --check --strict --quiet - || { rm -f "$3"; echo "$1 does not match its pinned SHA-256" >&2; exit 1; }
}

# <url> <sha256> <path of the executable in the archive>
unpack() {
  archive="$dir/download"
  fetch "$1" "$2" "$archive"
  tar -xf "$archive" -C "$dir/bin" --strip-components="$(printf '%s' "$3" | tr -cd / | wc -c)" "$3"
  rm "$archive"
}

# <url> <sha256> <name of the distribution's directory>: a whole distribution beside bin/.
distribution() {
  fetch "$1" "$2" "$dir/download"
  rm -rf "${dir:?}/$3"
  tar -xf "$dir/download" -C "$dir"
  rm "$dir/download"
}

for tool in "$@"; do
  case "$tool" in
    zig)
      # zlob is built with the Zig that mise.toml pins, which is upstream's (CUSTOM.md).
      version=0.17.1
      pinned=$(sed -n 's/^zig = "\(.*\)"$/\1/p' "$root/mise.toml")
      [ "$pinned" = "$version" ] || {
        echo "mise.toml pins Zig ${pinned:-(none)}, this script $version: update both together" >&2
        exit 1
      }
      case "$host" in
        Linux-x86_64) platform=x86_64-linux sum=3581863e3cbcfd0fce0efc682ce7e9e93da96668a977791d356a26d2a7e9b17a ;;
        Darwin-arm64) platform=aarch64-macos sum=75f1bc7a1e9852f17ec9a0c1895f767c4ec56d46bc8a834920eece1d0359dd8d ;;
      esac
      name="zig-$platform-$version"
      distribution "https://ziglang.org/download/$version/$name.tar.xz" "$sum" "$name"
      ln -sf "../$name/zig" "$dir/bin/zig"
      ;;
    node)
      # npm 11.19.0 comes with it; trusted publishing needs npm 11.5.1 and Node 22.14 or later.
      version=24.21.0
      case "$host" in
        Linux-x86_64) platform=linux-x64 sum=fd8e59d5a511510f6a298afb548f18c7d2b1be404d8b4a27d94fbe49f56cb2d6 ;;
        Darwin-arm64) platform=darwin-arm64 sum=6239d4cf92d864487ec8cd3615038f7b67e7f58b77b21cd2f09ea9fbd68065fe ;;
      esac
      name="node-v$version-$platform"
      distribution "https://nodejs.org/dist/v$version/$name.tar.xz" "$sum" "$name"
      for exe in node npm npx; do ln -sf "../$name/bin/$exe" "$dir/bin/$exe"; done
      ;;
    cargo-zigbuild)
      [ "$host" = Linux-x86_64 ] || unpinned
      version=0.23.4
      unpack "https://github.com/rust-cross/cargo-zigbuild/releases/download/v$version/cargo-zigbuild-x86_64-unknown-linux-musl.tar.xz" \
        9e3cf73485edbd45905c8aadbc0fdf869c7ddc3848f0c898229f2680db52e44b \
        cargo-zigbuild-x86_64-unknown-linux-musl/cargo-zigbuild
      ;;
    cosign)
      [ "$host" = Linux-x86_64 ] || unpinned
      version=3.1.4
      fetch "https://github.com/sigstore/cosign/releases/download/v$version/cosign-linux-amd64" \
        424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa "$dir/bin/cosign"
      chmod 0755 "$dir/bin/cosign"
      ;;
    jq)
      [ "$host" = Linux-x86_64 ] || unpinned
      version=1.8.2
      fetch "https://github.com/jqlang/jq/releases/download/jq-$version/jq-linux-amd64" \
        b1c22172dd303f3be49e935aa56aa48a8b7a46e0bc838b4997d3bb451495870f "$dir/bin/jq"
      chmod 0755 "$dir/bin/jq"
      ;;
    glab)
      [ "$host" = Linux-x86_64 ] || unpinned
      version=1.126.0
      unpack "https://gitlab.com/gitlab-org/cli/-/releases/v$version/downloads/glab_${version}_linux_amd64.tar.gz" \
        f386519ba83e9482979992ee7f60eeebee4d95b950f95f4fe7289e0a1b1ea74c bin/glab
      ;;
    shellcheck)
      [ "$host" = Linux-x86_64 ] || unpinned
      version=0.11.0
      unpack "https://github.com/koalaman/shellcheck/releases/download/v$version/shellcheck-v$version.linux.x86_64.tar.xz" \
        8c3be12b05d5c177a04c29e3c78ce89ac86f1595681cab149b65b97c4e227198 \
        "shellcheck-v$version/shellcheck"
      ;;
    git-intake)
      # The release is pinned in .gitlab-ci.yml, GIT_INTAKE_VERSION and its archive's checksum.
      version=${GIT_INTAKE_VERSION:?GIT_INTAKE_VERSION is not set}
      sum=${GIT_INTAKE_SHA256_X86_64_LINUX:?GIT_INTAKE_SHA256_X86_64_LINUX is not set}
      [ "$host" = Linux-x86_64 ] || unpinned
      name="git-intake-v$version-x86_64-unknown-linux-musl.tar.gz"
      unpack "https://gitlab.com/sakiori-oss/git-intake/-/releases/v$version/downloads/binaries/$name" "$sum" git-intake
      ;;
    rustup)
      # rustup-init for the macOS runner, which installs the toolchain into the job's directory.
      [ "$host" = Darwin-arm64 ] || unpinned
      version=1.29.1
      fetch "https://static.rust-lang.org/rustup/archive/$version/aarch64-apple-darwin/rustup-init" \
        ec1b9233e7f72990ecd8e62063fa7f6c3dfc2bec8e97f88bff165f9100ac696a "$dir/bin/rustup-init"
      chmod 0755 "$dir/bin/rustup-init"
      ;;
    *)
      echo "unknown tool: $tool" >&2
      exit 1
      ;;
  esac
  echo "installed $tool"
done
