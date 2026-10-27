#!/usr/bin/env bash
# Build the StreamBeWI firmware and produce a flashable image.
# The firmware carries no Wi-Fi credentials (provisioned at runtime via Improv Serial),
# so the output in dist/ is safe to version and share.
# Re-runnable; run from anywhere. Requires: rustup toolchains stable + esp, espflash 4.6.0, Python esptool 5.5.0.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

BUILD_KIND="credential-free (Wi-Fi provisioned at runtime via Improv Serial)"
if [[ -n "${WIFI_SSID:-}${WIFI_PASSWORD:-}" ]]; then
    echo "note: WIFI_SSID/WIFI_PASSWORD are ignored; the firmware is provisioned via Improv Serial." >&2
fi
# The build and the packaging are scripts/build-image.sh, the command CI runs too.
source "$ROOT_DIR/scripts/build-env.sh"
DIST="$ROOT_DIR/dist"

CORE_CMD="cargo +stable test -p streambewi-core -p streambewi --locked --target x86_64-unknown-linux-gnu"
IMAGE_CMD="python3 tools/package_image.py $ELF_SRC $DIST/$NAME.bin --bootloader $DIST/bootloader.bin --otadata $DIST/otadata.bin"

echo "== host tests"
$CORE_CMD
"$ROOT_DIR/scripts/build-image.sh" "$DIST"
rm -f "$DIST/firmware-layout.json" "$DIST/build-info.txt"   # CI artefacts; dist/ keeps BUILD.txt and SHA256SUMS


COMMIT="$(git rev-parse HEAD)"
DIRTY=""
[ -n "$(git status --porcelain --untracked-files=no -- "$ROOT_DIR" ':(exclude)dist' ':(exclude)dist-local')" ] && DIRTY=" (+ uncommitted source changes in the repository)"
{
  echo "repository:   $(git remote get-url origin 2>/dev/null || echo unknown)"
  echo "branch:       $(git rev-parse --abbrev-ref HEAD)"
  echo "source:       $COMMIT$DIRTY"
  echo "target:       $TARGET (ESP32-S3)"
  echo "profile:      release"
  echo "delivery:     $BUILD_KIND"
  echo "runtime:      native rust-mcu-libs"
  echo "rustc (esp):  $(rustc +esp --version)"
  echo "cargo (esp):  $(cargo +esp --version)"
  echo "rustc (core): $(rustc +stable --version)"
  echo "espflash:     $(espflash --version)"
  echo
  echo "RUSTFLAGS:    $RUSTFLAGS"
  echo "SOURCE_DATE_EPOCH: $SOURCE_DATE_EPOCH"
  echo "build (cwd repo root):"
  echo "  $CORE_CMD"
  echo "  $BUILD_CMD"
  echo "image (cwd repo root):"
  echo "  $IMAGE_CMD"
  echo
  echo "flash layout: single merged image (bootloader + partition table + app) at 0x0"
  echo "  0x0 -> $NAME.bin"
  echo "  (espflash upstream bootloader; product partition table targets/esp32/partitions.csv)"
  echo
  echo "sha256:"
  sed 's/^/  /' "$DIST/SHA256SUMS"
} > "$DIST/BUILD.txt"

echo "== done: $BUILD_KIND"
echo "output: $DIST"
cat "$DIST/SHA256SUMS"
