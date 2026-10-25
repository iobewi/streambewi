//! Status LED policy (#3): which look the product's states get, and the task that shows it.
//!
//! Four sources publish what they know of themselves into one [`iobewi_state`] registry:
//!
//! | Source | Flags it publishes |
//! | --- | --- |
//! | System | `Booting`, `Configuring`, `Running` |
//! | Wi-Fi | `Unconfigured`, `Connecting`, `Connected`, `Reconnecting` |
//! | Stream | `Connecting`, `Connected`, `Ready`, `Reconnecting`, `Busy`, `Warning` |
//! | UsbMsc | `Initializing`, `Ready`, `Unavailable`, `Error` |
//!
//! Every publication is a named transition below, called at a boundary the product already knows
//! (never per sector, per chunk or from the MSC read path). A transition **replaces** the whole set
//! of its source, so the flags that stopped being true leave with it: there is no positive state to
//! forget to clear.
//!
//! [`look`] is the policy: an ordered list, the first rule that matches wins. It lives here, not in
//! the generic crates, and is tested on the host. The LED task ([`run_led`]) is portable too: the
//! target supplies a sink that writes one colour.
//!
//! A green LED means the Wi-Fi is connected and the stream has delivered its prebuffer. It does not
//! say that the player is listening: the product has no event for that.

use embassy_futures::select::{Either4, select4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::once_lock::OnceLock;
use embassy_time::{Instant, Timer};
use iobewi_indicator::{Indicator, Millis, Pattern, RGB8};
use iobewi_state::{Flags, Observer, Registry, Snapshot, Source, SourceId, State};

use crate::boot_policy::BootMode;
use crate::brightness::Brightness;

/// Sources of the product: System, Wi-Fi, Stream, UsbMsc.
pub const SOURCES: usize = 4;
/// Observers: the status LED, the settings saver (it waits for the disk to be free), and one spare for
/// the debug mode that comes later.
pub const OBSERVERS: usize = 3;

pub type Reg = Registry<CriticalSectionRawMutex, SOURCES, OBSERVERS>;
pub type Obs<'a> = Observer<'a, CriticalSectionRawMutex, SOURCES, OBSERVERS>;
pub type Snap = Snapshot<SOURCES>;
type Src<'a> = Source<'a, CriticalSectionRawMutex, SOURCES, OBSERVERS>;

/// The registry of the product.
pub static STATE: Reg = Registry::new();

// Colours at their full value: the LED brightness (`crate::brightness`) is the only attenuation, applied
// once by the indicator's master level. White, blue and green sit at two thirds of the level of orange and
// red, as they did (16/255 and 24/255 at the default brightness, level 23, to within 1/255).
const WHITE: RGB8 = RGB8::new(171, 171, 171);
const BLUE: RGB8 = RGB8::new(0, 0, 171);
const GREEN: RGB8 = RGB8::new(0, 171, 0);
const ORANGE: RGB8 = RGB8::new(255, 106, 0);
const RED: RGB8 = RGB8::new(255, 0, 0);

const FAST: u16 = 250;
const SLOW: u16 = 1_000;
const PULSE_PERIOD: u16 = 4_000;

/// The identifiers of the four sources, for the policy to read each one apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ids {
    pub system: SourceId,
    pub wifi: SourceId,
    pub stream: SourceId,
    pub usb: SourceId,
}

/// The four source handles. A handle is owned by its module's code: here they are kept together
/// because the product publishes through named transitions, and all the publications of one source
/// come from one task (`run`'s futures run in one task, cooperatively).
pub struct Sources<'a> {
    system: Src<'a>,
    wifi: Src<'a>,
    stream: Src<'a>,
    usb: Src<'a>,
}

impl<'a> Sources<'a> {
    /// Registers the four sources, or `None` if the registry has no room (it is only built for
    /// them).
    pub fn register(registry: &'a Reg) -> Option<Self> {
        Some(Self {
            system: registry.register().ok()?,
            wifi: registry.register().ok()?,
            stream: registry.register().ok()?,
            usb: registry.register().ok()?,
        })
    }

