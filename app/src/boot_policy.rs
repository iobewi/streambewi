//! Product-owned persisted USB boot policy. USB selection is immutable this boot.
//! Credentials and mode are separate commits, deliberately not a transaction.
use alloc::string::String;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex};
use iobewi_config_space::{
    Budget, ClaimError, ConfigBackend, ConfigManager, ConfigSpace, SpaceError,
};
use iobewi_wifi_core::WifiProvisioning;
use iobewi_wifi_manager::CONFIG_BUDGET as WIFI_CONFIG_BUDGET;

pub const SPACE: &str = "usb_boot";
const FALSE: &[u8] = b"USB1\x00";
const TRUE: &[u8] = b"USB1\x01";
pub const CONFIG_BUDGET: Budget = Budget::new(TRUE.len());
/// Every ConfigSpace of the product, as admitted by the store. Adding a space is admission arithmetic
/// only (the store is not reformatted); `the_spaces_fit_the_partition_to_the_page` keeps it within the 128-page
/// `config` partition of the OTA layout (`targets/esp32/partitions.csv`).
pub const SPACES: &[iobewi_config_space::storage::SpaceSpec] = &[
    iobewi_config_space::storage::SpaceSpec::new(
        crate::stations::SPACE,
        crate::stations::CONFIG_BUDGET.max_bytes(),
    ),
    iobewi_config_space::storage::SpaceSpec::new("wifi", WIFI_CONFIG_BUDGET.max_bytes()),
    iobewi_config_space::storage::SpaceSpec::new(SPACE, CONFIG_BUDGET.max_bytes()),
    iobewi_config_space::storage::SpaceSpec::new(
        crate::diag::SPACE,
        crate::diag::CONFIG_BUDGET.max_bytes(),
    ),
    // After these, so that the spaces above keep their place.
    iobewi_config_space::storage::SpaceSpec::new(
        crate::settings::SPACE,
        crate::settings::CONFIG_BUDGET.max_bytes(),
    ),
    // The OTM1 records of the system A OTA (`iobewi-firmware-update`). The budget is the reference
    // composition's; OTM1 itself is at most 512 bytes. With it the manifest admits 60 of the 112
    // reservable pages of the 128-page `config` partition.
    iobewi_config_space::storage::SpaceSpec::new(OTA_SPACE, OTA_CONFIG_BUDGET.max_bytes()),
];
/// The space of the OTA metadata, and its budget (as in `examples/firmware/ota-system`).
pub const OTA_SPACE: &str = "ota";
pub const OTA_CONFIG_BUDGET: Budget = Budget::new(2048);
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
        let wifi = manager.claim("wifi", WIFI_CONFIG_BUDGET)?;
        let flag = manager.claim(SPACE, CONFIG_BUDGET)?;
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
    /// Admit product radio settings through the same claim sequence as Wi-Fi and USB.
    pub fn claim_stations(backend: B) -> Result<ConfigSpace<B>, ClaimError> {
        let mut manager = ConfigManager::new(backend);
        manager.claim("wifi", WIFI_CONFIG_BUDGET)?;
        manager.claim(SPACE, CONFIG_BUDGET)?;
        manager.claim(crate::stations::SPACE, crate::stations::CONFIG_BUDGET)
    }
    /// Admit the log policy after the spaces above, in the same claim order.
    pub fn claim_log(backend: B) -> Result<ConfigSpace<B>, ClaimError> {
        let mut manager = ConfigManager::new(backend);
        manager.claim("wifi", WIFI_CONFIG_BUDGET)?;
        manager.claim(SPACE, CONFIG_BUDGET)?;
        manager.claim(crate::stations::SPACE, crate::stations::CONFIG_BUDGET)?;
        manager.claim(crate::diag::SPACE, crate::diag::CONFIG_BUDGET)
    }
    /// Admit the product settings after the spaces above, in the same claim order.
    pub fn claim_settings(backend: B) -> Result<ConfigSpace<B>, ClaimError> {
        let mut manager = ConfigManager::new(backend);
        manager.claim("wifi", WIFI_CONFIG_BUDGET)?;
        manager.claim(SPACE, CONFIG_BUDGET)?;
        manager.claim(crate::stations::SPACE, crate::stations::CONFIG_BUDGET)?;
        manager.claim(crate::diag::SPACE, crate::diag::CONFIG_BUDGET)?;
        manager.claim(crate::settings::SPACE, crate::settings::CONFIG_BUDGET)
    }
    /// The OTA metadata space, after the spaces above in the same claim order.
    pub fn claim_ota(backend: B) -> Result<ConfigSpace<B>, ClaimError> {
        let mut manager = ConfigManager::new(backend);
        manager.claim("wifi", WIFI_CONFIG_BUDGET)?;
        manager.claim(SPACE, CONFIG_BUDGET)?;
        manager.claim(crate::stations::SPACE, crate::stations::CONFIG_BUDGET)?;
        manager.claim(crate::diag::SPACE, crate::diag::CONFIG_BUDGET)?;
        manager.claim(crate::settings::SPACE, crate::settings::CONFIG_BUDGET)?;
        manager.claim(OTA_SPACE, OTA_CONFIG_BUDGET)
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
