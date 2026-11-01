#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."

# mise.toml must pin the Zig that upstream's setup-zig steps and flake.nix use (CUSTOM.md).
pinned=$(sed -n 's/^zig = "\(.*\)"\r*$/\1/p' mise.toml)
[[ -n "$pinned" ]] || { echo "mise.toml pins no Zig" >&2; exit 1; }
status=0
while IFS=: read -r file version; do
  if [[ "$version" != "$pinned" ]]; then
    echo "$file installs Zig $version, but mise.toml pins $pinned" >&2
    status=1
  fi
done < <(
  awk '/uses: .*setup-zig@/ { zig = 1; next } zig && /version:/ { gsub(/["'"'"' ]/, "", $2); print FILENAME ":" $2; zig = 0 }' .github/workflows/*.y*ml
  # shellcheck disable=SC2016 # the literal ${system} of flake.nix
  sed -n 's/.*zig-overlay\.packages\.\${system}\."\([^"]*\)".*/flake.nix:\1/p' flake.nix
)
[[ $status -eq 0 ]] && echo "Zig $pinned is pinned everywhere"
exit "$status"