    pub fn ids(&self) -> Ids {
        Ids {
            system: self.system.id(),
            wifi: self.wifi.id(),
            stream: self.stream.id(),
            usb: self.usb.id(),
        }
    }

    // --- System -----------------------------------------------------------------------------

    /// The firmware started and is preparing its configuration.
    pub fn system_booting(&self) {
        self.system.replace(Flags::of(&[State::Booting]));
    }

    /// Configuration is ready and the product's services start: provisioning boot or normal boot.
    pub fn system_started(&self, mode: BootMode) {
        let state = match mode {
            BootMode::Provisioning => State::Configuring,
            BootMode::MassStorage => State::Running,
        };
        self.system.replace(Flags::of(&[state]));
    }

    // --- Wi-Fi ------------------------------------------------------------------------------

    /// No saved network: waiting for provisioning.
    pub fn wifi_unconfigured(&self) {
        self.wifi.replace(Flags::of(&[State::Unconfigured]));
    }

    /// The manager is connecting, or took over from a connection it is going to redo.
    pub fn wifi_connecting(&self) {
        self.wifi.replace(Flags::of(&[State::Connecting]));
    }

    /// Association and DHCP are up.
    pub fn wifi_connected(&self) {
        self.wifi.replace(Flags::of(&[State::Connected]));
    }

    /// The link was lost and is being restored.
    pub fn wifi_link_down(&self) {
        self.wifi.replace(Flags::of(&[State::Reconnecting]));
    }

    // --- Stream -----------------------------------------------------------------------------

    /// The connection set of the stream is replaced; a warning about the saved selection is not
    /// part of it and stays.
    fn stream_link(&self, link: Flags) {
        let link = if self.stream.is_set(State::Warning) {
            link.with(State::Warning)
        } else {
            link
        };
        self.stream.replace(link);
    }

    /// A connection to the station is being opened for the first time since it was selected.
    pub fn stream_connecting(&self) {
        self.stream_link(Flags::of(&[State::Connecting]));
    }

    /// The connection ended or failed, a new one comes after a delay.
    pub fn stream_reconnecting(&self) {
        self.stream_link(Flags::of(&[State::Reconnecting]));
    }

    /// The station answered 200: its body is being received.
    pub fn stream_connected(&self) {
        self.stream_link(Flags::of(&[State::Connected]));
    }

    /// This connection delivered the prebuffer.
    pub fn stream_ready(&self) {
        self.stream_link(Flags::of(&[State::Connected, State::Ready]));
    }

    /// A station change took over: the old connection is dropped, the new one is not open yet.
    pub fn stream_busy(&self) {
        self.stream_link(Flags::of(&[State::Busy]));
    }

    /// The selection could not be saved (`true`) or was saved (`false`).
    pub fn stream_selection_unsaved(&self, unsaved: bool) {
        if unsaved {
            self.stream.raise(State::Warning);
        } else {
            self.stream.clear(State::Warning);
        }
    }

    // --- USB MSC ----------------------------------------------------------------------------

    /// Waiting for the prebuffer before the disk is presented.
    pub fn usb_initializing(&self) {
        self.usb.replace(Flags::of(&[State::Initializing]));
    }

    /// The disk is on the bus.
    pub fn usb_ready(&self) {
        self.usb.replace(Flags::of(&[State::Ready]));
    }

    /// The disk is off the bus on purpose (stall recovery, station change).
    pub fn usb_off_bus(&self) {
        self.usb.replace(Flags::of(&[State::Unavailable]));
    }

    /// The mass-storage class ended on an endpoint error.
    pub fn usb_error(&self) {
        self.usb.replace(Flags::of(&[State::Error]));
    }
}

/// The product's sources, once [`init`] ran. Before that (host tests, a target that did not call
/// it) every publication does nothing.
static SOURCES_CELL: OnceLock<Sources<'static>> = OnceLock::new();

/// Registers the product's four sources, once; later calls change nothing. The target calls it
/// before configuration so that `Booting` shows from the start.
pub fn init() {
    if !SOURCES_CELL.is_set()
        && let Some(sources) = Sources::register(&STATE)
    {
        // The registry holds only these four sources; a second caller's duplicate is dropped.
        let _ = SOURCES_CELL.init(sources);
    }
    with(|s| s.system_booting());
}

