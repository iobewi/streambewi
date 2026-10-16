# Persisted USB boot policy

This is product policy, implemented in `app/src/boot_policy.rs`, shared by Improv
and recovery. The concrete S3 target reads it before constructing USB-JTAG or OTG.

## Configuration and startup

A single ConfigManager claims `wifi` (the Wi-Fi manager's existing 128-byte
budget) and `usb_boot` (5 bytes). Admission failure stops startup. The USB space
encodes the product-owned `otg_enabled` flag as exactly `USB1` followed by byte
0 or 1; absent values are valid first boot, other encodings are errors. No schema
is imposed on ConfigSpace itself. Old firmware with credentials but no USB flag
boots into provisioning; reprovision once to publish the flag.

| Flag read at boot | Native USB mode |
| --- | --- |
| Absent or false | Provisioning: JTAG initialized, no OTG constructor |
| True | Mass storage: no JTAG constructor; OTG constructed at boot; enumeration waits for prebuffer |
| Read error, malformed/oversized or unsupported version | Error logged; provisioning for this boot, no writes or erasure |

UART0 remains an Improv transport in both modes. In MSC mode the native serial
port is absent. The boot decision remains immutable even when persistence
changes. A restart or board power cycle reads the new flag; replug is a power
cycle only if that port is the board's sole power source. There is no hot handover.
The target mounts one EKV Store on the `config` data/0x40 partition discovered
by label, with one shared physical flash owner. NVS is not migrated automatically.
Only a wholly blank region is explicitly initialized; invalid nonblank storage
halts with a pre-Improv UART diagnostic and is never reformatted.

## Provisioning and partial states

The Wi-Fi manager connects and commits credentials first. Only after it reports
success does BootPolicy commit true. These are separate writes, not an atomic
transaction. A mutex serializes this sequence with recovery.

If connection or credential persistence fails, the flag is not written. If the
flag fails after saving credentials, Improv reports an error and returns to
Authorized; it does not send a successful WifiSettings RPC response. The device
stays in its current USB mode, with saved credentials. Reprovisioning retries both
writes. The mutex is local to one cooperative product executor, not a multicore
flash lock. A successful request replies Provisioned/success on the requesting port
and requires a user restart to enter MSC; it never resets automatically.
The current improv-serial revision only provides UnableToConnect for provisioning
failures, so flag persistence failure uses that error frame, with its distinct
cause recorded in the product log.

## Recovery

Hold BOOT low for 3 seconds while firmware is running (holding the S3 strapping
pin at reset can enter ROM download mode).

1. Commit false to usb_boot.
2. Clear Wi-Fi credentials.
3. Invoke the target-provided reset callback, which arms RTC ResetSystem on S3.

If step 1 fails, do not clear credentials or reset: report the error and require
release/repress to retry. If step 2 fails, report it and restart: false is durable,
so provisioning with residual credentials is a valid, recoverable state. The
mutex prevents provisioning from interleaving credentials/flag writes with this
sequence. The current mode never changes before reset.

## Console and panic

The target installs `iobewi-log` once before configuration preparation, with a
no-op console callback. Normal product logs remain in the bounded RAM ring and
are not written onto UART0 or USB-JTAG, which carry Improv. The S3 panic handler
halts without touching these transports. Config startup failures have best-effort
UART diagnostics before Improv begins; the corresponding error is captured in
RAM. There is no ring consumer in this composition yet (`log-stream` is tracked
in #3). The product target is captured at Info, other targets at Warn and above.
Historical serial-monitor logs below describe earlier compositions.

## Validation

Host tests cover absent/false/true, malformed/oversized and read failures with no
writes, combined budget admission, connection/credential/flag failures, restart
selection, reprovisioning retry, recovery ordering and both partial persistence
states, plus provisioning/recovery serialization. An additional test drives the
actual Improv command handler with the real Wi-Fi manager and a fake radio/backend,
checking error/success frames and response routing on three ports, finite serial iterators of zero and three ports.

```sh
cargo +stable test -p streambewi-core -p streambewi --locked --target x86_64-unknown-linux-gnu
cargo +esp build -p streambewi-esp32 --release --locked \
  -Z build-std=core,alloc --target xtensa-esp32s3-none-elf
```

Hardware remains required for Improv provisioning, native USB enumeration, BOOT
hold/reset, RTC reset/PHY reinitialization and panic behavior. Build evidence is
not those hardware gates. Versioned dist images remain historical until rebuilt
and qualified; this change does not flash hardware or regenerate delivery images.

## Historical flash-fix baseline (2026-10-06)

That historical composition pinned all IOBEWI Git dependencies to
`596d180a3188823b125ede3444ca013ab62558e9` (merged PR #26). This restores
`esp-storage/critical-section` in the shared flash and partition adapters without
product-side dependency patches. The old `ddfa839` branch-only Wi-Fi change
(keeping an already-live association on repeated connect) is not in this main
revision. It is not silently reintroduced by this pin update.

Human-reported image 13A used product `03bef9b` against IOBEWI `26fbd2f`
through local Git-source patches. Provisioning, restart/power-cycle persistence,
ten minutes of Metronic playback and three recovery/reprovisioning/MSC cycles
succeeded. Its merged-image SHA-256 was
`7167978b1d3c6e34b42e7c2ba30695106433aba5ffb6d08202b35dfca0f84b02`.
Those observations describe that image; they do not qualify a new binary built
from this published pin. Error injection, panic in OTG, constructor counts and
separate RTC/system/replug reset identification remain open.

For this baseline, replay provisioning -> restart -> MSC -> long-BOOT recovery
-> reprovisioning on the normal CI-produced merged image. Record its product
SHA and image hash. No repeated-write stress campaign is needed for this product
replay. Flash latency and OTA limitations are recorded in IOBEWI PR #26; this
baseline does not declare global flash/OTA hardware acceptance.
