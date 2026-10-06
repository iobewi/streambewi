use iobewi_config_space::{Budget, ConfigBackend, Snapshot};
use iobewi_wifi_core::{Network, WifiProvisioning};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    task::{Context, Poll, Waker},
};
use streambewi::boot_policy::{BootMode, BootPolicy, ProvisionError, Recovery};

#[derive(Clone, Default)]
struct Backend(Rc<RefCell<Store>>);
#[derive(Default)]
struct Store {
    values: BTreeMap<String, Vec<u8>>,
    writes: Vec<String>,
    fail_load: bool,
    fail_commit: Option<&'static str>,
    fail_clear: bool,
    capacity: Option<usize>,
}
impl ConfigBackend for Backend {
    type Error = &'static str;
    fn capacity_units(&self) -> usize {
        self.0.borrow().capacity.unwrap_or(512)
    }
    fn reservation_units(&self, _: &str, budget: Budget) -> Option<usize> {
        Some(budget.max_bytes())
    }
    async fn load(&self, name: &str) -> Result<Option<Snapshot>, Self::Error> {
        let s = self.0.borrow();
        if s.fail_load {
            return Err("read failed");
        }
        Ok(s.values.get(name).map(|data| Snapshot {
            generation: 1,
            data: data.clone(),
        }))
    }
    async fn commit(&self, name: &str, data: &[u8]) -> Result<u64, Self::Error> {
        let mut s = self.0.borrow_mut();
        s.writes.push(format!("commit {name} {data:?}"));
        if s.fail_commit == Some(name) {
            return Err("commit failed");
        }
        s.values.insert(name.into(), data.to_vec());
        Ok(1)
    }
    async fn clear(&self, name: &str) -> Result<u64, Self::Error> {
        let mut s = self.0.borrow_mut();
        s.writes.push(format!("clear {name}"));
        if s.fail_clear {
            return Err("clear failed");
        }
        s.values.remove(name);
        Ok(1)
    }
}
struct Wifi {
    backend: Backend,
    connect: bool,
    gate: Option<Rc<Cell<bool>>>,
}
impl WifiProvisioning for Wifi {
    type Address = String;
    type NetworkHandle = ();
    async fn scan(&mut self) -> Vec<Network> {
        vec![]
    }
    async fn provision(&mut self, _: &str, _: String) -> bool {
        if let Some(gate) = &self.gate {
            std::future::poll_fn(|_| {
                if gate.get() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        }
        self.connect && self.backend.commit("wifi", b"credentials").await.is_ok()
    }
    fn address(&self) -> Option<String> {
        None
    }
    fn network_handle(&self) -> Option<()> {
        None
    }
    fn is_online(&self) -> bool {
        self.connect
    }
}
fn ready<F: std::future::Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("unexpected pending fixture"),
    }
}
fn seed(flag: Option<&[u8]>) -> Backend {
    let b = Backend::default();
    b.0.borrow_mut()
        .values
        .insert("wifi".into(), b"old credentials".to_vec());
    if let Some(flag) = flag {
        b.0.borrow_mut()
            .values
            .insert("usb_boot".into(), flag.to_vec());
    }
    b
}
fn wifi(backend: &Backend) -> Wifi {
    Wifi {
        backend: backend.clone(),
        connect: true,
        gate: None,
    }
}
#[test]
fn absent_false_and_true_are_distinct_boot_inputs_without_writes() {
    for (flag, mode) in [
        (None, BootMode::Provisioning),
        (Some(b"USB1\0".as_slice()), BootMode::Provisioning),
        (Some(b"USB1\x01".as_slice()), BootMode::MassStorage),
    ] {
        let b = seed(flag);
        let (p, _) = ready(BootPolicy::prepare(b.clone())).unwrap();
        assert_eq!(p.mode(), mode);
        assert!(p.read_error().is_none());
        assert!(b.0.borrow().writes.is_empty());
    }
}
#[test]
fn corrupt_unknown_version_or_oversized_flag_falls_back_without_erasing() {
    for raw in [
        b"bad".as_slice(),
        b"USB1\x02",
        b"USB2\0",
        b"USB1\x01trailing",
        b"",
    ] {
        let b = seed(Some(raw));
        let before = b.0.borrow().values.clone();
        let (p, _) = ready(BootPolicy::prepare(b.clone())).unwrap();
        assert_eq!(p.mode(), BootMode::Provisioning);
        assert!(p.read_error().is_some());
        assert_eq!(b.0.borrow().values, before);
        assert!(b.0.borrow().writes.is_empty());
    }
}
#[test]
fn read_error_is_observable_and_leaves_all_persistence_untouched() {
    let b = seed(Some(b"USB1\x01"));
    b.0.borrow_mut().fail_load = true;
    let before = b.0.borrow().values.clone();
    let (p, _) = ready(BootPolicy::prepare(b.clone())).unwrap();
    assert_eq!(p.mode(), BootMode::Provisioning);
    assert!(p.read_error().is_some());
    assert_eq!(b.0.borrow().values, before);
    assert!(b.0.borrow().writes.is_empty());
}
#[test]
fn insufficient_combined_budget_stops_startup_without_writes() {
    let b = seed(None);
    b.0.borrow_mut().capacity = Some(128);
    assert!(ready(BootPolicy::prepare(b.clone())).is_err());
    assert!(b.0.borrow().writes.is_empty());
}
#[test]
fn successful_provisioning_commits_credentials_then_flag_and_changes_only_next_boot() {
    let b = seed(None);
    let (p, _) = ready(BootPolicy::prepare(b.clone())).unwrap();
    ready(p.provision(&mut wifi(&b), "home", "pw".into())).unwrap();
    let s = b.0.borrow();
    assert!(s.writes[0].starts_with("commit wifi "));
    assert!(s.writes[1].starts_with("commit usb_boot "));
    drop(s);
    assert_eq!(p.mode(), BootMode::Provisioning);
    let (next, _) = ready(BootPolicy::prepare(b)).unwrap();
    assert_eq!(next.mode(), BootMode::MassStorage);
}
#[test]
fn connection_or_credential_commit_failure_never_writes_the_flag() {
    for failure in ["connection", "wifi"] {
        let b = seed(None);
        let (p, _) = ready(BootPolicy::prepare(b.clone())).unwrap();
        let mut w = wifi(&b);
        if failure == "connection" {
            w.connect = false;
        } else {
            b.0.borrow_mut().fail_commit = Some("wifi");
        }
        assert!(matches!(
            ready(p.provision(&mut w, "home", "pw".into())),
            Err(ProvisionError::Credentials)
        ));
        assert!(!b.0.borrow().values.contains_key("usb_boot"));
        assert!(
            b.0.borrow()
                .writes
                .iter()
                .all(|w| !w.starts_with("commit usb_boot "))
        );
    }
}
#[test]
fn flag_failure_reports_failure_with_saved_credentials_and_reprovisioning_retries() {
    let b = seed(None);
    let (p, _) = ready(BootPolicy::prepare(b.clone())).unwrap();
    b.0.borrow_mut().fail_commit = Some("usb_boot");
    assert!(matches!(
        ready(p.provision(&mut wifi(&b), "home", "pw".into())),
        Err(ProvisionError::Flag(_))
    ));
    assert_eq!(b.0.borrow().values["wifi"], b"credentials");
    assert!(!b.0.borrow().values.contains_key("usb_boot"));
    assert_eq!(
        ready(BootPolicy::prepare(b.clone())).unwrap().0.mode(),
        BootMode::Provisioning
    );
    b.0.borrow_mut().fail_commit = None;
    ready(p.provision(&mut wifi(&b), "home", "pw".into())).unwrap();
    assert_eq!(
        ready(BootPolicy::prepare(b.clone())).unwrap().0.mode(),
        BootMode::MassStorage
    );
    assert_eq!(b.0.borrow().writes.len(), 4);
}
#[test]
fn recovery_commits_false_before_clearing_and_changes_only_next_boot() {
    let b = seed(Some(b"USB1\x01"));
    let (p, _) = ready(BootPolicy::prepare(b.clone())).unwrap();
    assert!(matches!(ready(p.recover()), Ok(Recovery::Cleared)));
    assert!(b.0.borrow().writes[0].starts_with("commit usb_boot "));
    assert_eq!(b.0.borrow().writes[1], "clear wifi");
    assert!(!b.0.borrow().values.contains_key("wifi"));
    assert_eq!(p.mode(), BootMode::MassStorage);
    assert_eq!(
        ready(BootPolicy::prepare(b)).unwrap().0.mode(),
        BootMode::Provisioning
    );
}
#[test]
fn failed_recovery_flag_commit_preserves_credentials_and_does_not_allow_restart() {
    let b = seed(Some(b"USB1\x01"));
    let (p, _) = ready(BootPolicy::prepare(b.clone())).unwrap();
    b.0.borrow_mut().fail_commit = Some("usb_boot");
    let before = b.0.borrow().values.clone();
    assert!(ready(p.recover()).is_err());
    assert_eq!(b.0.borrow().values, before);
    assert_eq!(b.0.borrow().writes.len(), 1);
}
#[test]
fn failed_clear_after_false_is_valid_provisioning_with_residual_credentials() {
    let b = seed(Some(b"USB1\x01"));
    let (p, _) = ready(BootPolicy::prepare(b.clone())).unwrap();
    b.0.borrow_mut().fail_clear = true;
    assert!(matches!(
        ready(p.recover()),
        Ok(Recovery::ResidualCredentials("clear failed"))
    ));
    assert!(b.0.borrow().values.contains_key("wifi"));
    assert_eq!(
        ready(BootPolicy::prepare(b)).unwrap().0.mode(),
        BootMode::Provisioning
    );
}
#[test]
fn recovery_cannot_interleave_between_credential_and_flag_commits() {
    let b = seed(None);
    let (p, _) = ready(BootPolicy::prepare(b.clone())).unwrap();
    let gate = Rc::new(Cell::new(false));
    let mut w = wifi(&b);
    w.gate = Some(gate.clone());
    let mut provision = std::pin::pin!(p.provision(&mut w, "home", "pw".into()));
    let mut recovery = std::pin::pin!(p.recover());
    let mut cx = Context::from_waker(Waker::noop());
    assert!(provision.as_mut().poll(&mut cx).is_pending());
    assert!(recovery.as_mut().poll(&mut cx).is_pending());
    assert!(b.0.borrow().writes.is_empty());
    gate.set(true);
    assert!(matches!(
        provision.as_mut().poll(&mut cx),
        Poll::Ready(Ok(()))
    ));
    assert!(matches!(
        recovery.as_mut().poll(&mut cx),
        Poll::Ready(Ok(Recovery::Cleared))
    ));
    let writes = &b.0.borrow().writes;
    assert_eq!(writes.len(), 4);
    assert!(writes[0].starts_with("commit wifi "));
    assert!(writes[1].starts_with("commit usb_boot "));
    assert!(writes[2].starts_with("commit usb_boot "));
    assert_eq!(writes[3], "clear wifi");
}
