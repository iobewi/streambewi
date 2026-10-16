#!/usr/bin/env bash
# Flash dist/streambewi-esp32s3.bin (merged image, offset 0x0) and open the monitor.
# The serial port is auto-detected by espflash; pass --port <dev> to force one.
# Use the board's USB-UART port (not the native OTG port): it carries flashing, logs
# and Improv Serial provisioning.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="$ROOT_DIR/dist/streambewi-esp32s3.bin"
if [[ ! -f "$IMAGE" ]]; then
    echo "missing flash image: $IMAGE" >&2
    exit 1
fi

# Historical dist images are retained in Git; never mistake them for this port.
if ! grep -q '^runtime: *native rust-mcu-libs$' "$ROOT_DIR/dist/BUILD.txt" 2>/dev/null; then
    echo "dist is historical or missing build metadata; run scripts/build-release.sh first" >&2
    exit 1
fi

echo "flashing: $IMAGE"
python3 -m esptool --chip esp32s3 "$@" write-flash 0x0 "$IMAGE"
espflash monitor --chip esp32s3 "$@"
