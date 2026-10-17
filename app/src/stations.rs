//! Product-owned favorites, persisted atomically in one ConfigSpace.
use alloc::{string::String, vec, vec::Vec};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex};
use iobewi_config_space::{Budget, ConfigBackend, ConfigSpace};
use serde::{Deserialize, Serialize};

pub const SPACE: &str = "stations";
pub const CONFIG_BUDGET: Budget = Budget::new(4096);
pub const MAX_STATIONS: usize = 8;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Station {
    pub id: u32,
    pub name: String,
    pub url: String,
    pub logo: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub version: u8,
    pub selected: u32,
    pub stations: Vec<Station>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            version: 1,
            selected: 1,
            stations: vec![Station {
                id: 1,
                name: "Mon petit France Inter".into(),
                url: crate::stream::STREAM_URL.into(),
                logo: String::new(),
            }],
        }
    }
}

/// The producer currently supports direct MP3 streams over HTTP, not TLS or playlists.
pub fn stream_url_valid(url: &str) -> bool {
    valid_url(url, false)
}
fn valid_url(url: &str, https: bool) -> bool {
    if url.len() > 256
        || url
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return false;
    }
    let authority = url.strip_prefix("http://").or_else(|| {
        if https {
            url.strip_prefix("https://")
        } else {
            None
        }
    });
    let Some(rest) = authority else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !host.is_empty() && !host.contains('@') && !rest.contains('#')
}
impl Catalog {
    pub fn valid(&self) -> bool {
        self.version == 1
            && !self.stations.is_empty()
            && self.stations.len() <= MAX_STATIONS
            && self.stations.iter().any(|s| s.id == self.selected)
            && self.stations.iter().enumerate().all(|(i, s)| {
                s.id != 0
                    && !s.name.trim().is_empty()
                    && s.name.len() <= 64
                    && !s.name.chars().any(char::is_control)
                    && stream_url_valid(&s.url)
                    && (s.logo.is_empty() || valid_url(&s.logo, true))
                    && !self.stations[..i].iter().any(|p| p.id == s.id)
            })
    }
    pub fn active(&self) -> &Station {
        // Catalogs enter memory only after validation.
        self.stations
            .iter()
            .find(|s| s.id == self.selected)
            .expect("valid catalog")
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Full,
    Missing,
    LastStation,
    Storage,
}

pub struct Radio<B: ConfigBackend> {
    space: ConfigSpace<B>,
    catalog: Mutex<NoopRawMutex, Catalog>,
}
impl<B: ConfigBackend> Radio<B> {
    pub async fn load(space: ConfigSpace<B>) -> Result<Self, Error> {
        let catalog = match space.load().await.map_err(|_| Error::Storage)? {
            Some(value) => {
                serde_json::from_slice::<Catalog>(&value.data).map_err(|_| Error::Invalid)?
            }
            None => Catalog::default(),
        };
        if !catalog.valid() {
            return Err(Error::Invalid);
        }
        Ok(Self {
            space,
            catalog: Mutex::new(catalog),
        })
    }
    pub async fn snapshot(&self) -> Catalog {
        self.catalog.lock().await.clone()
    }
    pub async fn active_url(&self) -> String {
        self.catalog.lock().await.active().url.clone()
    }
    async fn commit(&self, next: &Catalog) -> Result<(), Error> {
        if !next.valid() {
            return Err(Error::Invalid);
        }
        let bytes = serde_json::to_vec(next).map_err(|_| Error::Invalid)?;
        if bytes.len() > CONFIG_BUDGET.max_bytes() {
            return Err(Error::Full);
        }
        self.space
            .commit(&bytes)
            .await
            .map_err(|_| Error::Storage)?;
        Ok(())
    }
    /// Return whether the active audio source changed (apply at the next product restart).
    pub async fn save(
        &self,
        id: Option<u32>,
        name: String,
        url: String,
        logo: String,
    ) -> Result<bool, Error> {
        let mut current = self.catalog.lock().await;
        let mut next = current.clone();
        let active_before = current.active().url.clone();
        let id = match id {
            Some(id) => {
                if !next.stations.iter().any(|s| s.id == id) {
                    return Err(Error::Missing);
                }
                id
            }
            None => {
                if next.stations.len() >= MAX_STATIONS {
                    return Err(Error::Full);
                }
                next.stations
                    .iter()
                    .map(|s| s.id)
                    .max()
                    .unwrap_or(0)
                    .checked_add(1)
                    .ok_or(Error::Full)?
            }
        };
        let value = Station {
            id,
            name,
            url,
            logo,
        };
        if let Some(old) = next.stations.iter_mut().find(|s| s.id == id) {
            *old = value;
        } else {
            next.stations.push(value);
        }
        self.commit(&next).await?;
        let changed = next.active().url != active_before;
        *current = next;
        Ok(changed)
    }
    pub async fn select(&self, id: u32) -> Result<bool, Error> {
        let mut current = self.catalog.lock().await;
        if current.selected == id {
            return Ok(false);
        }
        let mut next = current.clone();
        if !next.stations.iter().any(|s| s.id == id) {
            return Err(Error::Missing);
        }
        next.selected = id;
        self.commit(&next).await?;
        *current = next;
        Ok(true)
    }
    pub async fn delete(&self, id: u32) -> Result<bool, Error> {
        let mut current = self.catalog.lock().await;
        if !current.stations.iter().any(|s| s.id == id) {
            return Err(Error::Missing);
        }
        if current.stations.len() == 1 {
            return Err(Error::LastStation);
        }
        let mut next = current.clone();
        next.stations.retain(|s| s.id != id);
        let changed = next.selected == id;
        if changed {
            next.selected = next.stations[0].id;
        }
        self.commit(&next).await?;
        *current = next;
        Ok(changed)
    }
}

impl Error {
    pub fn message(self) -> &'static str {
        match self {
            Self::Invalid => {
                "Vérifiez le nom et l’adresse du flux MP3 direct HTTP (256 caractères maximum)."
            }
            Self::Full => "La liste est pleine ou trop volumineuse (8 stations maximum).",
            Self::Missing => "Cette station n’existe plus. Rechargez la page.",
            Self::LastStation => "Gardez au moins une station.",
            Self::Storage => "Enregistrement impossible. Réessayez.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::{Cell, RefCell};
    use iobewi_config_space::{ConfigManager, Snapshot};
    use std::rc::Rc;
    #[derive(Clone, Default)]
    struct Backend {
        bytes: Rc<RefCell<Option<Vec<u8>>>>,
        fail: Rc<Cell<bool>>,
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
            Ok(self.bytes.borrow().as_ref().map(|data| Snapshot {
                generation: 1,
                data: data.clone(),
            }))
        }
        async fn commit(&self, _: &str, data: &[u8]) -> Result<u64, ()> {
            if self.fail.get() {
                return Err(());
            }
            *self.bytes.borrow_mut() = Some(data.to_vec());
            Ok(1)
        }
        async fn clear(&self, _: &str) -> Result<u64, ()> {
            unreachable!()
        }
    }
    async fn load(backend: Backend) -> Result<Radio<Backend>, Error> {
        Radio::load(
            ConfigManager::new(backend)
                .claim(SPACE, CONFIG_BUDGET)
                .unwrap(),
        )
        .await
    }
    #[test]
    fn favorites_selection_and_edits_survive_reload() {
        embassy_futures::block_on(async {
            let backend = Backend::default();
            let radio = load(backend.clone()).await.unwrap();
            assert_eq!(radio.active_url().await, crate::stream::STREAM_URL);
            assert!(
                !radio
                    .save(
                        None,
                        "FIP".into(),
                        "http://example.org/fip.mp3".into(),
                        "https://example.org/logo.png".into()
                    )
                    .await
                    .unwrap()
            );
            assert!(radio.select(2).await.unwrap());
            assert!(!radio.select(2).await.unwrap());
            let reloaded = load(backend.clone()).await.unwrap();
            assert_eq!(reloaded.snapshot().await.selected, 2);
            assert_eq!(reloaded.active_url().await, "http://example.org/fip.mp3");
            assert!(
                !reloaded
                    .save(
                        Some(2),
                        "FIP Jazz".into(),
                        "http://example.org/fip.mp3".into(),
                        "".into()
                    )
                    .await
                    .unwrap()
            );
            assert!(
                reloaded
                    .save(
                        Some(2),
                        "FIP Jazz".into(),
                        "http://example.org/jazz.mp3".into(),
                        "".into()
                    )
                    .await
                    .unwrap()
            );
        });
    }
    #[test]
    fn failed_flash_write_preserves_running_and_durable_selection() {
        embassy_futures::block_on(async {
            let backend = Backend::default();
            let radio = load(backend.clone()).await.unwrap();
            radio
                .save(
                    None,
                    "FIP".into(),
                    "http://example.org/fip".into(),
                    "".into(),
                )
                .await
                .unwrap();
            let old = radio.snapshot().await;
            backend.fail.set(true);
            assert_eq!(radio.select(2).await, Err(Error::Storage));
            assert_eq!(radio.snapshot().await, old);
            assert_eq!(load(backend).await.unwrap().snapshot().await, old);
        });
    }
    #[test]
    fn deletion_keeps_one_station_and_selects_a_valid_fallback() {
        embassy_futures::block_on(async {
            let radio = load(Backend::default()).await.unwrap();
            assert_eq!(radio.delete(1).await, Err(Error::LastStation));
            assert_eq!(radio.select(99).await, Err(Error::Missing));
            radio
                .save(
                    None,
                    "FIP".into(),
                    "http://example.org/fip".into(),
                    "".into(),
                )
                .await
                .unwrap();
            assert!(radio.delete(1).await.unwrap());
            assert_eq!(radio.snapshot().await.selected, 2);
        });
    }
    #[test]
    fn invalid_and_oversized_catalogs_are_never_published() {
        embassy_futures::block_on(async {
            let backend = Backend::default();
            let radio = load(backend.clone()).await.unwrap();
            for url in [
                "https://example.org/live",
                "javascript:alert(1)",
                "http://",
                "http://a/\r\nx",
                "http://user:pass@host/a",
            ] {
                assert_eq!(
                    radio
                        .save(None, "Radio".into(), url.into(), "".into())
                        .await,
                    Err(Error::Invalid)
                );
            }
            assert_eq!(radio.snapshot().await, Catalog::default());
            assert!(backend.bytes.borrow().is_none());
            *backend.bytes.borrow_mut() = Some(b"{\"version\":99}".to_vec());
            assert!(matches!(load(backend.clone()).await, Err(Error::Invalid)));
            assert_eq!(
                backend.bytes.borrow().as_deref(),
                Some(b"{\"version\":99}".as_slice())
            );
        });
    }
    #[test]
    fn station_limit_is_enforced_without_losing_existing_favorites() {
        embassy_futures::block_on(async {
            let radio = load(Backend::default()).await.unwrap();
            for _ in 1..MAX_STATIONS {
                radio
                    .save(
                        None,
                        "Radio".into(),
                        "http://example.org/live".into(),
                        "".into(),
                    )
                    .await
                    .unwrap();
            }
            assert_eq!(
                radio
                    .save(
                        None,
                        "Extra".into(),
                        "http://example.org/live".into(),
                        "".into()
                    )
                    .await,
                Err(Error::Full)
            );
            assert_eq!(radio.snapshot().await.stations.len(), MAX_STATIONS);
        });
    }
}
