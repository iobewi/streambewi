# Status LED (#3)

StreamBeWI shows its main states on the RGB LED of its board. The LED only **observes** the product:
nothing in the Wi-Fi, stream, rolling buffer, USB MSC or provisioning behaviour depends on it, and a
build without a board feature has no LED and does not touch the RMT.

```text
Wi-Fi ──────┐
Stream ─────┤  replace()                                   RMT
UsbMsc ─────┼──▶ iobewi-state ──▶ look() ──▶ iobewi-indicator ──▶ iobewi-esp-ws2812 ──▶ LED
System ─────┘     (registry)     (app/src/status.rs)
```

`app/src/status.rs` holds the policy and the LED task, with no hardware in it: it is tested on the
host. The target (`targets/esp32`) only supplies the RMT, the LED pin and the WS2812 driver.

## Sources and flags

Each source publishes what it knows of itself, at a boundary the product already had. A transition
**replaces** the whole set of its source, so a flag that stopped being true goes with it. None is
published per sector, per chunk or from the MSC read path.

| Source | Flags | Published |
| --- | --- | --- |
| System | `Booting` | `status::init()`, first thing in the target's `product` task |
| | `Configuring` / `Running` | start of `run`: provisioning boot / mass-storage boot |
| Wi-Fi | `Connecting` | start of each `maintain` round (`wifi_task`), and when an Improv or web request takes over |
| | `Connected` | `LinkObserver::ready` |
| | `Reconnecting` | `LinkObserver::link_down` |
| | `Unconfigured` | `maintain` returned `NotProvisioned` |
| Stream | `Connecting` / `Reconnecting` | `produce`: first attempt of a station / every later one, and as soon as an attempt returns |
| | `Connected` | the station answered 200 |
| | `Connected`, `Ready` | this connection delivered the prebuffer (`PREBUFFER_BYTES`): once, in `pump` |
| | `Busy` | a station change took over (`SWITCH`) |
| | `Warning` | the selection could not be saved; cleared by a successful save, kept across connection changes |
| UsbMsc | `Initializing` | `serve` waits for the prebuffer |
| | `Ready` | MSC enabled; disk back on the bus (stall recovery, station change) |
| | `Unavailable` | disk off the bus on purpose (stall recovery, station change) |
| | `Error` | the mass-storage class ended on an endpoint error |

`Critical` and `Degraded` are read by the policy but nothing publishes them yet.

## Policy

The first rule that matches wins (`status::look`). Rules 6 and 7 read the Wi-Fi and Stream sources
themselves, not the aggregate.

| # | Condition | LED |
| --- | --- | --- |
| 1 | `Critical` or `Error` (any source) | red blinking, 250/250 ms |
| 2 | `Degraded` or `Warning` (any source) | orange steady |
| 3 | `Configuring` or `Unconfigured` | blue blinking, 250/250 ms |
| 4 | Wi-Fi `Connecting` or `Reconnecting` | blue blinking slowly, 1000/1000 ms |
| 5 | `Booting` | white steady |
| 6 | Wi-Fi `Connected` and stream `Ready` | green steady |
| 7 | Wi-Fi `Connected`, stream not ready | green pulsing, 4 s |
| 8 | anything else | off |

The colours are stored at their full value and the brightness setting (below) is the only attenuation, applied once by the
indicator's master level. At the default brightness (level 23) white, blue and green show 16/255 and orange and red
24/255, to within 1/255, as before the setting existed; the ratio between them is kept at every brightness.

`Busy` and `Unavailable` have no look of their own. A station change only makes the stream not
ready (rule 7 shows until the new station has delivered its prebuffer); the disk leaving the bus
changes nothing.

**Green means the Wi-Fi is connected and the stream delivered its prebuffer. It does not say that the
player is listening**: the product has no event for that.

## Boards

The LED pin is chosen at build time. At most one board feature may be selected; without one there is
no LED.

| Feature | Board | LED |
| --- | --- | --- |
| `board-devkitc-1` | ESP32-S3-DevKitC-1 v1.0 | GPIO48 |
| `board-devkitc-1-v1-1` | ESP32-S3-DevKitC-1 v1.1 | GPIO38 |
| `board-s3-zero` | Waveshare ESP32-S3-Zero | GPIO21 |

