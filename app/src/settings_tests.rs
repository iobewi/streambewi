use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::vec::Vec;

use embassy_futures::block_on;
use iobewi_config_space::{ConfigBackend, ConfigManager, Snapshot};

use super::*;

/// A backend holding one record, which can be made to fail.
#[derive(Clone, Default)]
struct Backend {
    bytes: Rc<RefCell<Option<Vec<u8>>>>,
    fail_write: Rc<Cell<bool>>,
    fail_read: Rc<Cell<bool>>,
    commits: Rc<Cell<usize>>,
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
        if self.fail_read.get() {
            return Err(());
        }
        Ok(self.bytes.borrow().as_ref().map(|data| Snapshot {
            generation: 1,
            data: data.clone(),
        }))
    }
    async fn commit(&self, _: &str, data: &[u8]) -> Result<u64, ()> {
        if self.fail_write.get() {
            return Err(());
        }
        self.commits.set(self.commits.get() + 1);
        *self.bytes.borrow_mut() = Some(data.to_vec());
        Ok(1)
    }
    async fn clear(&self, _: &str) -> Result<u64, ()> {
        unreachable!("the settings are never cleared")
    }
}

fn store(backend: &Backend) -> (SettingsStore<Backend>, Loaded) {
    block_on(async {
        let mut manager = ConfigManager::new(backend.clone());
        let space = manager.claim(SPACE, CONFIG_BUDGET).unwrap();
        SettingsStore::load(space).await
    })
}

#[test]
fn an_absent_record_gives_the_default_and_loading_writes_nothing() {
    let backend = Backend::default();
    let (settings, loaded) = store(&backend);
    assert_eq!(loaded, Loaded::Absent);
    assert_eq!(
        block_on(settings.led_percent()),
        brightness::DEFAULT_PERCENT
    );
    assert_eq!(backend.commits.get(), 0);
    assert!(backend.bytes.borrow().is_none());
}

#[test]
fn a_saved_value_is_one_commit_of_the_whole_record_and_survives_a_reload() {
    let backend = Backend::default();
    let (settings, _) = store(&backend);
    block_on(settings.save_led_percent(0)).unwrap();
    assert_eq!(backend.commits.get(), 1);
    assert_eq!(
        backend.bytes.borrow().as_deref(),
        Some(&b"SET1\x01\x01\x00"[..])
    );
    assert_eq!(block_on(settings.led_percent()), 0);

    // A restart: the record is read back.
    let (again, loaded) = store(&backend);
    assert_eq!(loaded, Loaded::Record);
    assert_eq!(
        block_on(again.led_percent()),
        0,
        "0 % is a value, not an absence"
    );
    block_on(again.save_led_percent(100)).unwrap();
    assert_eq!(
        backend.bytes.borrow().as_deref(),
        Some(&b"SET1\x01\x01\x64"[..])
    );
}

#[test]
fn a_failed_commit_keeps_the_previous_value() {
    let backend = Backend::default();
    let (settings, _) = store(&backend);
    block_on(settings.save_led_percent(70)).unwrap();
    backend.fail_write.set(true);
    assert_eq!(
        block_on(settings.save_led_percent(10)),
        Err(SaveError::Storage)
    );
    assert_eq!(
        block_on(settings.led_percent()),
        70,
        "RAM follows the storage, not the request"
    );
    assert_eq!(
        backend.bytes.borrow().as_deref(),
        Some(&b"SET1\x01\x01\x46"[..])
    );
    backend.fail_write.set(false);
    block_on(settings.save_led_percent(10)).unwrap();
    assert_eq!(block_on(settings.led_percent()), 10);
}

#[test]
fn a_value_out_of_range_is_not_written() {
    let backend = Backend::default();
    let (settings, _) = store(&backend);
    assert_eq!(
        block_on(settings.save_led_percent(101)),
        Err(SaveError::Storage)
    );
    assert_eq!(backend.commits.get(), 0);
}

#[test]
fn an_invalid_record_gives_the_default_without_panic_or_erasure() {
    for (bytes, loaded, percent) in [
        (&b"junk"[..], Loaded::Unreadable, 36),
        (&b""[..], Loaded::Unreadable, 36),
        (&b"SET2\x01\x01\x10"[..], Loaded::Unreadable, 36),
        (&b"SET1\x01"[..], Loaded::Unreadable, 36),
        (&b"SET1\x01\x05\x10"[..], Loaded::Unreadable, 36),
        // Known entry, unusable value: absent.
        (&b"SET1\x01\x01\xC8"[..], Loaded::Record, 36),
        (&b"SET1\x01\x02\x10\x10"[..], Loaded::Record, 36),
        (&b"SET1\x01\x00"[..], Loaded::Record, 36),
        // Valid.
        (&b"SET1"[..], Loaded::Record, 36),
        (&b"SET1\x01\x01\x00"[..], Loaded::Record, 0),
    ] {
        let backend = Backend::default();
        *backend.bytes.borrow_mut() = Some(bytes.to_vec());
        let (settings, how) = store(&backend);
        assert_eq!(how, loaded, "{bytes:?}");
        assert_eq!(block_on(settings.led_percent()), percent, "{bytes:?}");
        assert_eq!(backend.commits.get(), 0, "loading never writes");
        assert_eq!(
            backend.bytes.borrow().as_deref(),
            Some(bytes),
            "nothing is erased"
        );
    }
}

