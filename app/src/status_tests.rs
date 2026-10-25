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
            color: RGB8::new(24, 0, 0),
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

#[test]
fn the_colours_are_dim() {
    let (reg, s) = product();
    s.system_booting();
    let Pattern::Solid(white) = look(&reg.snapshot(), &s.ids()).pattern else {
        panic!("steady white at boot");
    };
    assert_eq!(iobewi_indicator::level(white), 16);
    assert_eq!(iobewi_indicator::level(BLUE), 16);
    assert_eq!(iobewi_indicator::level(GREEN), 16);
    assert_eq!(iobewi_indicator::level(ORANGE), 24);
    assert_eq!(iobewi_indicator::level(RED), 24);
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
    let (_observer, ids) = led_observer().expect("an observer for the LED");
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
    let sink = |color: RGB8| {
        let mut written = written.borrow_mut();
        if fail_after.is_some_and(|n| written.len() >= n) {
            return Err(());
        }
        written.push(color);
        Ok(())
    };
    let task = run_led(observer, ids, sink);
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
    let ended = block_on(select(run_led(observer, ids, sink), driver));
    assert!(
        matches!(ended, Either::First(())),
        "the task ended on its own"
    );
    // The registry goes on working after the LED is gone.
    s.wifi_link_down();
    assert_eq!(flags(reg, ids.wifi), Flags::of(&[State::Reconnecting]));
}
