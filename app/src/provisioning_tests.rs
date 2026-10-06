use super::*;
use iobewi_config_space::{Budget, Snapshot};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    task::{Context, Poll, Waker},
};
#[derive(Clone, Default)]
struct Backend(Rc<RefCell<Store>>);
#[derive(Default)]
struct Store {
    values: BTreeMap<String, Vec<u8>>,
    fail_flag: bool,
}
impl ConfigBackend for Backend {
    type Error = ();
    fn capacity_units(&self) -> usize {
        512
    }
    fn reservation_units(&self, _: &str, budget: Budget) -> Option<usize> {
        Some(budget.max_bytes())
    }
    async fn load(&self, name: &str) -> Result<Option<Snapshot>, ()> {
        Ok(self.0.borrow().values.get(name).map(|data| Snapshot {
            generation: 1,
            data: data.clone(),
        }))
    }
    async fn commit(&self, name: &str, data: &[u8]) -> Result<u64, ()> {
        let mut s = self.0.borrow_mut();
        if name == "usb_boot" && s.fail_flag {
            return Err(());
        }
        s.values.insert(name.into(), data.to_vec());
        Ok(1)
    }
    async fn clear(&self, name: &str) -> Result<u64, ()> {
        self.0.borrow_mut().values.remove(name);
        Ok(1)
    }
}
struct Radio;
impl WifiTransport for Radio {
    type Address = String;
    type NetworkHandle = Stack<'static>;
    async fn connect(&mut self, _: &str, _: String) -> bool {
        true
    }
    async fn scan(&mut self) -> Vec<iobewi_wifi_core::Network> {
        vec![]
    }
    async fn wait_down(&mut self) {
        core::future::pending::<()>().await;
    }
    fn ip(&self) -> Option<String> {
        None
    }
    fn network_handle(&self) -> Option<Stack<'static>> {
        None
    }
    fn is_online(&self) -> bool {
        true
    }
}
#[derive(Default)]
struct Tx(Vec<u8>);
impl ErrorType for Tx {
    type Error = Infallible;
}
impl Write for Tx {
    async fn write(&mut self, data: &[u8]) -> Result<usize, Infallible> {
        self.0.extend_from_slice(data);
        Ok(data.len())
    }
    async fn flush(&mut self) -> Result<(), Infallible> {
        Ok(())
    }
}
fn ready<F: core::future::Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("unexpected pending"),
    }
}
#[test]
fn flag_failure_sends_an_improv_error_on_the_requesting_port_and_retry_succeeds() {
    for port in [Port::A, Port::B] {
        let backend = Backend::default();
        let (boot, wifi_space) = ready(BootPolicy::prepare(backend.clone())).unwrap();
        let mut manager = WifiManager::new(Radio, wifi_space);
        let mut ports = Ports {
            a: Tx::default(),
            b: Tx::default(),
        };
        let mut state = State::Authorized;
        let mut flag_committed = false;
        backend.0.borrow_mut().fail_flag = true;
        let settings = || {
            ParsedCommand::WifiSettings(improv::WifiSettings {
                ssid: "home".into(),
                password: "pw".into(),
            })
        };
        ready(handle(
            settings(),
            Reply {
                ports: &mut ports,
                port,
            },
            &mut state,
            &mut manager,
            b"fake",
            &boot,
            &mut flag_committed,
        ));
        assert_eq!(state, State::Authorized);
        assert!(!flag_committed);
        assert!(backend.0.borrow().values.contains_key("wifi"));
        let expected = [
            improv::state_frame(State::Provisioning),
            improv::error_frame(ImprovError::UnableToConnect),
            improv::state_frame(State::Authorized),
        ]
        .concat();
        let (request, other) = match port {
            Port::A => (&ports.a.0, &ports.b.0),
            Port::B => (&ports.b.0, &ports.a.0),
        };
        assert_eq!(*request, expected);
        assert!(other.is_empty());
        backend.0.borrow_mut().fail_flag = false;
        ports.a.0.clear();
        ports.b.0.clear();
        ready(handle(
            settings(),
            Reply {
                ports: &mut ports,
                port,
            },
            &mut state,
            &mut manager,
            b"fake",
            &boot,
            &mut flag_committed,
        ));
        assert_eq!(state, State::Provisioned);
        assert!(flag_committed);
        let expected = [
            improv::state_frame(State::Provisioning),
            improv::state_frame(State::Provisioned),
            improv::rpc_response_frame(Command::WifiSettings, &[]),
        ]
        .concat();
        let (request, other) = match port {
            Port::A => (&ports.a.0, &ports.b.0),
            Port::B => (&ports.b.0, &ports.a.0),
        };
        assert_eq!(*request, expected);
        assert!(other.is_empty());
        assert_eq!(boot.mode(), BootMode::Provisioning);
        assert_eq!(
            ready(BootPolicy::prepare(backend)).unwrap().0.mode(),
            BootMode::MassStorage
        );
    }
}
