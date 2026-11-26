"""A7 - LED d'etat (image construite avec la feature `board-devkitc-1`, GPIO48).

Verifie par l'API et le journal : disponibilite, apercu/annulation/enregistrement de la luminosite, refus des
valeurs invalides, enchaînement des etats lors d'un changement de station. Les couleurs reellement affichees
sont a confirmer a l'oeil. Le nettoyage remet la luminosite enregistree d'origine et annule tout apercu.
"""
import json
import time

import pytest

from test_a5_active import call, form, wait_stream_ready
from test_a5_api import BASE, state

pytestmark = pytest.mark.skipif(not BASE, reason="BOARD_URL not set")


def led():
    return state()["led"]


def wait_led(pred, timeout=15):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if pred(led()):
            return led()
        time.sleep(0.3)
    raise AssertionError(f"led state not reached: {led()}")


@pytest.fixture
def led_restore():
    original = led()
    assert original["available"] is True, "LED not available: image built without a board feature?"
    yield original
    call("POST", "/api/led/cancel", "")
    if led()["saved"] != original["saved"]:
        call("POST", "/api/led/save", form(percent=original["saved"]))
        wait_led(lambda l: l["saved"] == original["saved"] and not l["busy"], 30)
    final = led()
    assert final["saved"] == original["saved"] and final["preview"] is None and final["error"] is None, final


def test_led_is_available(led_restore):
    l = led_restore
    assert l["error"] is None and l["busy"] is False
    assert 0 <= l["saved"] <= 100 and l["applied"] == l["saved"] and l["preview"] is None


def test_preview_and_cancel_do_not_write_or_touch_the_disk(led_restore):
    saved = led_restore["saved"]
    target = 80 if saved != 80 else 20
    status, _ = call("POST", "/api/led/preview", form(percent=target))
    assert 200 <= status < 300, status
    l = wait_led(lambda l: l["preview"] == target)
    assert l["applied"] == target and l["saved"] == saved
    s = state()
    assert s["health"]["usb"] == "ready" and s["health"]["stream"] == "ready"
    status, _ = call("POST", "/api/led/cancel", "")
    assert 200 <= status < 300
    l = wait_led(lambda l: l["preview"] is None)
    assert l["applied"] == saved and l["saved"] == saved


@pytest.mark.parametrize("percent", ["101", "-1", "abc", ""])
def test_invalid_percent_is_refused(led_restore, percent):
    saved = led_restore["saved"]
    status, _ = call("POST", "/api/led/preview", form(percent=percent))
    assert 400 <= status < 500, status
    status, _ = call("POST", "/api/led/save", form(percent=percent))
    assert 400 <= status < 500, status
    l = led()
    assert l["saved"] == saved and l["preview"] is None


def test_save_persists_and_is_restored(led_restore):
    saved = led_restore["saved"]
    target = 50 if saved != 50 else 30
    status, _ = call("POST", "/api/led/save", form(percent=target))
    assert status == 202, status
    l = wait_led(lambda l: l["saved"] == target and not l["busy"], 30)
    assert l["error"] is None and l["applied"] == target
    # the fixture restores the original value and checks it


def test_station_change_walks_through_green_states(led_restore):
    s = state()["catalog"]
    others = [x["id"] for x in s["stations"] if x["id"] != s["selected"]]
    if not others:
        pytest.skip("only one station in the catalog")
    first = s["selected"]
    try:
        status, _ = call("POST", "/api/stations/select", form(id=others[0]))
        assert 200 <= status < 300
        wait_stream_ready(others[0], timeout=60)
        status, body = call("GET", "/api/logs")
        lines = [l["m"] for l in json.loads(body)["lines"]]
        leds = [m for m in lines if "status: LED" in m]
        print("LED lines after the station change:", leds[-4:])
        assert any("green pulsing" in m for m in leds), leds
        assert "green steady" in leds[-1], leds
    finally:
        call("POST", "/api/stations/select", form(id=first))
        wait_stream_ready(first, timeout=60)
