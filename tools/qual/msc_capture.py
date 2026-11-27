#!/usr/bin/env python3
"""Read-only UART capture through IDLab, for the MSC activity recipe (#47).

    IDLAB_URL=... IDLAB_BOARD=... IDLAB_BOARD_TOKEN=... python3 tools/qual/msc_capture.py OUT_DIR

Writes OUT_DIR/uart.log (raw bytes), OUT_DIR/chunks.tsv (host time in ms, offset and size of each chunk,
which ties the device clock of the `msct` lines to the host clock) and OUT_DIR/events.log (sessions, errors,
bytes dropped by the server). Stops when OUT_DIR/stop exists. It never resets, flashes or writes to the board.
"""
import os
import sys
import time

from idlab_client import IDLabClient

RETRY_S = 5.0
POLL_S = 0.2


def main(out_dir):
    name, token = os.environ["IDLAB_BOARD"], os.environ["IDLAB_BOARD_TOKEN"]
    os.makedirs(out_dir, exist_ok=True)
    stop = os.path.join(out_dir, "stop")
    raw = open(os.path.join(out_dir, "uart.log"), "ab", buffering=0)
    chunks = open(os.path.join(out_dir, "chunks.tsv"), "a", buffering=1)
    events = open(os.path.join(out_dir, "events.log"), "a", buffering=1)

    def event(text):
        events.write(f"{int(time.time() * 1000)}\t{text}\n")

    while not os.path.exists(stop):
        try:
            with IDLabClient(token="").control(name, token) as board:
                # The server buffer predates the session: it is skipped, not taken as evidence.
                old, cursor, _ = board.read_uart()
                event(f"session open, skipped {len(old)} buffered bytes")
                while not os.path.exists(stop):
                    data, cursor, dropped = board.read_uart(cursor)
                    if dropped:
                        event(f"DROPPED {dropped}")
                    if data:
                        chunks.write(f"{int(time.time() * 1000)}\t{raw.tell()}\t{len(data)}\n")
                        raw.write(data)
                    time.sleep(POLL_S)
        except Exception as error:  # a lost session is recorded, never hidden
            event(f"session error {type(error).__name__}: {str(error)[:160]}")
            time.sleep(RETRY_S)
    event("stop")


if __name__ == "__main__":
    main(sys.argv[1])
