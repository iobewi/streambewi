//! The protected save of the settings: when the disk is on the bus the write happens only while it is
//! off, a station change in the meantime keeps the disk, and nothing of the stream connection is touched.

use std::boxed::Box;
use std::string::String;
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use embassy_futures::{block_on, join::join, select::select};
use iobewi_config_space::{Budget, ConfigBackend, ConfigManager, Snapshot};

use super::*;
use crate::status::{Reg, Sources};
use crate::usb::UsbLink;

type Events = Arc<Mutex<Vec<String>>>;

fn events(log: &Events) -> Vec<String> {
    log.lock().unwrap().clone()
}

struct Link(Events);
impl UsbLink for Link {
    fn set_connected(&self, connected: bool) {
        self.0
            .lock()
            .unwrap()
            .push(if connected { "link:on" } else { "link:off" }.into());
    }
}

/// A backend that logs its commits in the same list as the link.
#[derive(Clone)]
struct Backend {
    log: Events,
    fail: Arc<Mutex<bool>>,
    bytes: Arc<Mutex<Option<Vec<u8>>>>,
}
impl ConfigBackend for Backend {
    type Error = ();
    fn capacity_units(&self) -> usize {
        4096
    }
    fn reservation_units(&self, _: &str, budget: Budget) -> Option<usize> {
        Some(budget.max_bytes())
    }
    async fn load(&self, _: &str) -> Result<Option<Snapshot>, ()> {
        Ok(self.bytes.lock().unwrap().clone().map(|data| Snapshot {
            generation: 1,
            data,
        }))
    }
    async fn commit(&self, _: &str, data: &[u8]) -> Result<u64, ()> {
        if *self.fail.lock().unwrap() {
            return Err(());
        }
        self.log.lock().unwrap().push("commit".into());
        *self.bytes.lock().unwrap() = Some(data.to_vec());
        Ok(1)
    }
    async fn clear(&self, _: &str) -> Result<u64, ()> {
        unreachable!()
    }
}

struct Fixture {
    log: Events,
    fail: Arc<Mutex<bool>>,
    settings: SettingsStore<Backend>,
    stream: &'static SharedStream,
    link: &'static Link,
    reg: &'static Reg,
    sources: Sources<'static>,
}

fn fixture() -> Fixture {
    let log: Events = Arc::default();
    let fail = Arc::new(Mutex::new(false));
    let backend = Backend {
        log: log.clone(),
        fail: fail.clone(),
        bytes: Arc::default(),
    };
    let mut manager = ConfigManager::new(backend);
    let space = manager
        .claim(crate::settings::SPACE, crate::settings::CONFIG_BUDGET)
        .unwrap();
    let (settings, _) = block_on(SettingsStore::load(space));
    let reg: &'static Reg = Box::leak(Box::new(Reg::new()));
    let sources = Sources::register(reg).unwrap();
    Fixture {
        link: Box::leak(Box::new(Link(log.clone()))),
        stream: Box::leak(Box::new(SharedStream::new())),
        log,
        fail,
        settings,
        reg,
        sources,
    }
}

impl Fixture {
    fn watch(&self) -> Option<crate::status::Disk<'_>> {
        Some(crate::status::Disk {
            observer: self.reg.observer().unwrap(),
            sources: &self.sources,
        })
    }
    fn save(&self, link: bool, percent: u8) -> (Result<(), SaveError>, std::time::Duration) {
        let started = std::time::Instant::now();
        let mut watch = self.watch();
        let link: Option<&'static dyn UsbLink> = link.then_some(self.link as &'static dyn UsbLink);
        let result = block_on(save_once(
            &self.settings,
            percent,
            self.stream,
            link,
            &mut watch,
        ));
        (result, started.elapsed())
    }
    fn usb(&self) -> Flags {
        self.reg.snapshot().of(self.sources.ids().usb).unwrap()
    }
}

#[test]
fn without_a_disk_the_settings_are_written_at_once() {
    let f = fixture();
    let (result, took) = f.save(false, 70);
    assert_eq!(result, Ok(()));
    assert_eq!(events(&f.log), ["commit"]);
    assert!(took < std::time::Duration::from_millis(500), "{took:?}");
    assert_eq!(block_on(f.settings.led_percent()), 70);
}

#[test]
fn a_disk_not_presented_yet_is_not_detached() {
    for prepare in [
        |_: &Sources<'_>| {},
        |s: &Sources<'_>| s.usb_initializing(),
        |s: &Sources<'_>| s.usb_error(),
    ] {
        let f = fixture();
        prepare(&f.sources);
        let (result, _) = f.save(true, 70);
        assert_eq!(result, Ok(()));
        assert_eq!(
            events(&f.log),
            ["commit"],
            "no host is on the bus to be disturbed"
        );
    }
}

