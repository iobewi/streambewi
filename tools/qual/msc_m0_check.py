#!/usr/bin/env python3
"""M0 gate of the MSC recipe (#47): is the trace complete, and does it leave the MSC alone?

    python3 tools/qual/msc_m0_check.py CAPTURE_DIR FROM_MS TO_MS

FROM_MS and TO_MS are device times (the `t=` of the `msct` lines). The thresholds are those of
docs/qualification-47/README.md, fixed before the measurement. The sound is judged by the operator only.
"""
import os
import re
import sys

NOMINAL_BYTES_PER_S = 16_000  # 128 kbit/s
RATE_TOLERANCE = 0.05
MAX_S_INTERVAL_MS = 1_500
MAX_TX_US_PER_WINDOW = 100_000
MAX_TX_US_PER_LINE = 20_000

STATUS = re.compile(
    r"msct S t=(\d+) live=(\d+) consumed=(\d+) open=\S+ lost=(\d+) lines=(\d+) tx_us=(\d+) txmax_us=(\d+)"
)
ANY_TIME = re.compile(r"msct [RCBS] .*?\bt=(\d+)")


def main(capture, start, end):
    text = open(os.path.join(capture, "uart.log"), "rb").read().decode("utf-8", "replace")
    lines = text.splitlines()
    inside, now = [], None
    for line in lines:
        match = ANY_TIME.search(line)
        if match:
            now = int(match.group(1))
        if now is not None and start <= now <= end:
            inside.append(line)
    status = [tuple(map(int, m.groups())) for line in inside if (m := STATUS.search(line))]
    if len(status) < 2:
        sys.exit("fewer than two S lines in the window: nothing can be concluded")
    events = open(os.path.join(capture, "events.log")).read()
    span_s = (status[-1][0] - status[0][0]) / 1000
    rate = (status[-1][2] - status[0][2]) / span_s
    intervals = [b[0] - a[0] for a, b in zip(status, status[1:])]
    reads = [line for line in inside if "msct R " in line]
    field = lambda name, rows: [int(m.group(1)) for r in rows if (m := re.search(rf" {name}=(\d+)", r))]
    checks = [
        ("window of at least 60 s", span_s >= 59, f"{span_s:.1f} s"),
        ("no line lost on the device", max(s[3] for s in status) == 0, f"lost={max(s[3] for s in status)}"),
        ("no byte dropped by the bench (whole capture)", "DROPPED" not in events, ""),
        ("no session error (whole capture)", "session error" not in events, ""),
        ("S lines without a hole", max(intervals) <= MAX_S_INTERVAL_MS, f"max {max(intervals)} ms"),
        ("no ZEROFILL", not any("ZEROFILL" in l for l in inside), ""),
        ("no stall", not any("leaving the bus" in l for l in inside), ""),
        ("no USB bus event", not any("msct B " in l for l in inside), ""),
        ("no failed or aborted command", not any(re.search(r" st=([1-9]\d*|abort)", l) for l in inside), ""),
        ("consumption at the audio rate", abs(rate / NOMINAL_BYTES_PER_S - 1) <= RATE_TOLERANCE, f"{rate:.0f} B/s"),
        ("trace cost per window", max(s[5] for s in status) <= MAX_TX_US_PER_WINDOW,
         f"max {max(s[5] for s in status)} us, mean {sum(s[5] for s in status) / len(status):.0f} us"),
        ("longest console write", max(s[6] for s in status) <= MAX_TX_US_PER_LINE, f"{max(s[6] for s in status)} us"),
    ]
    for name, ok, detail in checks:
        print(f"{'PASS' if ok else 'FAIL'}  {name}  {detail}")
    print(f"info  READ(10) lines={len(reads)} longest silence={max(field('gap', reads), default=0)} ms "
          f"longest service={max(field('svc', reads), default=0)} ms")
    sys.exit(0 if all(ok for _, ok, _ in checks) else 1)


if __name__ == "__main__":
    main(sys.argv[1], int(sys.argv[2]), int(sys.argv[3]))
