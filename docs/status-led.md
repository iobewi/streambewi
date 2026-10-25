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

Levels: 16/255 for white, blue and green; 24/255 for orange and red.

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

- A fatal startup error (`boot_result`) loops without yielding, so the LED task cannot run: the LED
  stays at the white of `Booting`.
- No "degraded stream" state (no failure counter) and no "player is listening" state.
- Nothing is written to flash for a change of status.
