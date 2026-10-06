# StreamBeWI Board / entry migration

## Scope and dependency

The ESP target invokes `iobewi_entry::entry!(streambewi::run)` and calls
`iobewi_entry_build::emit()` from build.rs. It contains no peripheral initialization,
GPIO selection, NVS address, heap initialization or task wrapper. The product
consumes `Board::into_parts` once and adds its own Embassy network bound.

Every IOBEWI dependency uses published commit
`f0360f51a61c2a475924ab4633b77757b2a832b7`, from the candidate
[IOBEWI PR #24](https://github.com/iobewi/iobewi/pull/24). No local source patches
are used. That framework PR must be accepted before this migration is merged;
this document does not declare `entry!` hardware acceptance.

IOBEWI owns the S3 profile (BOOT, UART0, native USB), max CPU clock, allocator,
shared flash, label-based NVS discovery, Wi-Fi startup, identity and reset.
The flash critical-section fix remains enabled by that revision. StreamBeWI owns
configuration admission, flag interpretation, Improv response routing, stream
prebuffer and recovery. The persisted schema and write ordering are unchanged.

The product reads the flag before `BootIoFactory::select`. An absent, false or
unreadable flag selects provisioning; true selects MSC. The actual OTG driver
is now constructed during this selection, while enumeration still waits for
configuration and prebuffer. This timing difference requires a new hardware
replay. No runtime USB handover is introduced.

## Finite serial bank

The product consumes all available RX/TX pairs, preserving bank order. Transmit
halves are owned by the Wi-Fi/Improv handler; receive halves are independently
polled. There is no two-port assumption or synthetic absent port. An empty bank
stays pending without spinning. Each reader future is separately boxed on the
board heap, so the exported product future size excludes those allocations.
EOF and zero-length writes yield or stop respectively instead of busy-looping.

Host tests exercise zero and three ports and the real Improv handler's error,
retry and successful response on each of three ports. The original eleven boot
policy tests and six core tests still pass (19 total).

## Actual ESP release measurements

Measured locally with `cargo +esp build --locked -p streambewi-esp32 --release
-Z build-std=core,alloc --target xtensa-esp32s3-none-elf`, using the real product
join of provisioning, recovery, streaming and MSC, not the experiment fixture:

| ELF evidence | Value |
| --- | --- |
| Product descriptor | `streambewi-esp32` |
| Product version | `0.1.0` |
| Chip metadata | `ESP32-S3` |
| Monomorphized product future | 8,632 bytes, alignment 8 |
| Entry task future | 8,680 bytes, alignment 8 |
| Linker stack reservation | 51,616 bytes (57,744 before `iobewi-log` added its static ring) |
| Declared heap | 98,304 bytes |
| Requested minimum stack reservation | 16,384 bytes |
| Requested sockets | 3 (DHCP, DNS, HTTP) |

`tools/inspect_entry_elf.py` checks the descriptor/chip and extracts both exported
future layouts and linker stack symbols. The existing ESP CI job runs it and
includes `entry-layout.json` with its merged-image artifact; no additional job
is created. The linker emits its existing RWX LOAD-segment warning.

The stack reservation is **not measured free stack**. The declared heap is not
measured free heap. The 96 KiB rolling audio ring and static entry task storage
are included in the linked RAM layout; Wi-Fi runtime allocations, boxed serial
readers and temporary NVS buffers consume heap at runtime. Hardware observations
are still needed for allocator headroom and stack high-water usage during
provisioning, playback and recovery. Successful linking alone does not establish
those margins.

## Human hardware acceptance

Use the CI-produced merged image and record the product commit, IOBEWI revision,
image SHA-256 and card/player model. A full installation can erase NVS; preserve
credentials if that is part of the intended test, or explicitly start virgin.
Do not infer qualification from the earlier PR #5 image.

1. Virgin provisioning: Improv works on the available serial ports, reports
   `ESP32-S3`, saves credentials/flag and replies successfully. No MSC appears
   during that same boot.
2. Restart or power cycle: MSC appears after prebuffer, JTAG is absent, and the
   Metronic plays for ten minutes. Record how the restart was triggered; closing
   a browser serial session is not in itself a general reset guarantee.
3. Short BOOT press does nothing; a three-second hold clears the flag first,
   clears credentials and restarts into provisioning. Reprovision and restart
   into MSC, then check persistence across a power cycle.
4. Record runtime heap headroom and stack high-water usage when diagnostic
   tooling is available. Do not add a physical JTAG console or panic sink to
   this image. Missing measurements remain open, not an implicit pass.

Nominal replay is sufficient for this migration check; no repeated-write stress
campaign is requested. Error injection, panic in OTG and detailed reset/PHY
proof remain separate open framework gates. No hardware has been flashed by
this migration work.
