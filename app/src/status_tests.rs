use core::cell::RefCell;

use embassy_futures::{
    block_on,
    select::{Either, select},
};
use embassy_time::Timer;

use super::*;

fn registry() -> &'static Reg {
    std::boxed::Box::leak(std::boxed::Box::new(Registry::new()))
}

/// A product's registry with its four sources.
fn product() -> (&'static Reg, Sources<'static>) {
    let reg = registry();
    let sources = Sources::register(reg).expect("room for four sources");
    (reg, sources)
}

fn name(reg: &Reg, sources: &Sources<'_>) -> &'static str {
    look(&reg.snapshot(), &sources.ids()).name
}

/// A source's flags in the registry.
fn flags(reg: &Reg, id: SourceId) -> Flags {
    reg.snapshot().of(id).expect("registered")
}

// --- Policy -------------------------------------------------------------------------------------

#[test]
fn nothing_published_is_off() {
    let (reg, s) = product();
    assert_eq!(name(reg, &s), "off");
    assert_eq!(look(&reg.snapshot(), &s.ids()).pattern, Pattern::Off);
}

#[test]
fn the_policy_table() {
    let (reg, s) = product();

    s.system_booting();
    assert_eq!(name(reg, &s), "white steady");
    assert_eq!(
        look(&reg.snapshot(), &s.ids()).pattern,
        Pattern::Solid(WHITE)
    );

    s.system_started(BootMode::MassStorage);
    assert_eq!(name(reg, &s), "off");

    s.wifi_connecting();
    assert_eq!(name(reg, &s), "blue blinking slowly");
    s.wifi_link_down();
    assert_eq!(name(reg, &s), "blue blinking slowly");

    s.wifi_connected();
    assert_eq!(
        name(reg, &s),
        "green pulsing",
        "connected, stream not ready"
    );
    s.stream_connecting();
    s.stream_connected();
    assert_eq!(name(reg, &s), "green pulsing");
    s.stream_ready();
    assert_eq!(name(reg, &s), "green steady");

    s.wifi_unconfigured();
    assert_eq!(name(reg, &s), "blue blinking");
}

#[test]
fn provisioning_boot_is_blue_whatever_the_wifi_does() {
    let (reg, s) = product();
    s.system_started(BootMode::Provisioning);
    assert_eq!(name(reg, &s), "blue blinking");
    s.wifi_connecting();
    assert_eq!(name(reg, &s), "blue blinking");
    s.wifi_connected();
    s.stream_ready();
    assert_eq!(
        name(reg, &s),
        "blue blinking",
        "provisioning wins over a ready stream"
    );
}

#[test]
fn an_error_or_a_warning_beats_the_rest() {
    let (reg, s) = product();
    s.system_started(BootMode::MassStorage);
    s.wifi_connected();
    s.stream_ready();
    assert_eq!(name(reg, &s), "green steady");

    s.stream_selection_unsaved(true);
    assert_eq!(name(reg, &s), "orange steady");
    s.usb_error();
    assert_eq!(name(reg, &s), "red blinking", "an error beats a warning");
    let red = look(&reg.snapshot(), &s.ids()).pattern;
    assert_eq!(
        red,
        Pattern::Blink {
            color: RED,
            on_ms: 250,
            off_ms: 250
        }
    );

    s.usb_ready();
    assert_eq!(name(reg, &s), "orange steady");
    s.stream_selection_unsaved(false);
    assert_eq!(name(reg, &s), "green steady");
}

#[test]
fn green_needs_the_wifi_and_the_stream_themselves() {
    let (reg, s) = product();
    s.system_started(BootMode::MassStorage);

    // A `Connected` or a `Ready` of another source is not the Wi-Fi connected or the stream ready.
    s.stream_ready();
    assert_eq!(name(reg, &s), "off", "ready stream, no Wi-Fi: not green");
    s.usb_ready();
    assert_eq!(name(reg, &s), "off");
    s.wifi_connecting();
    assert_eq!(
        name(reg, &s),
        "blue blinking slowly",
        "the Wi-Fi rule is above the green ones"
    );

    s.wifi_connected();
    s.stream_connecting();
    s.usb_ready();
    assert_eq!(
        name(reg, &s),
        "green pulsing",
        "the USB `Ready` is not the stream's"
    );
}

#[test]
fn a_station_change_and_the_disk_leaving_the_bus_have_no_look_of_their_own() {
    let (reg, s) = product();
    s.system_started(BootMode::MassStorage);
    s.wifi_connected();
    s.stream_ready();
    s.usb_ready();
    assert_eq!(name(reg, &s), "green steady");

    // The disk leaving the bus changes nothing.
    s.usb_off_bus();
    assert_eq!(name(reg, &s), "green steady");

    // A station change only makes the stream not ready.
    s.stream_busy();
    assert_eq!(name(reg, &s), "green pulsing");
    s.stream_connecting();
    s.stream_connected();
    s.stream_ready();
    s.usb_ready();
    assert_eq!(name(reg, &s), "green steady");
}

/// The colour the LED gets from `color` at `level`, as the indicator computes it.
fn shown(color: RGB8, level: u8) -> RGB8 {
    let mut indicator = Indicator::new(Pattern::Solid(color), 0);
    indicator.set_intensity(level);
    indicator.poll(0).color.expect("a colour to show")
}

#[test]
fn the_colours_keep_their_look_at_the_default_brightness_and_are_attenuated_once() {
    use crate::brightness::{DEFAULT_PERCENT, MAX_LEVEL, level_of};
    let level = level_of(DEFAULT_PERCENT);
    assert_eq!(level, 23);
    // What the LED showed before the setting existed: 16/255 for white, blue and green, 24/255 for
    // orange and red, to within 1/255.
    let near = |got: u8, want: u8| got.abs_diff(want) <= 1;
    for color in [WHITE, BLUE, GREEN] {
        let on = iobewi_indicator::level(shown(color, level));
        assert!(near(on, 16), "{color:?} shows {on}");
    }
    for color in [ORANGE, RED] {
        let on = iobewi_indicator::level(shown(color, level));
        assert!(near(on, 24), "{color:?} shows {on}");
    }
    assert_eq!(shown(ORANGE, level), RGB8::new(23, 9, 0));
    // The proportion between the ordinary and the strong colours is kept at every level.
    for level in [8, 23, MAX_LEVEL] {
        let ordinary = u32::from(iobewi_indicator::level(shown(GREEN, level)));
        let strong = u32::from(iobewi_indicator::level(shown(RED, level)));
        assert!(
            (strong * 2).abs_diff(ordinary * 3) <= 3,
            "level {level}: {ordinary} and {strong}"
        );
    }
    // 100 % is the cap, 0 % is dark.
    assert_eq!(iobewi_indicator::level(shown(RED, MAX_LEVEL)), MAX_LEVEL);
    assert_eq!(shown(RED, 0), RGB8::new(0, 0, 0));
    assert_eq!(shown(WHITE, 0), RGB8::new(0, 0, 0));
}

// --- Transitions leave nothing stale ---------------------------------------------------------

#[test]
fn the_wifi_never_keeps_a_flag_it_stopped_to_have() {
    let (reg, s) = product();
    let id = s.ids().wifi;
    let only = |state| Flags::of(&[state]);

    s.wifi_connecting();
    assert_eq!(flags(reg, id), only(State::Connecting));
    s.wifi_connected();
    assert_eq!(flags(reg, id), only(State::Connected));
    s.wifi_link_down();
    assert_eq!(
        flags(reg, id),
        only(State::Reconnecting),
        "no `Connected` after the link is lost"
    );
    s.wifi_connected();
    s.wifi_unconfigured();
    assert_eq!(flags(reg, id), only(State::Unconfigured));
    s.wifi_connecting();
    assert_eq!(flags(reg, id), only(State::Connecting));
}

#[test]
fn the_stream_never_keeps_a_flag_it_stopped_to_have() {
    let (reg, s) = product();
    let id = s.ids().stream;

    s.stream_connecting();
    s.stream_connected();
    s.stream_ready();
    assert_eq!(flags(reg, id), Flags::of(&[State::Connected, State::Ready]));

    // The connection ends: neither `Connected` nor `Ready` may stay.
    s.stream_reconnecting();
    assert_eq!(flags(reg, id), Flags::of(&[State::Reconnecting]));

    s.stream_connected();
    s.stream_ready();
    // A station change drops the connection whatever it was doing.
    s.stream_busy();
    assert_eq!(flags(reg, id), Flags::of(&[State::Busy]));
    s.stream_connecting();
    assert_eq!(
        flags(reg, id),
        Flags::of(&[State::Connecting]),
        "`Busy` is gone with the change"
    );
}

#[test]
fn the_unsaved_warning_survives_the_connection_but_not_a_successful_save() {
    let (reg, s) = product();
    let id = s.ids().stream;

    s.stream_busy();
    s.stream_selection_unsaved(true);
    assert_eq!(flags(reg, id), Flags::of(&[State::Busy, State::Warning]));
    s.stream_connecting();
    assert_eq!(
        flags(reg, id),
        Flags::of(&[State::Connecting, State::Warning])
    );
    s.stream_connected();
    s.stream_ready();
    s.stream_reconnecting();
    assert!(flags(reg, id).contains(State::Warning));

    s.stream_selection_unsaved(false);
    assert_eq!(flags(reg, id), Flags::of(&[State::Reconnecting]));
}

#[test]
fn the_usb_and_the_system_never_keep_a_flag_they_stopped_to_have() {
    let (reg, s) = product();
    let (usb, system) = (s.ids().usb, s.ids().system);

    s.usb_initializing();
    assert_eq!(flags(reg, usb), Flags::of(&[State::Initializing]));
    s.usb_ready();
    assert_eq!(flags(reg, usb), Flags::of(&[State::Ready]));
    s.usb_off_bus();
    assert_eq!(flags(reg, usb), Flags::of(&[State::Unavailable]));
    s.usb_ready();
    s.usb_error();
    assert_eq!(flags(reg, usb), Flags::of(&[State::Error]));

    s.system_booting();
    assert_eq!(flags(reg, system), Flags::of(&[State::Booting]));
    s.system_started(BootMode::Provisioning);
    assert_eq!(
        flags(reg, system),
        Flags::of(&[State::Configuring]),
        "`Booting` is gone"
    );
    s.system_started(BootMode::MassStorage);
    assert_eq!(flags(reg, system), Flags::of(&[State::Running]));
}

/// Every transition the product has, by name.
type Transition = (&'static str, fn(&Sources<'_>));
const TRANSITIONS: [Transition; 16] = [
    ("system_booting", |s| s.system_booting()),
    ("system_started(provisioning)", |s| {
        s.system_started(BootMode::Provisioning)
    }),
    ("system_started(mass storage)", |s| {
        s.system_started(BootMode::MassStorage)
    }),
    ("wifi_unconfigured", |s| s.wifi_unconfigured()),
    ("wifi_connecting", |s| s.wifi_connecting()),
    ("wifi_connected", |s| s.wifi_connected()),
    ("wifi_link_down", |s| s.wifi_link_down()),
    ("stream_connecting", |s| s.stream_connecting()),
    ("stream_reconnecting", |s| s.stream_reconnecting()),
    ("stream_connected", |s| s.stream_connected()),
    ("stream_ready", |s| s.stream_ready()),
    ("stream_busy", |s| s.stream_busy()),
    ("stream_selection_unsaved(true)", |s| {
        s.stream_selection_unsaved(true)
    }),
    ("usb_initializing", |s| s.usb_initializing()),
    ("usb_off_bus", |s| s.usb_off_bus()),
    ("usb_error", |s| s.usb_error()),
];

