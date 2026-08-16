#!/usr/bin/env bash
# Wrap the signed/notarized universal binary in a signed, notarized, STAPLED
# .pkg installer for manual (non-Homebrew) distribution. Installs brewsoak to
# /usr/local/bin. Unlike a bare CLI, a .pkg can carry a stapled ticket, so it
# opens with zero Gatekeeper friction even offline.
#
# Run AFTER build.sh and sign-notarize.sh (the binary must already be signed).
#   ./release/build-pkg.sh            # version from Cargo.toml
#   ./release/build-pkg.sh 0.1.0      # explicit version
set -euo pipefail
# shellcheck source=release/config.sh
source "$(dirname "${BASH_SOURCE[0]}")/config.sh"

if [[ ! -f "$UNIVERSAL_BIN" ]]; then
    echo "error: $UNIVERSAL_BIN not found — run build.sh + sign-notarize.sh first" >&2
    exit 1
fi

# Refuse to package an unsigned binary; the .pkg would notarize but the inner
# binary would still trip Gatekeeper.
if ! codesign --verify --strict "$UNIVERSAL_BIN" 2>/dev/null; then
    echo "error: $UNIVERSAL_BIN is not validly signed — run sign-notarize.sh first" >&2
    exit 1
fi

VERSION="${1:-$(grep -m1 '^version' "${REPO_ROOT}/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')}"
PKG="${DIST_DIR}/${BIN_NAME}-v${VERSION}-macos.pkg"

# Stage a payload root containing just the install layout: usr/local/bin/brewsoak.
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "${STAGE}/usr/local/bin"
cp "$UNIVERSAL_BIN" "${STAGE}/usr/local/bin/${BIN_NAME}"
chmod 755 "${STAGE}/usr/local/bin/${BIN_NAME}"

echo "==> Building component package (pkgbuild)"
UNSIGNED_PKG="${DIST_DIR}/${BIN_NAME}-unsigned.pkg"
pkgbuild \
    --root "$STAGE" \
    --identifier "$PKG_IDENTIFIER" \
    --version "$VERSION" \
    --install-location "/" \
    "$UNSIGNED_PKG"

echo "==> Signing installer (Developer ID Installer)"
productsign --sign "$INSTALLER_IDENTITY" "$UNSIGNED_PKG" "$PKG"
rm -f "$UNSIGNED_PKG"

echo "==> Verifying installer signature"
pkgutil --check-signature "$PKG"

echo "==> Notarizing the .pkg (a few minutes)"
xcrun notarytool submit "$PKG" \
    --keychain-profile "$NOTARY_PROFILE" \
    --wait

echo "==> Stapling the ticket to the .pkg"
xcrun stapler staple "$PKG"
xcrun stapler validate "$PKG"

echo "==> Final Gatekeeper assessment (installer policy)"
spctl --assess --type install --verbose=4 "$PKG"

echo "==> Built + stapled: $PKG"
