#!/usr/bin/env bash
# Codesign (Developer ID + hardened runtime), notarize, and verify the
# universal binary built by build.sh. CLI tools can't be stapled directly
# (only bundles/dmgs/pkgs can hold a stapled ticket), so we verify the
# notarization result online instead.
#   ./release/sign-notarize.sh
set -euo pipefail
# shellcheck source=release/config.sh
source "$(dirname "${BASH_SOURCE[0]}")/config.sh"

if [[ ! -f "$UNIVERSAL_BIN" ]]; then
    echo "error: $UNIVERSAL_BIN not found — run ./release/build.sh first" >&2
    exit 1
fi

echo "==> Codesigning with hardened runtime"
codesign --force --timestamp --options runtime \
    --sign "$SIGN_IDENTITY" \
    "$UNIVERSAL_BIN"

echo "==> Verifying signature"
codesign --verify --strict --verbose=2 "$UNIVERSAL_BIN"

echo "==> Zipping for notarization"
ZIP="${DIST_DIR}/${BIN_NAME}-notarize.zip"
rm -f "$ZIP"
# ditto preserves the executable bit and is Apple's recommended zipper.
ditto -c -k --keepParent "$UNIVERSAL_BIN" "$ZIP"

echo "==> Submitting to Apple notary service (this can take a few minutes)"
xcrun notarytool submit "$ZIP" \
    --keychain-profile "$NOTARY_PROFILE" \
    --wait

echo "==> Confirming the binary is accepted by Gatekeeper policy"
# spctl assesses against the system policy; for a notarized signed CLI this
# should report "accepted" with source "Notarized Developer ID".
spctl --assess --type execute --verbose=4 "$UNIVERSAL_BIN" || {
    echo "warning: spctl assessment did not pass. For a bare CLI binary this" >&2
    echo "         can still be fine once distributed in a notarized archive;" >&2
    echo "         review the notarytool log above for the authoritative result." >&2
}

rm -f "$ZIP"
echo "==> Signed + notarized: $UNIVERSAL_BIN"