#[test]
fn after_any_sequence_each_source_holds_one_coherent_set() {
    // Every sequence of three transitions, and what each source may hold at the end. A source
    // holds the set its last transition gave it (plus the unsaved warning for the stream), never
    // a union of two.
    let ok = |set: Flags, allowed: &[&[State]]| allowed.iter().any(|a| set == Flags::of(a));
    let n = TRANSITIONS.len();
    for a in 0..n {
        for b in 0..n {
            for c in 0..n {
                let (reg, s) = product();
                let steps = [a, b, c];
                for step in steps {
                    (TRANSITIONS[step].1)(&s);
                }
                let ids = s.ids();
                let label = || steps.map(|i| TRANSITIONS[i].0);

                let wifi = flags(reg, ids.wifi);
                assert!(
                    wifi.is_empty()
                        || ok(
                            wifi,
                            &[
                                &[State::Unconfigured],
                                &[State::Connecting],
                                &[State::Connected],
                                &[State::Reconnecting],
                            ]
                        ),
                    "wifi {wifi:?} after {:?}",
                    label()
                );
                let stream = flags(reg, ids.stream).without(State::Warning);
                assert!(
                    stream.is_empty()
                        || ok(
                            stream,
                            &[
                                &[State::Connecting],
                                &[State::Reconnecting],
                                &[State::Connected],
                                &[State::Connected, State::Ready],
                                &[State::Busy],
                            ]
                        ),
                    "stream {stream:?} after {:?}",
                    label()
                );
                let usb = flags(reg, ids.usb);
                assert!(
                    usb.is_empty()
                        || ok(
                            usb,
                            &[
                                &[State::Initializing],
                                &[State::Ready],
                                &[State::Unavailable],
                                &[State::Error],
                            ]
                        ),
                    "usb {usb:?} after {:?}",
                    label()
                );
                let system = flags(reg, ids.system);
                assert!(
                    system.is_empty()
                        || ok(
                            system,
                            &[&[State::Booting], &[State::Configuring], &[State::Running]]
                        ),
                    "system {system:?} after {:?}",
                    label()
                );
            }
        }
    }
}