fn with(f: impl FnOnce(&Sources<'static>)) {
    if let Some(sources) = SOURCES_CELL.try_get() {
        f(sources);
    }
}

pub fn system_started(mode: BootMode) {
    with(|s| s.system_started(mode));
}
pub fn wifi_unconfigured() {
    with(|s| s.wifi_unconfigured());
}
pub fn wifi_connecting() {
    with(|s| s.wifi_connecting());
}
pub fn wifi_connected() {
    with(|s| s.wifi_connected());
}
pub fn wifi_link_down() {
    with(|s| s.wifi_link_down());
}
pub fn stream_connecting() {
    with(|s| s.stream_connecting());
}
pub fn stream_reconnecting() {
    with(|s| s.stream_reconnecting());
}
pub fn stream_connected() {
    with(|s| s.stream_connected());
}
pub fn stream_ready() {
    with(|s| s.stream_ready());
}
pub fn stream_busy() {
    with(|s| s.stream_busy());
}
pub fn stream_selection_unsaved(unsaved: bool) {
    with(|s| s.stream_selection_unsaved(unsaved));
}
pub fn usb_initializing() {
    with(|s| s.usb_initializing());
}
pub fn usb_ready() {
    with(|s| s.usb_ready());
}
pub fn usb_off_bus() {
    with(|s| s.usb_off_bus());
}
pub fn usb_error() {
    with(|s| s.usb_error());
}

/// An observer of the product's registry, and the identifiers of its sources. `None` before [`init`],
/// or if the observers are all taken.
pub fn observer() -> Option<(Obs<'static>, Ids)> {
    let sources = SOURCES_CELL.try_get()?;
    Some((STATE.observer()?, sources.ids()))
}

/// What the settings saver needs to know about the USB disk and to say about it: to watch the UsbMsc
/// source and to publish its own detachment.
pub struct Disk<'a> {
    pub observer: Obs<'a>,
    pub sources: &'a Sources<'a>,
}

/// The saver's view of the product's registry. `None` before [`init`], or if the observers are all
/// taken.
pub fn disk() -> Option<Disk<'static>> {
    Some(Disk {
        observer: STATE.observer()?,
        sources: SOURCES_CELL.try_get()?,
    })
}

/// A look: the pattern and a name for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Look {
    pub pattern: Pattern,
    pub name: &'static str,
}

const fn look_of(pattern: Pattern, name: &'static str) -> Look {
    Look { pattern, name }
}

/// The policy: the first rule that matches wins.
///
/// 1. `Critical` or `Error` (any source): red blinking.
/// 2. `Degraded` or `Warning` (any source): orange steady.
/// 3. Provisioning (`Configuring`) or `Unconfigured`: blue blinking.
/// 4. The Wi-Fi is connecting or reconnecting: blue blinking slowly.
/// 5. `Booting`: white steady.
/// 6. The Wi-Fi is connected **and** the stream is ready: green steady.
/// 7. The Wi-Fi is connected, the stream is not ready yet: green pulsing slowly.
/// 8. Anything else: off.
///
/// `Busy` (station change) and `Unavailable` (disk off the bus) have no rule of their own. A
/// station change only makes the stream not ready, so rule 7 shows; the disk leaving the bus changes
/// nothing. Rules 6 and 7 read the Wi-Fi and Stream sources themselves, not the aggregate: another
/// source's `Connected` or `Ready` must not turn the LED green.
pub fn look(snapshot: &Snap, ids: &Ids) -> Look {
    let any = snapshot.aggregate();
    let flags_of = |id| snapshot.of(id).unwrap_or(Flags::EMPTY);
    let wifi = flags_of(ids.wifi);
    let stream = flags_of(ids.stream);

    if any.contains(State::Critical) || any.contains(State::Error) {
        look_of(blink(RED, FAST), "red blinking")
    } else if any.contains(State::Degraded) || any.contains(State::Warning) {
        look_of(Pattern::Solid(ORANGE), "orange steady")
    } else if any.contains(State::Configuring) || any.contains(State::Unconfigured) {
        look_of(blink(BLUE, FAST), "blue blinking")
    } else if wifi.contains(State::Connecting) || wifi.contains(State::Reconnecting) {
        look_of(blink(BLUE, SLOW), "blue blinking slowly")
    } else if any.contains(State::Booting) {
        look_of(Pattern::Solid(WHITE), "white steady")
    } else if wifi.contains(State::Connected) && stream.contains(State::Ready) {
        look_of(Pattern::Solid(GREEN), "green steady")
    } else if wifi.contains(State::Connected) {
        let pulse = Pattern::Pulse {
            color: GREEN,
            period_ms: PULSE_PERIOD,
        };
        look_of(pulse, "green pulsing")
    } else {
        look_of(Pattern::Off, "off")
    }
}

