#!/bin/sh
# Signs a release's archives in dist/ with cosign, keyless: the job's GitLab ID token
# (SIGSTORE_ID_TOKEN, audience sigstore) obtains a short-lived certificate naming this
# project's CI configuration and the tag, and each signature goes to Sigstore's transparency
# log. Writes SHA256SUMS over the archives and <file>.sigstore.json for every archive and for
# SHA256SUMS, then verifies each bundle against the identity users verify with (README.md).
#
# usage: .gitlab/scripts/sign.sh   (in a tag pipeline, with cosign on the PATH)
set -eu

tag=${CI_COMMIT_TAG:?not a tag pipeline}
: "${SIGSTORE_ID_TOKEN:?the job has no ID token for Sigstore}"
: "${CI_SERVER_URL:?}" "${CI_PROJECT_PATH:?}" "${CI_CONFIG_PATH:?}"

cd dist
set -- \
  "fff-mcp-$tag-x86_64-unknown-linux-musl.tar.gz" \
  "fff-mcp-$tag-aarch64-unknown-linux-musl.tar.gz" \
  "fff-mcp-$tag-x86_64-apple-darwin.tar.gz" \
  "fff-mcp-$tag-aarch64-apple-darwin.tar.gz"
for archive in "$@"; do
  [ -e "$archive" ] || { echo "dist/ lacks $archive" >&2; exit 1; }
  [ -e "$archive.sha256" ] || { echo "$archive has no .sha256 next to it" >&2; exit 1; }
done
sha256sum --check --strict -- *.sha256
sha256sum -- "$@" > SHA256SUMS

identity="$CI_SERVER_URL/$CI_PROJECT_PATH//$CI_CONFIG_PATH@refs/tags/$tag"
for file in "$@" SHA256SUMS; do
  cosign sign-blob --yes --bundle "$file.sigstore.json" "$file"
  cosign verify-blob --bundle "$file.sigstore.json" \
    --certificate-identity "$identity" --certificate-oidc-issuer "$CI_SERVER_URL" "$file"
done
ls -l