#[test]
fn repeating_a_transition_changes_nothing() {
    let (reg, s) = product();
    for (label, transition) in TRANSITIONS {
        transition(&s);
        let rev = reg.snapshot().revision();
        transition(&s);
        assert_eq!(reg.snapshot().revision(), rev, "{label} twice");
    }
}

#[test]
fn publishing_before_init_does_nothing_and_init_registers_once() {
    // The free functions are the ones the product calls.
    wifi_connected();
    stream_ready();
    usb_error();
    init();
    init();
    let snapshot = STATE.snapshot();
    assert_eq!(
        snapshot.sources().count(),
        4,
        "four sources, registered once"
    );
    let (_observer, ids) = observer().expect("an observer for the LED");
    assert_eq!(
        snapshot.of(ids.system),
        Some(Flags::of(&[State::Booting])),
        "init raises `Booting`"
    );
    wifi_connecting();
    assert_eq!(
        STATE.snapshot().of(ids.wifi),
        Some(Flags::of(&[State::Connecting]))
    );
    wifi_connected();
    assert_eq!(
        STATE.snapshot().of(ids.wifi),
        Some(Flags::of(&[State::Connected]))
    );
}

// --- Health -------------------------------------------------------------------------------------

fn words(reg: &Reg, sources: &Sources<'_>) -> [&'static str; 4] {
    let h = health(&reg.snapshot(), &sources.ids());
    [h.system, h.wifi, h.stream, h.usb]
}