const fn blink(color: RGB8, half_period_ms: u16) -> Pattern {
    Pattern::Blink {
        color,
        on_ms: half_period_ms,
        off_ms: half_period_ms,
    }
}

fn now_ms() -> Millis {
    // The low 32 bits are enough: the indicator works modulo 2^32 ms.
    Instant::now().as_millis() as Millis
}

/// The status LED task: shows [`look`] of the registry on one LED through `sink`, at the brightness
/// of `brightness`.
///
/// It wakes on four things and nothing else: the registry changes, the indicator says the colour can
/// next change (blink, pulse), the brightness changes, and a preview of the brightness runs out. A
/// change that leaves the look and the level the same writes nothing; a change of level keeps the
/// phase of the animation. Until the saved brightness is known the LED stays dark. `sink` writes one
/// colour; its first error ends the task and nothing else (the product goes on without the LED).
pub async fn run_led<E>(
    mut observer: Obs<'_>,
    ids: Ids,
    brightness: &Brightness,
    mut sink: impl FnMut(RGB8) -> Result<(), E>,
) {
    brightness.set_available(true);
    run_led_loop(&mut observer, ids, brightness, &mut sink).await;
    brightness.set_available(false);
}

async fn run_led_loop<E>(
    observer: &mut Obs<'_>,
    ids: Ids,
    brightness: &Brightness,
    sink: &mut impl FnMut(RGB8) -> Result<(), E>,
) {
    let mut indicator = Indicator::new(Pattern::Off, now_ms());
    indicator.set_intensity(brightness.applied_level(Instant::now()));
    let mut last_name = "";
    let mut snapshot = observer.changed().await;
    loop {
        let look = look(&snapshot, &ids);
        let now = now_ms();
        indicator.set(look.pattern, now);
        if look.name != last_name {
            last_name = look.name;
            log::info!(
                "status: LED {} (state rev {})",
                look.name,
                snapshot.revision()
            );
        }
        let Ok(mut wake) = indicator.drive(now, &mut *sink) else {
            log::warn!("status: LED write failed, the status LED stops");
            return;
        };
        snapshot = loop {
            let tick = async {
                match wake {
                    Some(ms) => Timer::after_millis(u64::from(ms)).await,
                    None => core::future::pending().await,
                }
            };
            let expiry = async {
                match brightness.preview_deadline() {
                    Some(deadline) => Timer::at(deadline).await,
                    None => core::future::pending().await,
                }
            };
            match select4(observer.changed(), tick, brightness.changed(), expiry).await {
                Either4::First(next) => break next,
                // The colour can change although no flag did: blink and pulse.
                Either4::Second(()) => {}
                // The level changed, or a preview ran out: same look, other level, same phase.
                Either4::Third(()) | Either4::Fourth(()) => {
                    brightness.expire(Instant::now());
                    indicator.set_intensity(brightness.applied_level(Instant::now()));
                }
            }
            match indicator.drive(now_ms(), &mut *sink) {
                Ok(next) => wake = next,
                Err(_) => {
                    log::warn!("status: LED write failed, the status LED stops");
                    return;
                }
            }
        };
    }
}

#[cfg(test)]
#[path = "status_tests.rs"]
mod tests;