#[test]
fn an_unreadable_storage_gives_the_default_and_can_still_be_saved() {
    let backend = Backend::default();
    backend.fail_read.set(true);
    let (settings, loaded) = store(&backend);
    assert_eq!(loaded, Loaded::StorageError);
    assert_eq!(
        block_on(settings.led_percent()),
        brightness::DEFAULT_PERCENT
    );
    backend.fail_read.set(false);
    block_on(settings.save_led_percent(55)).unwrap();
    assert_eq!(block_on(settings.led_percent()), 55);
}

#[test]
fn a_setting_this_firmware_does_not_know_survives_the_save_of_another() {
    // Entry 7, three bytes, written by a later firmware, before and after the brightness.
    let backend = Backend::default();
    *backend.bytes.borrow_mut() = Some(b"SET1\x07\x03abc\x01\x01\x10\x09\x01Z".to_vec());
    let (settings, loaded) = store(&backend);
    assert_eq!(loaded, Loaded::Record);
    assert_eq!(block_on(settings.led_percent()), 16);
    block_on(settings.save_led_percent(90)).unwrap();
    let record = Values::decode(backend.bytes.borrow().as_deref().unwrap()).unwrap();
    assert_eq!(record.led_percent, Some(90));
    let bytes = backend.bytes.borrow().clone().unwrap();
    assert!(bytes.windows(5).any(|w| w == b"\x07\x03abc"), "{bytes:?}");
    assert!(bytes.windows(3).any(|w| w == b"\x09\x01Z"), "{bytes:?}");
    assert!(bytes.len() <= CONFIG_BUDGET.max_bytes());
}

#[test]
fn the_record_round_trips() {
    for percent in [None, Some(0), Some(36), Some(100)] {
        let values = Values {
            led_percent: percent,
            other: b"\x05\x02xy".to_vec(),
        };
        assert_eq!(Values::decode(&values.encode()), Some(values));
    }
}

// --- The manifest on a partition that already holds the four earlier spaces -------------------------

mod partition {
    use super::*;
    use crate::boot_policy::{BootPolicy, SPACES};
    use ekv::flash::{Flash, MemFlash, PageID};
    use iobewi_config_space::storage::{SpaceSpec, Store};

    /// One flash that several stores, one after the other, can open: the partition across reboots.
    #[derive(Clone)]
    struct Shared(Rc<RefCell<MemFlash>>);

    impl Flash for Shared {
        type Error = core::convert::Infallible;
        fn page_count(&self) -> usize {
            self.0.borrow().page_count()
        }
        async fn erase(&mut self, page: PageID) -> Result<(), Self::Error> {
            let mut flash = std::mem::replace(&mut *self.0.borrow_mut(), MemFlash::new());
            let result = flash.erase(page).await;
            *self.0.borrow_mut() = flash;
            result
        }
        async fn read(
            &mut self,
            page: PageID,
            offset: usize,
            data: &mut [u8],
        ) -> Result<(), Self::Error> {
            let mut flash = std::mem::replace(&mut *self.0.borrow_mut(), MemFlash::new());
            let result = flash.read(page, offset, data).await;
            *self.0.borrow_mut() = flash;
            result
        }
        async fn write(
            &mut self,
            page: PageID,
            offset: usize,
            data: &[u8],
        ) -> Result<(), Self::Error> {
            let mut flash = std::mem::replace(&mut *self.0.borrow_mut(), MemFlash::new());
            let result = flash.write(page, offset, data).await;
            *self.0.borrow_mut() = flash;
            result
        }
    }

    /// The manifest of the firmware before the settings existed: the first four spaces, in place.
    static BEFORE: &[SpaceSpec] = &[SPACES[0], SPACES[1], SPACES[2], SPACES[3]];

    const WIFI: &[u8] = b"wifi-record";
    const USB: &[u8] = b"USB1\x01";
    const STATIONS: &[u8] = b"{\"stations\":\"kept\"}";
    const LOG: &[u8] = b"log:\n  default_level: info\n";

