//! Product-owned persisted USB boot policy. USB selection is immutable this boot.
//! Credentials and mode are separate commits, deliberately not a transaction.
use alloc::string::String;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex};
use iobewi_config_space::{
    Budget, ClaimError, ConfigBackend, ConfigManager, ConfigSpace, SpaceError,
};
use iobewi_wifi_core::WifiProvisioning;
use iobewi_wifi_manager::CONFIG_BUDGET;

const SPACE: &str = "usb_boot";
const FALSE: &[u8] = b"USB1\x00";
const TRUE: &[u8] = b"USB1\x01";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootMode {
    Provisioning,
    MassStorage,
}
#[derive(Debug)]
pub enum ReadError<E> {
    Storage(SpaceError<E>),
    InvalidFlag,
}
#[derive(Debug)]
pub enum ProvisionError<E> {
    Credentials,
    Flag(SpaceError<E>),
}
#[derive(Debug)]
pub enum Recovery<E> {
    Cleared,
    /// Valid provisioning state; recovery may restart, but must report this error.
    ResidualCredentials(E),
}
/// Created once before USB selection; copies of the backend share persistence.
/// The mutex serializes reprovisioning with recovery on the product executor.
pub struct BootPolicy<B: ConfigBackend> {
    mode: BootMode,
    read_error: Option<ReadError<B::Error>>,
    flag: ConfigSpace<B>,
    backend: B,
    writes: Mutex<NoopRawMutex, ()>,
}
impl<B: ConfigBackend> BootPolicy<B> {
    /// Claims both budgets together. Admission failure is a startup error, while
    /// unreadable/invalid flag falls back without writing or clearing anything.
    pub async fn prepare(backend: B) -> Result<(Self, ConfigSpace<B>), ClaimError> {
        let mut manager = ConfigManager::new(backend.clone());
        let wifi = manager.claim("wifi", CONFIG_BUDGET)?;
        let flag = manager.claim(SPACE, Budget::new(TRUE.len()))?;
        let (mode, read_error) = match flag.load().await {
            Ok(None) => (BootMode::Provisioning, None),
            Ok(Some(value)) if value.data == FALSE => (BootMode::Provisioning, None),
            Ok(Some(value)) if value.data == TRUE => (BootMode::MassStorage, None),
            Ok(Some(_)) => (BootMode::Provisioning, Some(ReadError::InvalidFlag)),
            Err(error) => (BootMode::Provisioning, Some(ReadError::Storage(error))),
        };
        Ok((
            Self {
                mode,
                read_error,
                flag,
                backend,
                writes: Mutex::new(()),
            },
            wifi,
        ))
    }
    pub fn mode(&self) -> BootMode {
        self.mode
    }
    pub fn read_error(&self) -> Option<&ReadError<B::Error>> {
        self.read_error.as_ref()
    }
    /// Wi-Fi provisioning succeeds only after its credential commit. A flag
    /// failure is returned as failure, even if credentials now exist. Reprovision
    /// retries both operations; no automatic restart or live USB switch occurs.
    pub async fn provision<P: WifiProvisioning>(
        &self,
        wifi: &mut P,
        ssid: &str,
        password: String,
    ) -> Result<(), ProvisionError<B::Error>> {
        let _guard = self.writes.lock().await;
        if !wifi.provision(ssid, password).await {
            return Err(ProvisionError::Credentials);
        }
        self.flag.commit(TRUE).await.map_err(ProvisionError::Flag)?;
        Ok(())
    }
    /// Commit false first. On failure, do not clear credentials or request reset.
    /// If clear fails afterwards, false is durable and recovery can restart into
    /// provisioning with residual credentials. Caller reports the partial error.
    pub async fn recover(&self) -> Result<Recovery<B::Error>, SpaceError<B::Error>> {
        let _guard = self.writes.lock().await;
        self.flag.commit(FALSE).await?;
        Ok(match self.backend.clear("wifi").await {
            Ok(_) => Recovery::Cleared,
            Err(error) => Recovery::ResidualCredentials(error),
        })
    }
}