#[test]
fn nothing_published_is_none_and_an_unreadable_registry_is_unknown() {
    let (reg, sources) = product();
    assert_eq!(words(reg, &sources), ["none"; 4], "registered, silent");
    // A snapshot without these sources (registry never initialised for them) reads as unknown.
    let empty = health(&Snap::EMPTY, &sources.ids());
    assert_eq!(
        [empty.system, empty.wifi, empty.stream, empty.usb],
        ["unknown"; 4]
    );
}

#[test]
fn boot_provisioning_and_running_are_told_apart() {
    let (reg, sources) = product();
    sources.system_booting();
    assert_eq!(words(reg, &sources)[0], "booting");
    sources.system_started(BootMode::Provisioning);
    assert_eq!(words(reg, &sources)[0], "configuring");
    sources.system_started(BootMode::MassStorage);
    assert_eq!(words(reg, &sources)[0], "running");
}

#[test]
fn the_wifi_words_follow_its_transitions() {
    let (reg, sources) = product();
    let mut seen = std::vec::Vec::new();
    for step in [
        |s: &Sources<'static>| s.wifi_unconfigured(),
        |s: &Sources<'static>| s.wifi_connecting(),
        |s: &Sources<'static>| s.wifi_connected(),
        |s: &Sources<'static>| s.wifi_link_down(),
        |s: &Sources<'static>| s.wifi_connecting(),
        |s: &Sources<'static>| s.wifi_connected(),
    ] {
        step(&sources);
        seen.push(words(reg, &sources)[1]);
    }
    assert_eq!(
        seen,
        [
            "unconfigured",
            "connecting",
            "connected",
            "reconnecting",
            "connecting",
            "connected"
        ]
    );
}

