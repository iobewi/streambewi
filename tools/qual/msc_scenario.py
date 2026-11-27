#!/usr/bin/env python3
"""Report of one scenario of the MSC recipe (#47), against the M0 signature. It decides nothing.

    python3 tools/qual/msc_scenario.py CAPTURE_DIR FROM_MS [TO_MS]

Times are device times (the `t=` of the `msct` lines). Silences are only compared with the longest one
seen in M0: that is a reference, not a detection threshold.
"""
import os
import re
import sys

M0_MAX_SILENCE_MS = 270
DATA_START_LBA = 261  # first sector of RADIO.MP3 (FAT16_CONFIG)

TIMED = re.compile(r"msct ([RCBS]) .*?\bt=(\d+)(?:-(\d+))?")
READ = re.compile(r"msct R t=(\d+)-(\d+) n=(\d+) lba=(\d+)\.\.(\d+) blk=(\d+)/(\d+) gap=(\d+) svc=(\d+) st=(\S+)")
STATUS = re.compile(r"msct S t=(\d+) live=(\d+) consumed=(\d+) open=(\S+) lost=(\d+)")


def short(line):
    return line.split("msc_trace: ")[-1] if "msc_trace: " in line else line.replace("INFO - streambewi::", "")


def main(capture, start, end):
    text = open(os.path.join(capture, "uart.log"), "rb").read().decode("utf-8", "replace").replace("\r", "")
    rows, now = [], None  # (device time of the last msct line at or before it, line)
    for line in text.split("\n"):
        match = TIMED.search(line)
        if match:
            now = int(match.group(3) or match.group(2))
        if now is not None and start <= now <= end and line.strip():
            rows.append((now, line))
    reads = [(i, tuple(map(int, m.groups()[:9])), m.group(10)) for i, (_, l) in enumerate(rows) if (m := READ.search(l))]
    status = [(int(m[1]), int(m[2]), int(m[3]), m[4], int(m[5])) for _, l in rows if (m := STATUS.search(l))]
    print(f"window {start}..{rows[-1][0] if rows else '?'}: {len(reads)} read lines, "
          f"lost={max((s[4] for s in status), default='?')}, "
          f"max S interval={max((b[0] - a[0] for a, b in zip(status, status[1:])), default=0)} ms")
    events = open(os.path.join(capture, "events.log")).read()
    print("bench:", f"DROPPED={events.count('DROPPED')} session_errors={events.count('session error')}")
    print("read sizes:", sorted({(r[5], r[6]) for _, r, _ in reads}), "| bad status:", [s for _, _, s in reads if s != "0"])
    jumps = [(a[1][4], b[1][3], b[1][0]) for a, b in zip(reads, reads[1:]) if b[1][3] != a[1][4] + 1]
    print("LBA discontinuities (last, next, at ms):", jumps)
    print("commands other than READ(10):")
    for _, line in rows:
        if "msct C " in line:
            print("   ", short(line))
    print("USB bus events:", [short(l) for _, l in rows if "msct B " in l])
    print("product lines (except status LED and 60 s counters):")
    for t, line in rows:
        if "msct " not in line and not re.search(r"stream: live=\d+ consumed", line):
            print(f"    ~{t}  {short(line)[:150]}")
    silences = [(a, b) for a, b in zip(reads, reads[1:]) if b[1][7] > M0_MAX_SILENCE_MS]
    print(f"READ(10) silences longer than the M0 maximum ({M0_MAX_SILENCE_MS} ms): {len(silences)}")
    for (i, a, _), (j, b, _) in silences:
        gap = b[7]
        during = [s for s in status if a[1] < s[0] < b[0]]
        print(f"  silence {gap} ms")
        print(f"    last read before : t={a[1]} lba..{a[4]} (file offset after it {(a[4] + 1 - DATA_START_LBA) * 512})")
        print(f"    first read after : t={b[0]} lba {b[3]} (file offset {(b[3] - DATA_START_LBA) * 512}); "
              f"sectors from expected next: {b[3] - (a[4] + 1)}")
        if during:
            print(f"    during: consumed {during[0][2]} -> {during[-1][2]}, live {during[0][1]} -> {during[-1][1]}, "
                  f"lead {during[0][1] - during[0][2]} -> {during[-1][1] - during[-1][2]}, "
                  f"open={sorted({s[3] for s in during})[:3]}")
            frozen = next((s for k, s in enumerate(during) if all(x[1] == s[1] for x in during[k:])), None)
            if frozen and during[-1][0] - frozen[0] >= 2000:
                print(f"    live stops advancing from about t={frozen[0]}")
        print("    raw lines around the transitions:")
        for lo, hi in ((i - 2, i + 1), (j - 1, j + 3)):
            for t, line in rows[max(lo, 0):hi]:
                print("      ", short(line)[:170])
            print("       ...")
    if status:
        print(f"last S: live={status[-1][1]} consumed={status[-1][2]} lead={status[-1][1] - status[-1][2]}")


if __name__ == "__main__":
    main(sys.argv[1], int(sys.argv[2]), int(sys.argv[3]) if len(sys.argv) > 3 else 10**12)