GPIO48 is qualified on a DevKitC-1 N16R8. GPIO38 and GPIO21 come from the manufacturers'
documentation ([DevKitC-1](https://docs.espressif.com/projects/esp-dev-kits/en/latest/esp32s3/esp32-s3-devkitc-1/user_guide.html),
[Waveshare ESP32-S3-Zero](https://www.waveshare.com/wiki/ESP32-S3-Zero)) and are not verified on a
board. None of the three pins is used by the product (GPIO0, 19, 20, 43 and 44 are).

```sh
FEATURES=board-devkitc-1 scripts/build-image.sh OUT_DIR
```

## Task and failure

The LED is a separate Embassy task (`status_led` in the target), woken by the registry and by the
deadlines of the indicator; nothing polls. A change that leaves the look the same writes nothing and
does not restart the animation. A write error ends that task alone: the product goes on.

The WS2812 write is a blocking RMT transmission of one LED.

## Limits

- The LED stays dark until the saved brightness is read, right after the configuration is mounted. A fatal
  startup error before that (`boot_result` loops without yielding, in any case) leaves it dark; one after it
  leaves the look of `Booting`.
- No "degraded stream" state (no failure counter) and no "player is listening" state.
- Nothing is written to flash for a change of status.

## Brightness setting

The "Luminosité de la LED" slider of the settings panel (the gear) sets the brightness from 0 to 100 %.
0 % is off, 100 % is level 64/255 (`brightness::MAX_LEVEL`), the level is `percent * 64 / 100` rounded down, linear,
no gamma. The default, with nothing saved, is 36 % (level 23), what the LED showed before the setting. Nothing in
the look (patterns, colours' proportions, priorities) depends on it. The control is hidden when the firmware has no LED
(`led.available` is false in `/api/state`).

Three values are kept apart (`app/src/brightness.rs`): **saved** (in the configuration), **previewed** (RAM only) and
**applied** (what the LED shows: the preview while there is one, else the saved value, else dark while the saved value is
not known yet).

| Request | Effect |
| --- | --- |
| `POST /api/led/preview` `percent=0..100` | applies the value at once; it lasts 60 s, renewed by every request. Nothing is written, the disk is not touched |
| `POST /api/led/cancel` | back to the saved value |
| `POST /api/led/save` `percent=0..100` | answers `202` at once and asks for a protected save; the write is done later |
| `GET /api/state` | `led`: `available`, `saved`, `preview`, `applied`, `busy` (a save is asked for or being written), `error` |

The page sends at most one preview at a time, 150 ms apart, and always the last value after the slider is released. It says
"Enregistré !" only when `/api/state` shows the saved value equal to the request, no save busy and no error. When the panel
is closed the page cancels the preview, but the firmware does not depend on it: the preview ends by itself after 60 s.

A failed save leaves the saved value, tells the page (`led.error`) and keeps the preview until it ends or is cancelled, so the
page can offer to try again. It does not raise the orange `Warning` of an unsaved station.

### Configuration

The value is kept through `iobewi-config-space` only, in a new `settings` space (`app/src/settings.rs`) that holds one small
record of simple settings: `SET1` then entries `id, length, value` (id 1: LED brightness, one byte, 0 to 100). An entry this
firmware does not know is kept when the record is rewritten, so another setting can be added without a space of its own.
A bad value counts as absent (the default applies), a record that is not a `SET1` record counts as empty; loading never
writes and nothing is erased. `usb_boot`, `wifi`, `stations` and `log` are not changed, nor their budgets.

**Admission**: a space reserves at least 8 pages whatever its size. With `settings` the manifest reserves 48 of the 48 pages
the 64-page `config` partition can admit (`stations` 16, `wifi` 8, `usb_boot` 8, `log` 8, `settings` 8): **there was no room
left for another space** (the OTA layout, `docs/ota.md`, gives `config` 128 pages and adds the `ota` space). The
partition table of that time is not changed in this paragraph.

**Compatibility** (tested over a real EKV store): a partition that holds the four earlier spaces mounts with the five-space
manifest without formatting or writing, and every byte of the earlier spaces is kept. **The other way is not possible**: once
the settings are saved, a firmware that does not know the `settings` space refuses to mount the store (`InvalidSpace`) and
halts at `STREAMBEWI:CONFIG-MOUNT` without reformatting. Nothing is lost, the newer firmware mounts it again, but going back to
an earlier firmware needs the configuration to be written again.

### Protected save

The write goes through `stream::save_settings`, one save at a time, in the order asked. With no disk (provisioning boot), or a
disk not presented yet, it is written at once. With the disk on the bus it is the detach window of a station change, without
the change of station: the disk leaves the bus, the write waits for 300 ms without a read (at most 2 s), the settings are
written, the disk stays off for 1.5 s since it left and comes back. The stream connection is not touched and the session is
held across the reconnection, as a stall recovery does; the music stops for the length of the window. A save asked for while
another actor (a station change, a stall recovery) has the disk off the bus waits for it; a station change that takes over
during the window keeps the disk and brings it back itself.
