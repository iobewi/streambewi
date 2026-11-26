"""A2 - boot et console du firmware classique (sans OTA).

Dix `console.reset` consecutifs. Pour chaque demarrage, on verifie sur la capture UART :
- le bootloader charge l'application `factory` a 0x50000 ;
- le point d'acces de configuration est annonce (flash sans identifiants Wi-Fi) ;
- aucun panic, abort, backtrace ni redemarrage inattendu pendant la fenetre d'observation.
"""
import time

CYCLES = 10
READY_TIMEOUT = 30.0
SETTLE = 3.0  # observation supplementaire apres le marqueur "pret"

READY = b"AP StreamBeWI-Setup ready"
LOADED = b"Loaded app from partition at offset 0x50000"
FORBIDDEN = (b"panic", b"PANIC", b"abort()", b"Backtrace", b"Guru Meditation", b"assert failed", b"rst:0x3 ", b"rst:0x7 ", b"rst:0x8 ", b"rst:0x9 ")


def _collect(board, cursor, seconds):
    data = b""
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        chunk, cursor, dropped = board.read_uart(cursor)
        assert dropped == 0, "UART bytes lost"
        data += chunk
        time.sleep(0.25)
    return data, cursor


def test_boot_cycles(board):
    results = []
    for n in range(1, CYCLES + 1):
        _, cursor, _ = board.read_uart()
        start = time.monotonic()
        board.reset()
        data, ready_after = b"", None
        while time.monotonic() - start < READY_TIMEOUT:
            chunk, cursor, dropped = board.read_uart(cursor)
            assert dropped == 0, "UART bytes lost"
            data += chunk
            if READY in data:
                ready_after = time.monotonic() - start
                break
            time.sleep(0.25)
        extra, cursor = _collect(board, cursor, SETTLE)
        data += extra
        text = data.decode("utf-8", "replace")
        results.append({
            "cycle": n,
            "ready_s": None if ready_after is None else round(ready_after, 2),
            "loaded_factory": LOADED in data,
            "forbidden": [m.decode() for m in FORBIDDEN if m in data],
            "boots_seen": data.count(b"2nd stage bootloader"),
        })
        print(results[-1])
        if ready_after is None:
            print("--- UART (cycle", n, ") ---\n", text[-1500:])
    bad = [r for r in results if r["ready_s"] is None or not r["loaded_factory"] or r["forbidden"] or r["boots_seen"] != 1]
    times = [r["ready_s"] for r in results if r["ready_s"] is not None]
    if times:
        print(f"ready time: min {min(times)} s, max {max(times)} s, mean {sum(times)/len(times):.2f} s")
    assert not bad, f"failing cycles: {bad}"
