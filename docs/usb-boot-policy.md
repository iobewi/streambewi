# Persisted USB boot policy

This is product policy, implemented in `app/src/boot_policy.rs`, shared by Improv
and recovery. It precedes StreamBeWI's migration onto the IOBEWI Board contract.
The legacy S3 target still exists and now binds this same policy; no Board/entry
migration is performed by this change.

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
| True | Mass storage: no JTAG constructor; OTG starts after stream prebuffer |
| Read error, malformed/oversized or unsupported version | Error logged; provisioning for this boot, no writes or erasure |

UART0 remains an Improv transport in both modes. In MSC mode the native serial
port is absent. The boot decision remains immutable even when persistence
changes. A restart or board power cycle reads the new flag; replug is a power
cycle only if that port is the board's sole power source. There is no hot handover.
The S3 target reads the flag before constructing either native USB controller.
The legacy target still has its existing hardcoded NVS partition; discovery by
label is supplied by the pending IOBEWI Board migration, not duplicated here.

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
3. Restart using the existing RTC ResetSystem adapter.

If step 1 fails, do not clear credentials or reset: report the error and require
release/repress to retry. If step 2 fails, report it and restart: false is durable,
so provisioning with residual credentials is a valid, recoverable state. The
mutex prevents provisioning from interleaving credentials/flag writes with this
sequence. The current mode never changes before reset.

## Console and panic

The legacy target no longer installs a physical console callback and removes
esp-println/esp-backtrace dependencies. Logging remains in the framework's bounded
ring; the panic handler silently halts without accessing USB/JTAG. This is a
firmware policy change: historical serial-monitor logs in the stage descriptions
are not an available console in this composition. Startup-fatal UART diagnostics
will come from IOBEWI Board startup during migration.

## Validation

Host tests cover absent/false/true, malformed/oversized and read failures with no
writes, combined budget admission, connection/credential/flag failures, restart
selection, reprovisioning retry, recovery ordering and both partial persistence
states, plus provisioning/recovery serialization. An additional test drives the
actual Improv command handler with the real Wi-Fi manager and a fake radio/backend,
checking error/success frames and response routing on both ports.

```sh
cargo +1.95.0 test -p streambewi-core -p streambewi --locked --target x86_64-unknown-linux-gnu
cargo +esp build -p streambewi-esp32 --release --locked \
  -Z build-std=core,alloc --target xtensa-esp32s3-none-elf
```

Hardware remains required for Improv provisioning, native USB enumeration, BOOT
hold/reset, RTC reset/PHY reinitialization and panic behavior. Build evidence is
not those hardware gates. Versioned dist images remain historical until rebuilt
and qualified; this change does not flash hardware or regenerate delivery images.
