#!/usr/bin/env python3
"""Offline analysis of the `mp3c` lines of a capture (#47): rereads, first MPEG frame, frame continuity.

    python3 tools/qual/mp3_capture_report.py UART_LOG [OUT_DIR]

With OUT_DIR, the bytes of each episode are also written as episode-N.bin (file sectors from 0).
It reports; whether the player takes the file is observed separately.
"""
import re
import sys
import zlib

SECTOR = 512
BITRATES = {3: [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
            2: [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160]}
RATES = {3: [44100, 48000, 32000], 2: [22050, 24000, 16000], 0: [11025, 12000, 8000]}
NAMES = {3: "MPEG-1", 2: "MPEG-2", 0: "MPEG-2.5"}


def header(d, i):
    """The Layer III frame header at offset i, or None."""
    if i + 4 > len(d) or d[i] != 0xFF or d[i + 1] & 0xE0 != 0xE0:
        return None
    version, layer = (d[i + 1] >> 3) & 3, (d[i + 1] >> 1) & 3
    bitrate, rate, padding = d[i + 2] >> 4, (d[i + 2] >> 2) & 3, (d[i + 2] >> 1) & 1
    if version == 1 or layer != 1 or bitrate in (0, 15) or rate == 3:
        return None
    kbps = BITRATES[3 if version == 3 else 2][bitrate]
    hz = RATES[version][rate]
    length = (144 if version == 3 else 72) * kbps * 1000 // hz + padding
    crc = not d[i + 1] & 1
    side = i + 4 + (2 if crc else 0)
    reservoir = None  # main_data_begin: bytes of earlier frames this frame needs
    if side + 2 <= len(d):
        reservoir = ((d[side] << 1) | (d[side + 1] >> 7)) if version == 3 else d[side]
    return {"version": version, "kbps": kbps, "hz": hz, "padding": padding, "crc": crc,
            "mode": d[i + 3] >> 6, "length": length, "reservoir": reservoir}


def chain(d, i):
    """Consecutive frames of one stream (same version and sample rate) from offset i, and where it ends."""
    frames, first = [], header(d, i)
    while (h := header(d, i)) and (h["version"], h["hz"]) == (first["version"], first["hz"]):
        frames.append((i, h))
        i += h["length"]
    return frames, i


def analyse(d):
    """The first run of frames, and whether it holds to the end of the data.

    A run is the earliest one with three headers in sequence, or two reaching the end of the data. Starting
    from the earliest keeps a later break visible. A lone header is reported as unconfirmed.
    """
    candidates = [i for i in range(max(len(d) - 3, 0)) if header(d, i)]
    for minimum in (3, 2):
        for i in candidates:
            frames, end = chain(d, i)
            complete = end + 4 > len(d)
            if len(frames) >= minimum or (len(frames) >= 2 and complete):
                return {"offset": i, "frames": frames, "end": end, "confirmed": True, "complete": complete}
    # Too little data for a second header: only a header whose frame runs past the data can be right.
    open_ = [i for i in candidates if i + header(d, i)["length"] + 4 > len(d)]
    return {"offset": None, "unconfirmed": open_, "confirmed": False}


def parse(text):
    episodes = {}
    for line in text.replace("\r", "").split("\n"):
        if m := re.search(r"mp3c E ep=(\d+) t=(\d+) base=(\S+) replaced=(\d+)", line):
            episodes[int(m[1])] = {"t": int(m[2]), "base": m[3], "replaced": int(m[4]), "reads": [], "data": {}, "summary": None}
        elif m := re.search(r"mp3c R ep=(\d+) n=(\d+) t=(\d+) idx=(\d+) crc=([0-9a-f]{8}) same=(\w+)", line):
            episodes[int(m[1])]["reads"].append((int(m[2]), int(m[3]), int(m[4]), int(m[5], 16), m[6]))
        elif m := re.search(r"mp3c D ep=(\d+) idx=(\d+) v=(\w+) off=(\d+) hex=([0-9a-f]{128})\b", line):
            episodes[int(m[1])]["data"].setdefault((int(m[2]), m[3]), {})[int(m[4])] = bytes.fromhex(m[5])
        elif m := re.search(r"mp3c Z ep=(\d+) (reads=.*)", line):
            episodes[int(m[1])]["summary"] = m[2].strip()
    return episodes


def report(n, e, out_dir=None):
    print(f"== episode {n}: t={e['t']} base={e['base']} replaced={e['replaced']}")
    print(f"   summary: {e['summary']}")
    sectors = {}
    for (idx, version), chunks in sorted(e["data"].items()):
        if sorted(chunks) != list(range(0, SECTOR, 64)):
            print(f"   sector {idx} ({version}): INCOMPLETE export, {len(chunks)}/8 chunks")
            continue
        raw = b"".join(chunks[o] for o in sorted(chunks))
        wanted = "first" if version == "first" else "NO"
        crc = next((r[3] for r in e["reads"] if r[2] == idx and r[4] == wanted), None)
        ok = crc is not None and zlib.crc32(raw) == crc
        if not ok:
            print(f"   sector {idx} ({version}): EXPORT CORRUPT or unrecorded (crc)")
        elif version == "first":
            sectors[idx] = raw
    print(f"   reads in order (sector:same, ms after the first): "
          + " ".join(f"{r[2]}:{r[4]}@{r[1] - e['t']}" for r in e["reads"]))
    rereads = [r for r in e["reads"] if r[4] != "first"]
    first_crc = {r[2]: r[3] for r in e["reads"] if r[4] == "first"}
    consistent = all((r[3] == first_crc.get(r[2])) == (r[4] == "yes") for r in rereads)
    print(f"   rereads: {len(rereads)}, differing: {sum(r[4] == 'NO' for r in rereads)}, CRC agrees with the verdicts: {consistent}")
    contiguous = b""
    k = 0
    while k in sectors:
        contiguous += sectors[k]
        k += 1
    print(f"   verified contiguous bytes from the start of the file: {len(contiguous)} ({k} sectors); other sectors: {sorted(i for i in sectors if i >= k)}")
    if not contiguous:
        return
    print(f"   first 16 bytes: {contiguous[:16].hex()} | all zero: {not any(contiguous)}")
    a = analyse(contiguous)
    if not a["confirmed"]:
        print(f"   no frame confirmed by a successor; headers whose frame runs past the data: "
              + str([(i, header(contiguous, i)['kbps'], header(contiguous, i)['hz']) for i in a["unconfirmed"]]))
    else:
        f = a["frames"]
        print(f"   first confirmed frame at byte {a['offset']} ; {len(f)} headers in sequence, chain "
              + ("reaches the end of the data" if a["complete"] else f"BREAKS at byte {a['end']}: {contiguous[a['end']:a['end'] + 8].hex()}"))
        print("   stream:", sorted({(NAMES[h['version']], h['hz'], h['kbps'], 'crc' if h['crc'] else 'nocrc', f"mode{h['mode']}") for _, h in f}))
        if not a["complete"]:
            after = analyse(contiguous[a["end"]:])
            print("   after the break:", f"frames again at byte {a['end'] + after['offset']}, {len(after['frames'])} in sequence, "
                  + ("to the end" if after["complete"] else "breaking again") if after["confirmed"] else "no confirmed frame")
        print("   frame offsets/lengths:", [(o, h["length"]) for o, h in f][:24])
        print("   main_data_begin of the first frames:", [h["reservoir"] for _, h in f][:8])
    if out_dir:
        open(f"{out_dir}/episode-{n}.bin", "wb").write(contiguous)


if __name__ == "__main__":
    found = parse(open(sys.argv[1], "rb").read().decode("utf-8", "replace"))
    if not found:
        sys.exit("no mp3c episode in this capture")
    for number, episode in sorted(found.items()):
        report(number, episode, sys.argv[2] if len(sys.argv) > 2 else None)
