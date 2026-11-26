"""A5 - niveau actif : catalogue de stations (ajout, selection, suppression, limite, persistance).

Ecrit dans la configuration en flash et coupe brievement le disque USB a chaque changement de station.
Requiert BOARD_URL et, pour la persistance, le contrôle IDLab (IDLAB_*). Un nettoyage final remet le catalogue
a son etat d'origine, meme en cas d'echec.
"""
import json
import os
import time
import urllib.parse

import pytest

from test_a5_api import BASE, call, state

pytestmark = pytest.mark.skipif(not BASE, reason="BOARD_URL not set")

FIP = "http://icecast.radiofrance.fr/fip-midfi.mp3"


def form(**kv):
    return urllib.parse.urlencode(kv)


def stations():
    return state()["catalog"]["stations"]


def wait_stream_ready(selected=None, timeout=45):
    end = time.monotonic() + timeout
    last = None
    while time.monotonic() < end:
        try:
            s = state()
            last = (s["health"], s["catalog"]["selected"])
            if s["health"]["stream"] == "ready" and s["health"]["usb"] == "ready" and (
                selected is None or s["catalog"]["selected"] == selected
            ):
                return s
        except Exception as e:  # the page may stall while the disk is detached
            last = repr(e)
        time.sleep(1)
    raise AssertionError(f"stream not ready for station {selected}: {last}")


@pytest.fixture
def pristine():
    original = state()["catalog"]
    assert [s["id"] for s in original["stations"]] == [1] and original["selected"] == 1, "unexpected starting catalog"
    yield original
    # cleanup: station 1 selected, every other station deleted
    try:
        if state()["catalog"]["selected"] != 1:
            call("POST", "/api/stations/select", form(id=1))
            wait_stream_ready(1)
        for st in state()["catalog"]["stations"]:
            if st["id"] != 1:
                call("POST", "/api/stations/delete", form(id=st["id"]))
    finally:
        final = state()["catalog"]
        assert [s["id"] for s in final["stations"]] == [1] and final["selected"] == 1, f"cleanup incomplete: {final}"


def add(name, url):
    status, body = call("POST", "/api/stations/save", form(name=name, url=url, logo=""))
    assert 200 <= status < 300, (status, body[:200])
    found = [s for s in stations() if s["name"] == name]
    assert len(found) == 1, f"station {name} not in the catalog"
    return found[0]["id"]


def test_add_select_switch_delete(pristine):
    sid = add("FIP test", FIP)
    cat = state()["catalog"]
    assert cat["version"] == 1, "`version` is the catalog format version (always 1), not a revision counter"
    assert next(s for s in cat["stations"] if s["id"] == sid)["url"] == FIP

    status, body = call("POST", "/api/stations/select", form(id=sid))
    assert 200 <= status < 300, (status, body[:200])
    s = wait_stream_ready(sid)
    assert s["persisted"] is True
    status, body = call("GET", "/api/logs")
    lines = [l["m"] for l in json.loads(body)["lines"]]
    # the firmware logs the host only (never the path): check the observable steps of a station change
    tail = lines[-12:]
    assert any("selection persisted=true" in m for m in tail), tail
    assert any("HTTP status=200" in m for m in tail), tail
    assert any("disk back after" in m for m in tail), tail

    status, _ = call("POST", "/api/stations/select", form(id=1))
    assert 200 <= status < 300
    wait_stream_ready(1)

    status, _ = call("POST", "/api/stations/delete", form(id=sid))
    assert 200 <= status < 300
    assert [s["id"] for s in stations()] == [1]


def test_catalog_limit_is_eight(pristine):
    ids = [add(f"limit {i}", f"http://example.org/limit{i}.mp3") for i in range(1, 8)]
    assert len(stations()) == 8
    before = state()["catalog"]
    status, _ = call("POST", "/api/stations/save", form(name="ninth", url="http://example.org/ninth.mp3", logo=""))
    assert status >= 400, f"the ninth station was accepted (status {status})"
    assert state()["catalog"] == before
    for sid in ids:
        status, _ = call("POST", "/api/stations/delete", form(id=sid))
        assert 200 <= status < 300


def test_deleting_the_selected_station_is_handled(pristine):
    # never delete station 1 here: it is the one the bench must keep
    sid = add("selected then deleted", FIP)
    status, _ = call("POST", "/api/stations/select", form(id=sid))
    assert 200 <= status < 300
    wait_stream_ready(sid)
    status, body = call("POST", "/api/stations/delete", form(id=sid))
    after = state()["catalog"]
    print("delete of the selected station ->", status, body[:120], "catalog:", after)
    assert status < 500
    assert sid not in [s["id"] for s in after["stations"]] or status >= 400
    assert after["selected"] in [s["id"] for s in after["stations"]], "selected station is dangling"


def test_catalog_survives_a_restart(pristine, board):
    sid = add("persist test", "http://example.org/persist.mp3")
    board.reset()
    s = wait_stream_ready(timeout=90)
    assert any(st["id"] == sid and st["name"] == "persist test" for st in s["catalog"]["stations"]), "station lost after restart"
    assert s["catalog"]["selected"] == 1