    #[test]
    fn the_manifest_names_the_four_spaces_in_place_then_the_settings() {
        let names: Vec<&str> = SPACES.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            ["stations", "wifi", "usb_boot", "log", "settings", "ota"]
        );
        assert_eq!(BEFORE.len(), 4);
        // The USB boot format is the one it was: 5 bytes, not touched by the settings.
        assert_eq!(SPACES[2].max_bytes, 5);
        assert_eq!(SPACES[1].max_bytes, 128);
        assert_eq!(SPACES[3].max_bytes, 1024);
    }

    #[test]
    fn the_spaces_fit_the_partition_to_the_page() {
        // The `config` partition of the OTA layout has 128 pages, 16 kept in reserve: 112 reservable.
        let flash = Shared(Rc::new(RefCell::new(MemFlash::new())));
        let pages = flash.page_count();
        assert!(pages >= 128, "{pages}");
        let reserve =
            |spec: &SpaceSpec| 4 * ((16 + spec.name.len() + spec.max_bytes).div_ceil(2048) + 1);
        let total = SPACES.iter().map(reserve).sum::<usize>();
        assert_eq!(total, 60, "the six spaces, with the OTA metadata");
        assert!(total + 16 <= 128, "within the 128-page partition");
        assert_eq!(BEFORE.iter().map(reserve).sum::<usize>(), 40);
        assert_eq!(reserve(&SPACES[4]), 8, "the settings reserve the minimum");
        assert_eq!(reserve(&SPACES[5]), 12, "the OTA metadata");
    }

    /// Writes a distinct value in each of the four earlier spaces, as the earlier firmware would have.
    fn write_before(flash: &Shared) {
        block_on(async {
            let store = Store::new(flash.clone(), 7, BEFORE).unwrap();
            store.format().await.unwrap();
            store.mount().await.unwrap();
            for (name, data) in [
                ("wifi", WIFI),
                ("usb_boot", USB),
                ("stations", STATIONS),
                ("log", LOG),
            ] {
                let mut manager = ConfigManager::new(store.backend());
                let budget = BEFORE.iter().find(|s| s.name == name).unwrap().max_bytes;
                let space = manager.claim(name, Budget::new(budget)).unwrap();
                space.commit(data).await.unwrap();
            }
        });
    }

    #[test]
    fn a_partition_holding_the_four_spaces_mounts_with_the_five_and_keeps_every_byte() {
        let flash = Shared(Rc::new(RefCell::new(MemFlash::new())));
        write_before(&flash);

        block_on(async {
            // The new firmware: the new manifest over the same flash. No format, no write to mount.
            let writes = flash.0.borrow().write_count;
            let store = Store::new(flash.clone(), 9, SPACES).unwrap();
            store.mount().await.unwrap();
            assert_eq!(
                flash.0.borrow().write_count,
                writes,
                "mounting writes nothing"
            );

            for (name, data) in [
                ("wifi", WIFI),
                ("usb_boot", USB),
                ("stations", STATIONS),
                ("log", LOG),
            ] {
                let mut manager = ConfigManager::new(store.backend());
                let budget = SPACES.iter().find(|s| s.name == name).unwrap().max_bytes;
                let space = manager.claim(name, Budget::new(budget)).unwrap();
                assert_eq!(space.load().await.unwrap().unwrap().data, data, "{name}");
            }

            // The settings: absent first, then saved, through the product's own claim sequence.
            let space = BootPolicy::claim_settings(store.backend()).unwrap();
            let (settings, loaded) = SettingsStore::load(space).await;
            assert_eq!(loaded, Loaded::Absent);
            assert_eq!(settings.led_percent().await, brightness::DEFAULT_PERCENT);
            settings.save_led_percent(0).await.unwrap();

            // The earlier four are still as they were, and so is the format of `usb_boot`.
            for (name, data) in [
                ("wifi", WIFI),
                ("usb_boot", USB),
                ("stations", STATIONS),
                ("log", LOG),
            ] {
                let mut manager = ConfigManager::new(store.backend());
                let budget = SPACES.iter().find(|s| s.name == name).unwrap().max_bytes;
                let space = manager.claim(name, Budget::new(budget)).unwrap();
                assert_eq!(
                    space.load().await.unwrap().unwrap().data,
                    data,
                    "{name} after the save"
                );
            }
        });

        // A restart of the new firmware reads the saved value.
        block_on(async {
            let store = Store::new(flash.clone(), 11, SPACES).unwrap();
            store.mount().await.unwrap();
            let space = BootPolicy::claim_settings(store.backend()).unwrap();
            let (settings, loaded) = SettingsStore::load(space).await;
            assert_eq!(loaded, Loaded::Record);
            assert_eq!(settings.led_percent().await, 0);
        });
    }

    #[test]
    fn an_earlier_firmware_cannot_mount_a_partition_that_holds_the_settings() {
        // The limit to document: once the settings are saved, the manifest of the earlier firmware refuses
        // the store (an unknown space), and the product halts at `STREAMBEWI:CONFIG-MOUNT` without
        // reformatting anything. The data is not lost: the new firmware mounts it again.
        let flash = Shared(Rc::new(RefCell::new(MemFlash::new())));
        write_before(&flash);
        block_on(async {
            let store = Store::new(flash.clone(), 1, SPACES).unwrap();
            store.mount().await.unwrap();
            let space = BootPolicy::claim_settings(store.backend()).unwrap();
            let (settings, _) = SettingsStore::load(space).await;
            settings.save_led_percent(20).await.unwrap();
        });
        block_on(async {
            let old = Store::new(flash.clone(), 2, BEFORE).unwrap();
            let refused = old.mount().await;
            assert!(
                refused.is_err(),
                "the earlier manifest does not know `settings`"
            );
            let new = Store::new(flash.clone(), 3, SPACES).unwrap();
            new.mount().await.unwrap();
        });
    }
}
