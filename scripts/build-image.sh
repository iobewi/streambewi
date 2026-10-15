#!/usr/bin/env bash
# Build the firmware and package it, the one way, for the release script and for CI.
#   scripts/build-image.sh OUT_DIR                     release image
#   FEATURES=usb-debug scripts/build-image.sh OUT_DIR  same build with extra cargo features
# Writes in OUT_DIR: the ELF, the merged flashable image, the product layout, SHA256SUMS and build-info.txt.
# Requires: the esp Rust toolchain, espflash 4.6.0, Python esptool 5.5.0, and the full git history.
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 OUT_DIR" >&2
    exit 2
fi
OUT="$1"
source "$(dirname "${BASH_SOURCE[0]}")/build-env.sh"
cd "$ROOT_DIR"
if [[ "$(git rev-parse --is-shallow-repository)" == "true" ]]; then
    echo "error: shallow clone; the build date comes from the git history (fetch-depth: 0)" >&2
    exit 1
fi

echo "== firmware build: $BUILD_CMD"
$BUILD_CMD

mkdir -p "$OUT"
cp "$ELF_SRC" "$OUT/$NAME.elf"
echo "== flashable image"
IMAGE_CMD="python3 tools/package_image.py $ELF_SRC $OUT/$NAME.bin"
$IMAGE_CMD
python3 tools/inspect_firmware_elf.py "$ELF_SRC" > "$OUT/firmware-layout.json"
( cd "$OUT" && sha256sum "$NAME.bin" "$NAME.elf" > SHA256SUMS )

{
  echo "source:            $(git rev-parse HEAD)${FEATURES:+  (features: $FEATURES)}"
  echo "rustc (esp):       $(rustc +esp --version)"
  echo "espflash:          $(espflash --version)"
  echo "RUSTFLAGS:         $RUSTFLAGS"
  echo "SOURCE_DATE_EPOCH: $SOURCE_DATE_EPOCH"
  echo "build:             $BUILD_CMD"
  echo "image:             $IMAGE_CMD"
  echo "sha256:"
  sed 's/^/  /' "$OUT/SHA256SUMS"
} > "$OUT/build-info.txt"
cat "$OUT/SHA256SUMS"
