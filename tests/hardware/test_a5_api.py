"""A5 - API locale et interface web du firmware classique (niveau lecture seule).

Cible : BOARD_URL (adresse HTTP de la carte, ex. http://<ip-de-la-carte>). Aucune ecriture reussie n'est attendue : seules des requetes
invalides sont envoyees, et elles doivent etre refusees sans changer le catalogue.
Pas de POST /wifi : accepte par erreur, il changerait le Wi-Fi de la carte.
"""
import json
import os
import time
import urllib.error
import urllib.request

import pytest

BASE = os.environ.get("BOARD_URL", "").rstrip("/")
pytestmark = pytest.mark.skipif(not BASE, reason="BOARD_URL not set")


def call(method, path, body=None, timeout=10):
    data = body.encode() if isinstance(body, str) else body
    req = urllib.request.Request(BASE + path, data=data, method=method)
    if data is not None:
        req.add_header("Content-Type", "application/x-www-form-urlencoded")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def state():
    status, body = call("GET", "/api/state")
    assert status == 200
    return json.loads(body)


def test_state_schema_and_health():
    s = state()
    for key in ("catalog", "health", "ip", "led", "online", "persisted", "ready", "setup"):
        assert key in s, key
    assert s["health"] == {"stream": "ready", "system": "running", "usb": "ready", "wifi": "connected"}
    assert s["setup"] is False and s["persisted"] is True and s["online"] is True
    cat = s["catalog"]
    assert any(st["id"] == cat["selected"] for st in cat["stations"]), "selected station is not in the catalog"
    assert len(cat["stations"]) <= 8


def test_state_100_reads():
    worst = 0.0
    for _ in range(100):
        t = time.monotonic()
        status, _ = call("GET", "/api/state")
        worst = max(worst, time.monotonic() - t)
        assert status == 200
    print(f"worst /api/state latency over 100 reads: {worst*1000:.0f} ms")
    assert worst < 3.0


def test_logs_endpoint():
    status, body = call("GET", "/api/logs")
    assert status == 200
    j = json.loads(body)
    assert isinstance(j["lines"], list) and j["capacity"] >= len(j["lines"])
    assert all({"l", "m"} <= set(line) for line in j["lines"])
    status, body = call("GET", "/api/logs?n=2")
    assert status == 200 and len(json.loads(body)["lines"]) <= 2


@pytest.mark.parametrize("path", ["/api/logs?n=abc", "/api/logs?after=abc", "/api/logs?after=99999999999"])
def test_logs_bad_query_never_5xx(path):
    status, _ = call("GET", path)
    assert status < 500, status


def test_log_config_readable():
    status, body = call("GET", "/api/logs/config")
    assert status == 200 and body


def test_web_page():
    status, body = call("GET", "/", timeout=15)
    assert status == 200
    html = body.decode("utf-8", "replace")
    assert "<title>StreamBeWI" in html and "/api/state" in html


@pytest.mark.parametrize("method,path", [
    ("GET", "/api/stations/save"), ("GET", "/api/stations/select"), ("PUT", "/api/stations/delete"),
    ("DELETE", "/api/stations/save"), ("POST", "/api/stations/foo"), ("GET", "/api/stations/foo"),
    ("POST", "/api/stationsXYZ"), ("GET", "/no-such-page"),
])
def test_wrong_method_or_route_is_refused(method, path):
    status, _ = call(method, path, "" if method in ("POST", "PUT") else None)
    assert 400 <= status < 500, status


@pytest.mark.parametrize("path,form", [
    ("/api/stations/save", "name=&url="),
    ("/api/stations/save", "bogus=1"),
    ("/api/stations/save", "name=X&url=ftp%3A%2F%2Fexample.org%2Fa.mp3"),
    ("/api/stations/select", "id=99"),
    ("/api/stations/select", "id=abc"),
    ("/api/stations/delete", "id=99"),
])
def test_invalid_station_requests_are_refused_and_change_nothing(path, form):
    before = state()["catalog"]
    status, _ = call("POST", path, form)
    assert 400 <= status < 500, status
    assert state()["catalog"] == before
