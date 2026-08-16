#!/usr/bin/env bash
# Shared configuration for the brewsoak release scripts.
# Sourced by the build/sign/notarize/release scripts; edit values here only.
# The vars below are consumed by the sourcing scripts, not this file (SC2034).
# shellcheck disable=SC2034
set -euo pipefail

# --- Project ---
BIN_NAME="brewsoak"
# Repo "owner/name" used for GitHub Releases and the Homebrew formula URL.
GH_REPO="ericfitz/brewsoakr"

# --- Code signing ---
# Developer ID Application: signs the Mach-O binary.
# Developer ID Installer:   signs the .pkg installer (a different cert).
# Use the full identity string OR the SHA-1 hash from `security find-identity -v`.
SIGN_IDENTITY="Developer ID Application: Robert Fitzgerald (796T45968D)"
INSTALLER_IDENTITY="Developer ID Installer: Robert Fitzgerald (796T45968D)"
TEAM_ID="796T45968D"

# Reverse-DNS identifier for the .pkg.
PKG_IDENTIFIER="com.ericfitz.brewsoak"

# --- Notarization ---
# Name of the keychain profile created with:
#   xcrun notarytool store-credentials sqdist-notary \
#     --apple-id "<your-apple-id>" --team-id 796T45968D --password "<app-specific-password>"
# Shared with sqdist: the same Apple ID/team signs both, so reuse the profile.
NOTARY_PROFILE="sqdist-notary"

# --- Build targets (universal binary) ---
TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)

# --- Layout (resolved relative to repo root) ---
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${REPO_ROOT}/dist"          # build outputs (gitignored)
UNIVERSAL_BIN="${DIST_DIR}/${BIN_NAME}"