#[test]
fn the_stream_words_keep_connected_and_ready_apart_and_forget_nothing_stale() {
    let (reg, sources) = product();
    let mut seen = std::vec::Vec::new();
    for step in [
        |s: &Sources<'static>| s.stream_connecting(),
        |s: &Sources<'static>| s.stream_connected(),
        |s: &Sources<'static>| s.stream_ready(),
        |s: &Sources<'static>| s.stream_reconnecting(),
        |s: &Sources<'static>| s.stream_connected(),
        |s: &Sources<'static>| s.stream_ready(),
        |s: &Sources<'static>| s.stream_busy(),
        |s: &Sources<'static>| s.stream_connecting(),
    ] {
        step(&sources);
        seen.push(words(reg, &sources)[2]);
    }
    assert_eq!(
        seen,
        [
            "connecting",
            "connected",
            "ready",
            "reconnecting",
            "connected",
            "ready",
            "busy",
            "connecting"
        ]
    );
}

#[test]
fn the_unsaved_warning_changes_no_stream_word() {
    let (reg, sources) = product();
    sources.stream_selection_unsaved(true);
    assert_eq!(
        words(reg, &sources)[2],
        "none",
        "a warning alone is no link"
    );
    sources.stream_ready();
    assert_eq!(words(reg, &sources)[2], "ready");
    sources.stream_busy();
    assert_eq!(words(reg, &sources)[2], "busy");
    sources.stream_selection_unsaved(false);
    assert_eq!(words(reg, &sources)[2], "busy");
}

#[test]
fn the_usb_keeps_unavailable_and_error_apart() {
    let (reg, sources) = product();
    let mut seen = std::vec::Vec::new();
    for step in [
        |s: &Sources<'static>| s.usb_initializing(),
        |s: &Sources<'static>| s.usb_ready(),
        |s: &Sources<'static>| s.usb_off_bus(),
        |s: &Sources<'static>| s.usb_ready(),
        |s: &Sources<'static>| s.usb_error(),
    ] {
        step(&sources);
        seen.push(words(reg, &sources)[3]);
    }
    assert_eq!(
        seen,
        ["initializing", "ready", "unavailable", "ready", "error"]
    );
}

#[test]
fn a_set_the_table_does_not_know_is_unknown_not_nominal() {
    let (reg, sources) = product();
    sources.usb.replace(Flags::of(&[State::Healthy]));
    sources.wifi.replace(Flags::of(&[State::Paused]));
    let w = words(reg, &sources);
    assert_eq!((w[1], w[3]), ("unknown", "unknown"));
}

#[test]
fn health_is_one_read_that_takes_no_observer() {
    let (reg, sources) = product();
    let _a = reg.observer().expect("first");
    let _b = reg.observer().expect("second");
    let _c = reg.observer().expect("third");
    sources.wifi_connected();
    assert_eq!(words(reg, &sources)[1], "connected");
    assert!(reg.observer().is_none(), "the three places were left alone");
}