#[test]
fn with_the_disk_on_the_bus_the_write_happens_only_while_it_is_off() {
    let f = fixture();
    f.sources.usb_ready();
    let (result, took) = f.save(true, 70);
    assert_eq!(result, Ok(()));
    // Off the bus, written, back on: in this order, and the commit never outside the window.
    assert_eq!(events(&f.log), ["link:off", "commit", "link:on"]);
    // The disk stays off for the usual 1.5 s of a station change.
    assert!(took >= std::time::Duration::from_millis(1_450), "{took:?}");
    assert_eq!(f.usb(), Flags::of(&[State::Ready]), "back on the bus");
    assert!(
        f.stream.hold.load(Ordering::Acquire),
        "the session is kept across the reconnection, as for a stall"
    );
    assert_eq!(block_on(f.settings.led_percent()), 70);
}

#[test]
fn a_failed_write_brings_the_disk_back_and_keeps_the_previous_value() {
    let f = fixture();
    f.sources.usb_ready();
    assert_eq!(f.save(true, 60).0, Ok(()));
    f.log.lock().unwrap().clear();
    *f.fail.lock().unwrap() = true;
    let (result, _) = f.save(true, 10);
    assert_eq!(result, Err(SaveError::Storage));
    assert_eq!(
        events(&f.log),
        ["link:off", "link:on"],
        "no commit went through"
    );
    assert_eq!(f.usb(), Flags::of(&[State::Ready]));
    assert_eq!(block_on(f.settings.led_percent()), 60);
}

#[test]
fn a_save_waits_while_another_actor_has_the_disk_off_the_bus() {
    let f = fixture();
    f.sources.usb_ready();
    f.sources.usb_off_bus(); // a station change or a stall recovery has the disk
    let started = std::time::Instant::now();
    let mut watch = f.watch();
    let link: Option<&'static dyn UsbLink> = Some(f.link);
    let (result, ()) = block_on(join(
        save_once(&f.settings, 70, f.stream, link, &mut watch),
        async {
            Timer::after(Duration::from_millis(200)).await;
            assert!(
                events(&f.log).is_empty(),
                "nothing happened while the disk was away"
            );
            f.sources.usb_ready();
        },
    ));
    assert_eq!(result, Ok(()));
    assert_eq!(events(&f.log), ["link:off", "commit", "link:on"]);
    assert!(started.elapsed() >= std::time::Duration::from_millis(1_600));
}

#[test]
fn a_station_change_during_the_window_keeps_the_disk_and_the_write_still_happens() {
    let f = fixture();
    f.sources.usb_ready();
    let mut watch = f.watch();
    let link: Option<&'static dyn UsbLink> = Some(f.link);
    let (result, ()) = block_on(join(
        save_once(&f.settings, 70, f.stream, link, &mut watch),
        async {
            Timer::after(Duration::from_millis(300)).await;
            // What the station change does: a new generation, the disk stays off until it rejoins itself.
            f.stream.begin_station_change();
        },
    ));
    assert_eq!(result, Ok(()));
    assert_eq!(
        events(&f.log),
        ["link:off", "commit"],
        "the save does not bring the disk back: the station change owns it"
    );
    assert_eq!(f.usb(), Flags::of(&[State::Unavailable]));
}

#[test]
fn the_stream_is_not_touched() {
    let f = fixture();
    f.sources.usb_ready();
    let before = (f.stream.generation(), f.stream.progress());
    let _ = f.save(true, 70);
    assert_eq!((f.stream.generation(), f.stream.progress()), before);
    assert_eq!(
        f.stream.reconnects.load(Ordering::Relaxed),
        0,
        "no reconnection"
    );
}

#[test]
fn the_saver_does_the_requests_in_order_and_reports_each_outcome() {
    let f = fixture();
    let brightness = Brightness::new();
    brightness.loaded(36);
    // No disk: quick.
    let driver = async {
        brightness.request_save(70).unwrap();
        while brightness.status(Instant::now()).busy {
            Timer::after(Duration::from_millis(10)).await;
        }
        let first = brightness.status(Instant::now());
        assert_eq!((first.saved, first.error), (70, None));

        *f.fail.lock().unwrap() = true;
        brightness.request_save(5).unwrap();
        while brightness.status(Instant::now()).busy {
            Timer::after(Duration::from_millis(10)).await;
        }
        let second = brightness.status(Instant::now());
        assert_eq!(second.saved, 70, "the previous value is kept");
        assert_eq!(second.error, Some(SaveError::Storage));
    };
    block_on(select(
        save_settings(&f.settings, &brightness, f.stream, None),
        driver,
    ));
    assert_eq!(
        events(&f.log),
        ["commit"],
        "one write went through, one failed"
    );
}
