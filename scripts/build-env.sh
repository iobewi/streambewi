#!/usr/bin/env bash
# Sourced, not executed. The single definition of how the firmware image is built: the release
# script and CI both go through scripts/build-image.sh, which sources this file, so the build command,
# its options, the path normalisation and the build date are the same everywhere.
#   FEATURES=usb-debug   optional cargo features of the firmware package

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET=xtensa-esp32s3-none-elf
PKG=streambewi-esp32
NAME=streambewi-esp32s3
ELF_SRC="target/$TARGET/release/$PKG"

# Deterministic build: strip the absolute paths of the checkout, of the cargo home (registry and git
# checkouts) and of the toolchain sysroot from the ELF (debug info and panic locations).
SYSROOT="$(rustc +esp --print sysroot)"
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
export RUSTFLAGS="-C force-frame-pointers \
 --remap-path-prefix=$ROOT_DIR=/src \
 --remap-path-prefix=$CARGO_HOME_DIR=/cargo \
 --remap-path-prefix=$SYSROOT=/sysroot"

# esp_app_desc embeds a build date/time; esp-bootloader-esp-idf honours SOURCE_DATE_EPOCH. It is tied
# to the last commit touching the sources (generated delivery directories excluded), which needs the
# full history: a shallow clone would give another date and another image.
SOURCE_DATE_EPOCH="$(git -C "$ROOT_DIR" log -1 --format=%ct -- . ':(exclude)dist' ':(exclude)dist-local')"
export SOURCE_DATE_EPOCH

BUILD_CMD="cargo +esp build -p $PKG --release --locked -Z build-std=core,alloc --target $TARGET${FEATURES:+ --features $FEATURES}"
