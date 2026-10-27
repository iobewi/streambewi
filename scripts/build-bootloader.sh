#!/usr/bin/env bash
# Build the second stage of the OTA layout (#3) and its initial boot metadata, from the `rust-mcu-libs` revision
# the product is pinned to. Nothing is copied from it into this repository: its sources are read where cargo
# fetched them, built as they are, with the dependency versions recorded in targets/esp32/bootloader.Cargo.lock
# (the library repository has no lock file of its own).
#   scripts/build-bootloader.sh OUT_DIR   writes OUT_DIR/bootloader.bin and OUT_DIR/otadata.bin, and the library's
#                                         image validator in target/tools/validate_image
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 OUT_DIR" >&2
    exit 2
fi
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$(mkdir -p "$1" && cd "$1" && pwd)"
cd "$ROOT_DIR"

# The checkout cargo made for the pinned revision: the manifest of a library crate that the product already uses.
LIB="$(cargo metadata --locked --format-version 1 | python3 -c '
import json, pathlib, sys
meta = json.load(sys.stdin)
package = next(p for p in meta["packages"] if p["name"] == "iobewi-esp-boot")
print(pathlib.Path(package["manifest_path"]).parents[4])')"
test -f "$LIB/examples/firmware/bootloader/Cargo.toml" || { echo "no bootloader crate in $LIB" >&2; exit 1; }

# A private copy, outside this repository (its .cargo/config must not apply), built with the recorded lock.
WORK="$(mktemp -d "${TMPDIR:-/tmp}/streambewi-bootloader.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
cp -a "$LIB/." "$WORK/src"
rm -rf "$WORK/src/.git" "$WORK/src/target"
cp "$ROOT_DIR/targets/esp32/bootloader.Cargo.lock" "$WORK/src/Cargo.lock"
cd "$WORK/src"

# The same path normalisation as the product build (scripts/build-env.sh).
SYSROOT="$(rustc +esp --print sysroot)"
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
export RUSTFLAGS="--remap-path-prefix=$WORK/src=/mcu-libs --remap-path-prefix=$CARGO_HOME_DIR=/cargo --remap-path-prefix=$SYSROOT=/sysroot"
export CARGO_TARGET_DIR="$WORK/target"
TARGET=xtensa-esp32s3-none-elf

echo "== second stage from $LIB"
cargo +esp build --locked -Z build-std=core,alloc --target "$TARGET" \
    -p iobewi-esp-bootloader --features esp32s3 --profile bootloader --bins
# DIO, as ESP-IDF does: the ROM applies the header's mode before loading the second stage.
python3 -m esptool --chip esp32s3 elf2image --flash-mode dio --flash-size 4MB \
    --output "$OUT/bootloader.bin" "$CARGO_TARGET_DIR/$TARGET/bootloader/iobewi-esp-bootloader"
size=$(stat -c %s "$OUT/bootloader.bin")
if (( size <= 0 || size > 0x8000 )); then
    echo "error: the second stage ($size bytes) must end before the partition table at 0x8000" >&2
    exit 1
fi
if [[ "$(python3 -c 'import sys; print(open(sys.argv[1], "rb").read()[2])' "$OUT/bootloader.bin")" != "2" ]]; then
    echo "error: the second stage image header is not DIO" >&2
    exit 1
fi

# The initial `otadata`: slot 0 confirmed, written by the library's own codec (`iobewi-firmware-boot`).
echo "== initial otadata"
RUSTFLAGS= cargo +stable run --locked --target x86_64-unknown-linux-gnu -p iobewi-esp-boot \
    --example factory_otadata -- "$OUT/otadata.bin" 0
# The library's own image validator (the one the boot and the OTA use), for tools/package_image.py.
echo "== image validator"
RUSTFLAGS= cargo +stable build --locked --target x86_64-unknown-linux-gnu -p iobewi-esp-boot \
    --example validate_image --features esp32s3
mkdir -p "$ROOT_DIR/target/tools"
cp "$WORK/target/x86_64-unknown-linux-gnu/debug/examples/validate_image" "$ROOT_DIR/target/tools/validate_image"
echo "second stage: $size bytes; otadata: $(stat -c %s "$OUT/otadata.bin") bytes"
