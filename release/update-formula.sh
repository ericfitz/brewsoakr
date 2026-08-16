#!/usr/bin/env bash
# Render the Homebrew formula from the template with a given tag + sha256.
# Writes to dist/brewsoak.rb by default; copy it into your tap repo's Formula/.
#   ./release/update-formula.sh v0.1.0 <sha256>
#   TAP_DIR=~/code/homebrew-tap ./release/update-formula.sh v0.1.0 <sha256>
set -euo pipefail
# shellcheck source=release/config.sh
source "$(dirname "${BASH_SOURCE[0]}")/config.sh"

TAG="${1:?usage: update-formula.sh <tag> <sha256>}"
SHA="${2:?usage: update-formula.sh <tag> <sha256>}"
VERSION="${TAG#v}"
TARBALL="${BIN_NAME}-${TAG}-macos-universal.tar.gz"
URL="https://github.com/${GH_REPO}/releases/download/${TAG}/${TARBALL}"

TMPL="${REPO_ROOT}/release/homebrew/brewsoak.rb.tmpl"
OUT="${DIST_DIR}/${BIN_NAME}.rb"
mkdir -p "$DIST_DIR"

sed -e "s|__VERSION__|${VERSION}|g" \
    -e "s|__URL__|${URL}|g" \
    -e "s|__SHA256__|${SHA}|g" \
    "$TMPL" > "$OUT"

echo "==> Wrote $OUT"

if [[ -n "${TAP_DIR:-}" ]]; then
    DEST="${TAP_DIR}/Formula/${BIN_NAME}.rb"
    mkdir -p "$(dirname "$DEST")"
    cp "$OUT" "$DEST"
    echo "==> Copied to $DEST  (commit & push the tap to publish)"
else
    echo "    Set TAP_DIR=<path-to-homebrew-tap> to auto-copy into your tap."
fi
