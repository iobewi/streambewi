"""A4 - flux HTTPS : poignee de main TLS, lecture soutenue par le Metronic, sante stable.

Utilise BOARD_URL et HTTPS_STREAM (URL https d'un flux MP3). Le nettoyage de test_a5_active.pristine remet
la station 1 a la fin, meme en cas d'echec.
"""
import json
import os
import re
import time

import pytest

from test_a5_active import add, call, form, stations, wait_stream_ready
from test_a5_api import BASE, state

HTTPS_STREAM = os.environ.get("HTTPS_STREAM", "")
pytestmark = pytest.mark.skipif(not (BASE and HTTPS_STREAM), reason="BOARD_URL / HTTPS_STREAM not set")

SAMPLE_SECONDS = 150


def log_lines():
    status, body = call("GET", "/api/logs")
    assert status == 200
    return [l["m"] for l in json.loads(body)["lines"]]


@pytest.fixture
def restore():
    """Keep whatever the operator put in the catalog: restore the original selection, delete only what we add."""
    original = state()["catalog"]
    ids_before = {s["id"] for s in original["stations"]}
    yield original
    try:
        if state()["catalog"]["selected"] != original["selected"]:
            call("POST", "/api/stations/select", form(id=original["selected"]))
            wait_stream_ready(original["selected"], timeout=60)
        for st in state()["catalog"]["stations"]:
            if st["id"] not in ids_before:
                call("POST", "/api/stations/delete", form(id=st["id"]))
    finally:
        final = state()["catalog"]
        assert final["selected"] == original["selected"]
        assert {s["id"] for s in final["stations"]} == ids_before, f"cleanup incomplete: {final}"


def test_https_stream_plays_and_stays_stable(restore):
    assert HTTPS_STREAM.startswith("https://")
    existing = [s for s in restore["stations"] if s["url"] == HTTPS_STREAM]
    sid = existing[0]["id"] if existing else add("HTTPS test", HTTPS_STREAM)
    print("using station", sid, "(already in the catalog)" if existing else "(added by the test)")
    status, body = call("POST", "/api/stations/select", form(id=sid))
    assert 200 <= status < 300, (status, body[:200])
    wait_stream_ready(sid, timeout=60)

    tail = log_lines()
    print("\n".join(tail[-14:]))
    # production log (Info/Warn): no "tls up in N ms" (instrumented builds only); the scheme is in "connecting"
    assert any(re.search(r"connecting attempt=\d+ to https://", m) for m in tail), "no HTTPS connection in the log"
    assert any("HTTP status=200" in m for m in tail), "no HTTP 200"
    assert any("disk back after" in m for m in tail), "disk did not come back"

    # sustained playback: consumed must keep advancing, lead must stay bounded, health must stay nominal
    samples, seen = [], set()
    end = time.monotonic() + SAMPLE_SECONDS
    while time.monotonic() < end:
        s = state()
        assert s["health"] == {"stream": "ready", "system": "running", "usb": "ready", "wifi": "connected"}, s["health"]
        for m in log_lines():
            g = re.search(r"live=(\d+) consumed=(\d+) lead=(\d+)", m)
            if g and g.group(0) not in seen:
                seen.add(g.group(0))
                samples.append(tuple(map(int, g.groups())))
        low = " ".join(log_lines()).lower()
        for bad in ("panic", "stall", "zerofill", "expired", "pending"):
            assert bad not in low, f"'{bad}' seen in the log"
        time.sleep(10)
    print("samples (live, consumed, lead):", samples)
    assert len(samples) >= 2, "fewer than two counter lines in the observation window"
    assert samples[-1][1] > samples[0][1], "consumed did not advance"
    assert all(lead <= 81920 for _, _, lead in samples), "lead above the 80 KiB bound"