// --- The task -----------------------------------------------------------------------------------

/// Runs the LED task against a registry that `drive` changes, and returns the colours written.
fn run_task(
    drive: impl AsyncFnOnce(&Reg, &Sources<'static>),
    fail_after: Option<usize>,
) -> std::vec::Vec<RGB8> {
    let (reg, sources) = product();
    let ids = sources.ids();
    let written = RefCell::new(std::vec::Vec::new());
    let observer = reg.observer().expect("an observer");
    let brightness = Brightness::new();
    brightness.loaded(crate::brightness::DEFAULT_PERCENT);
    let sink = |color: RGB8| {
        let mut written = written.borrow_mut();
        if fail_after.is_some_and(|n| written.len() >= n) {
            return Err(());
        }
        written.push(color);
        Ok(())
    };
    let task = run_led(observer, ids, &brightness, sink);
    let driver = async {
        // Let the task show the initial state, then play.
        Timer::after_millis(30).await;
        drive(reg, &sources).await;
        Timer::after_millis(30).await;
    };
    match block_on(select(task, driver)) {
        Either::First(()) | Either::Second(()) => {}
    }
    written.into_inner()
}

#[test]
fn the_task_writes_a_colour_only_when_the_look_changes() {
    let written = run_task(
        async |_reg, s| {
            s.system_started(BootMode::MassStorage);
            s.wifi_connected();
            s.stream_ready();
            Timer::after_millis(30).await;
            // These change the registry and not the look: nothing is written.
            s.usb_ready();
            s.usb_off_bus();
            s.usb_ready();
            s.system_started(BootMode::MassStorage);
            Timer::after_millis(30).await;
            s.wifi_link_down();
            Timer::after_millis(30).await;
        },
        None,
    );
    // Dark at the start, steady green, then the first colour of the slow blue blink.
    assert_eq!(
        written,
        [RGB8::new(0, 0, 0), RGB8::new(0, 16, 0), RGB8::new(0, 0, 16)]
    );
}

#[test]
fn a_wifi_flag_dropped_while_nothing_else_changes_leaves_the_led_alone() {
    // The aggregate keeps `Connected` here, and so does the look.
    let written = run_task(
        async |_reg, s| {
            s.wifi_connected();
            s.stream_ready();
            Timer::after_millis(30).await;
            s.usb_ready();
            Timer::after_millis(30).await;
        },
        None,
    );
    assert_eq!(written, [RGB8::new(0, 0, 0), RGB8::new(0, 16, 0)]);
}

#[test]
fn a_blinking_look_goes_on_while_no_flag_changes() {
    let written = run_task(
        async |_reg, s| {
            s.system_started(BootMode::Provisioning);
            // 250 ms on, 250 ms off: no flag changes in the next second.
            Timer::after_millis(1_100).await;
        },
        None,
    );
    let blue = RGB8::new(0, 0, 16);
    let dark = RGB8::new(0, 0, 0);
    // dark, then blue / dark alternating on the indicator's own deadlines.
    assert!(written.len() >= 5, "{written:?}");
    assert_eq!(&written[..3], &[dark, blue, dark]);
    assert!(written.windows(2).all(|w| w[0] != w[1]), "{written:?}");
}

#[test]
fn a_failed_write_ends_the_task_and_nothing_else() {
    let (reg, s) = product();
    let observer = reg.observer().expect("an observer");
    let ids = s.ids();
    // Fails on its second write.
    let writes = RefCell::new(0usize);
    let sink = |_color: RGB8| {
        let mut writes = writes.borrow_mut();
        *writes += 1;
        if *writes >= 2 { Err(()) } else { Ok(()) }
    };
    let driver = async {
        Timer::after_millis(20).await;
        s.wifi_connected();
        s.stream_ready();
        Timer::after_millis(200).await;
    };
    let brightness = Brightness::new();
    brightness.loaded(crate::brightness::DEFAULT_PERCENT);
    let ended = block_on(select(run_led(observer, ids, &brightness, sink), driver));
    assert!(
        matches!(ended, Either::First(())),
        "the task ended on its own"
    );
    // The registry goes on working after the LED is gone.
    s.wifi_link_down();
    assert_eq!(flags(reg, ids.wifi), Flags::of(&[State::Reconnecting]));
}

// --- The task and the brightness ----------------------------------------------------------------

/// Runs the LED task on `brightness` while `drive` plays, and returns every colour written with the
/// time since the start, in milliseconds.
fn run_timed(
    brightness: &Brightness,
    drive: impl AsyncFnOnce(&Reg, &Sources<'static>),
) -> std::vec::Vec<(u128, RGB8)> {
    let (reg, sources) = product();
    let ids = sources.ids();
    let start = std::time::Instant::now();
    let written = RefCell::new(std::vec::Vec::new());
    let observer = reg.observer().expect("an observer");
    let sink = |color: RGB8| -> Result<(), ()> {
        written
            .borrow_mut()
            .push((start.elapsed().as_millis(), color));
        Ok(())
    };
    let task = run_led(observer, ids, brightness, sink);
    let driver = async {
        Timer::after_millis(30).await;
        drive(reg, &sources).await;
        Timer::after_millis(30).await;
    };
    match block_on(select(task, driver)) {
        Either::First(()) | Either::Second(()) => {}
    }
    written.into_inner()
}

fn colors(written: &[(u128, RGB8)]) -> std::vec::Vec<RGB8> {
    written.iter().map(|(_, c)| *c).collect()
}

const DARK: RGB8 = RGB8::new(0, 0, 0);

fn green_ready(s: &Sources<'_>) {
    s.system_started(BootMode::MassStorage);
    s.wifi_connected();
    s.stream_ready();
}

#[test]
fn a_preview_is_shown_at_once_without_any_flag_changing_and_a_cancel_goes_back() {
    let brightness = Brightness::new();
    brightness.loaded(36);
    let written = run_timed(&brightness, async |_reg, s| {
        green_ready(s);
        Timer::after_millis(40).await;
        brightness.preview(100, Instant::now()).unwrap();
        Timer::after_millis(40).await;
        brightness.preview(0, Instant::now()).unwrap();
        Timer::after_millis(40).await;
        brightness.cancel();
        Timer::after_millis(40).await;
    });
    assert_eq!(
        colors(&written),
        [
            DARK,
            RGB8::new(0, 16, 0), // saved, 36 %
            RGB8::new(0, 43, 0), // 100 %, level 64
            DARK,                // 0 %
            RGB8::new(0, 16, 0), // cancelled
        ]
    );
    assert_eq!(
        brightness.status(Instant::now()).saved,
        36,
        "nothing was saved"
    );
}

#[test]
fn a_preview_ends_by_itself_and_the_led_goes_back_to_the_saved_value() {
    // The same mechanism as the 60 s of the firmware, with 150 ms.
    let brightness = Brightness::short_preview(embassy_time::Duration::from_millis(150));
    brightness.loaded(36);
    let written = run_timed(&brightness, async |_reg, s| {
        green_ready(s);
        Timer::after_millis(40).await;
        brightness.preview(100, Instant::now()).unwrap();
        Timer::after_millis(400).await;
    });
    assert_eq!(
        colors(&written),
        [
            DARK,
            RGB8::new(0, 16, 0),
            RGB8::new(0, 43, 0),
            RGB8::new(0, 16, 0)
        ]
    );
    let (shown, back) = (written[2].0, written[3].0);
    assert!(
        (120..=300).contains(&(back - shown)),
        "back after {} ms",
        back - shown
    );
}

#[test]
fn a_new_preview_renews_the_time_and_a_saved_value_ends_the_preview() {
    let brightness = Brightness::short_preview(embassy_time::Duration::from_millis(200));
    brightness.loaded(36);
    let written = run_timed(&brightness, async |_reg, s| {
        green_ready(s);
        Timer::after_millis(40).await;
        brightness.preview(100, Instant::now()).unwrap();
        // Renewed before it ran out, twice: 360 ms in all, more than the 200 ms of one preview.
        Timer::after_millis(120).await;
        brightness.preview(100, Instant::now()).unwrap();
        Timer::after_millis(120).await;
        brightness.preview(100, Instant::now()).unwrap();
        Timer::after_millis(120).await;
        assert_eq!(brightness.status(Instant::now()).preview, Some(100));
        // A save that succeeds: the saved value, no preview left.
        brightness.request_save(50).unwrap();
        let percent = brightness.next_save().await;
        brightness.finish_save(percent, Ok(()));
        Timer::after_millis(60).await;
    });
    assert_eq!(
        colors(&written),
        [
            DARK,
            RGB8::new(0, 16, 0),
            RGB8::new(0, 43, 0),
            RGB8::new(0, 22, 0)
        ],
        "100 % held through the renewals, then 50 % (level 32)"
    );
    assert_eq!(brightness.preview_deadline(), None);
}

#[test]
fn the_led_stays_dark_until_the_saved_brightness_is_known() {
    let brightness = Brightness::new(); // not loaded yet
    let written = run_timed(&brightness, async |_reg, s| {
        s.system_booting();
        Timer::after_millis(80).await;
        brightness.loaded(36);
        Timer::after_millis(60).await;
    });
    // The white of `Booting` appears only once the level is known.
    assert_eq!(colors(&written), [DARK, RGB8::new(16, 16, 16)]);
}

#[test]
fn a_brightness_saved_at_zero_never_lights_the_led_at_start() {
    let brightness = Brightness::new();
    let written = run_timed(&brightness, async |_reg, s| {
        s.system_booting();
        Timer::after_millis(40).await;
        brightness.loaded(0);
        Timer::after_millis(40).await;
        s.system_started(BootMode::MassStorage);
        s.wifi_connecting();
        Timer::after_millis(40).await;
    });
    assert!(!written.is_empty());
    assert!(colors(&written).iter().all(|c| *c == DARK), "{written:?}");
}

#[test]
fn a_change_of_level_keeps_the_phase_of_the_animation() {
    let brightness = Brightness::new();
    brightness.loaded(36);
    // Provisioning: blue blinking, 250 ms lit then 250 ms dark.
    let written = run_timed(&brightness, async |_reg, s| {
        s.system_started(BootMode::Provisioning);
        Timer::after_millis(100).await;
        brightness.preview(100, Instant::now()).unwrap(); // while lit
        Timer::after_millis(400).await;
    });
    let blue_on = written
        .iter()
        .position(|(_, c)| c.b == 16)
        .expect("lit at the saved level");
    let (t_on, _) = written[blue_on];
    assert_eq!(
        written[blue_on + 1].1,
        RGB8::new(0, 0, 43),
        "rewritten at once at the new level"
    );
    // The dark half still ends 250 ms after the start of the animation, not 250 ms after the change.
    let (t_off, color) = written[blue_on + 2];
    assert_eq!(color, DARK);
    assert!(
        (200..=330).contains(&(t_off - t_on)),
        "dark after {} ms",
        t_off - t_on
    );
}

#[test]
fn the_setting_is_available_while_the_led_task_runs_and_not_after() {
    let brightness = Brightness::new();
    brightness.loaded(36);
    let seen = RefCell::new(None);
    let _ = run_timed(&brightness, async |_reg, _s| {
        *seen.borrow_mut() = Some(brightness.status(Instant::now()).available);
    });
    assert_eq!(*seen.borrow(), Some(true));
    // `select` dropped the task at the end: it never reached its own exit, so check the exit path too.
    let failing = Brightness::new();
    failing.loaded(36);
    let (reg, sources) = product();
    let observer = reg.observer().unwrap();
    block_on(async {
        sources.system_booting();
        run_led(observer, sources.ids(), &failing, |_: RGB8| {
            Err::<(), ()>(())
        })
        .await;
    });
    assert!(
        !failing.status(Instant::now()).available,
        "the task ended: the setting is hidden"
    );
}
