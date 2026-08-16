#!/usr/bin/env bash
# Build a universal (arm64 + x86_64) release binary into dist/.
#   ./release/build.sh
set -euo pipefail
# shellcheck source=release/config.sh
source "$(dirname "${BASH_SOURCE[0]}")/config.sh"

echo "==> Ensuring Rust targets are installed"
for t in "${TARGETS[@]}"; do
    rustup target add "$t" >/dev/null
done

echo "==> Building release binaries"
for t in "${TARGETS[@]}"; do
    echo "    - $t"
    cargo build --release --target "$t"
done

mkdir -p "$DIST_DIR"

echo "==> Creating universal binary with lipo"
inputs=()
for t in "${TARGETS[@]}"; do
    inputs+=("${REPO_ROOT}/target/${t}/release/${BIN_NAME}")
done
lipo -create -output "$UNIVERSAL_BIN" "${inputs[@]}"

echo "==> Verifying architectures"
lipo -info "$UNIVERSAL_BIN"
file "$UNIVERSAL_BIN"
echo "==> Built: $UNIVERSAL_BIN"
