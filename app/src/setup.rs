//! Product provisioning page on the AP network only. Radio and DHCP are provided by the target.
//! HTTP uses picoserve; both UIs submit to the single owner of the Wi-Fi manager.
use crate::stations::Radio;
use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::cell::RefCell;
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use embassy_net::Stack;
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel, signal::Signal,
};
use iobewi_config_space::ConfigBackend;
use iobewi_wifi_core::AccessPointConfig;
use picoserve::{
    extract::Form,
    response::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

pub const AP_SSID: &str = "StreamBeWI-Setup";
// Development default, replace at build time for a device distributed to someone else.
pub const AP_PASSWORD: &str = match option_env!("STREAMBEWI_AP_PASSWORD") {
    Some(password) => password,
    None => "streambewi",
};

pub fn access_point_config() -> AccessPointConfig {
    AccessPointConfig::new(AP_SSID, AP_PASSWORD, 6)
        .expect("invalid STREAMBEWI_AP_PASSWORD (8..63 bytes)")
}

pub(crate) struct Request {
    pub id: u32,
    pub ssid: String,
    pub password: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Saved,
    Connection,
    Storage,
}

pub(crate) static REQUESTS: Channel<CriticalSectionRawMutex, Request, 1> = Channel::new();
pub(crate) static RESULT: Signal<CriticalSectionRawMutex, (u32, Outcome)> = Signal::new();
static NEXT_ID: AtomicU32 = AtomicU32::new(0);
pub(crate) static SCAN_REQUESTS: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();
pub(crate) static RESTART: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static SCANNING: AtomicU8 = AtomicU8::new(0);
static RESTARTING: AtomicU8 = AtomicU8::new(0);
static MODE_SETUP: AtomicU8 = AtomicU8::new(0);
#[derive(Clone, Serialize)]
struct Network {
    ssid: String,
    strength: i8,
    secured: bool,
}
struct WifiView {
    online: bool,
    ssid: String,
    ip: String,
    networks: Vec<Network>,
}
static WIFI: embassy_sync::blocking_mutex::Mutex<CriticalSectionRawMutex, RefCell<WifiView>> =
    embassy_sync::blocking_mutex::Mutex::new(RefCell::new(WifiView {
        online: false,
        ssid: String::new(),
        ip: String::new(),
        networks: Vec::new(),
    }));
pub(crate) fn set_setup(value: bool) {
    MODE_SETUP.store(u8::from(value), Ordering::Relaxed);
}
pub(crate) fn set_ssid(ssid: &str) {
    WIFI.lock(|v| v.borrow_mut().ssid = ssid.into());
}
pub(crate) fn set_network(stack: Stack<'static>) {
    WIFI.lock(|v| {
        let mut v = v.borrow_mut();
        v.online = stack.is_config_up();
        v.ip = stack
            .config_v4()
            .map(|c| c.address.address().to_string())
            .unwrap_or_default();
    });
}
pub(crate) fn set_offline() {
    WIFI.lock(|v| v.borrow_mut().online = false);
}
pub(crate) fn set_scan(networks: Vec<iobewi_wifi_core::Network>) {
    WIFI.lock(|v| {
        v.borrow_mut().networks = networks
            .into_iter()
            .take(12)
            .map(|n| Network {
                ssid: n.ssid,
                strength: n.signal_strength,
                secured: n.secured,
            })
            .collect()
    });
    SCANNING.store(0, Ordering::Relaxed);
}

/// Last Web provisioning state, written by the Wi-Fi owner (not by the HTTP handler, which is dropped
/// when the AP client is cut by the channel change). `/status` serves it once the client is back.
static STATUS: AtomicU8 = AtomicU8::new(0);

pub(crate) fn set_pending() {
    STATUS.store(1, Ordering::Relaxed);
}

pub(crate) fn set_outcome(outcome: Outcome) {
    STATUS.store(
        match outcome {
            Outcome::Saved => 2,
            Outcome::Connection => 3,
            Outcome::Storage => 4,
        },
        Ordering::Relaxed,
    );
}

/// The one user-facing status text, served by `/status` and injected in `/` (works without JavaScript,
/// e.g. in a phone's captive-portal sheet). Empty when no Web request was made.
pub(crate) fn status_message() -> &'static str {
    match STATUS.load(Ordering::Relaxed) {
        1 => {
            "Vérification en cours… Si votre Wi-Fi se coupe, rejoignez de nouveau StreamBeWI-Setup puis rechargez cette page : le résultat s’affichera ici."
        }
        2 => "Wi-Fi enregistré. Débranchez puis rebranchez StreamBeWI pour écouter la radio.",
        3 => {
            "Connexion impossible ou enregistrement Wi-Fi échoué. Vérifiez le nom et le mot de passe, puis réessayez."
        }
        4 => {
            "Wi-Fi enregistré, mais l’activation USB n’a pas été enregistrée. Réessayez avant de redémarrer."
        }
        _ => "",
    }
}

async fn page() -> ([(&'static str, &'static str); 2], String) {
    let (head, tail) = PAGE.split_once("<!--S-->").unwrap_or((PAGE, ""));
    let mut body = String::with_capacity(PAGE.len() + 160);
    body.push_str(head);
    body.push_str(status_message());
    body.push_str(tail);
    (
        [
            ("Content-Type", "text/html; charset=utf-8"),
            ("Cache-Control", "no-store"),
        ],
        body,
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    ssid: String,
    password: String,
}

impl Credentials {
    fn valid(&self) -> bool {
        !self.ssid.is_empty()
            && self.ssid.len() <= 32
            && !self.ssid.as_bytes().contains(&0)
            && (self.password.is_empty()
                || (8..=63).contains(&self.password.len())
                || (self.password.len() == 64
                    && self.password.bytes().all(|b| b.is_ascii_hexdigit())))
            && !self.password.as_bytes().contains(&0)
    }
}

const PAGE: &str = include_str!("setup.html");

async fn provision(
    Form(credentials): Form<Credentials>,
) -> (StatusCode, (&'static str, &'static str), &'static str) {
    if !credentials.valid() {
        return (
            StatusCode::BAD_REQUEST,
            ("Content-Type", "text/html; charset=utf-8"),
            "<html lang=fr><meta charset=utf-8><p>SSID : 1 à 32 octets. Mot de passe : vide pour un réseau ouvert, 8 à 63 octets ou 64 chiffres hexadécimaux.</p><a href=/>Réessayer</a></html>",
        );
    }
    // Publish pending before returning 202, including when Improv currently owns the radio.
    // One Web request in flight prevents a second client from observing an older success.
    let previous = STATUS.load(Ordering::Relaxed);
    if previous == 1
        || STATUS
            .compare_exchange(previous, 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return (
            StatusCode::CONFLICT,
            ("Content-Type", "text/html; charset=utf-8"),
            "Une configuration Wi-Fi est déjà en cours. Patientez puis rechargez la page.",
        );
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    REQUESTS
        .send(Request {
            id,
            ssid: credentials.ssid,
            password: credentials.password,
        })
        .await;
    (
        StatusCode::ACCEPTED,
        ("Content-Type", "text/html; charset=utf-8"),
        "<html lang=fr><meta charset=utf-8><p>Vérification en cours. Si votre connexion change, rejoignez le réseau puis rechargez la page.</p><a href=/>Retour</a></html>",
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StationForm {
    #[serde(default)]
    id: Option<u32>,
    name: String,
    url: String,
    #[serde(default)]
    logo: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdForm {
    id: u32,
}
const JSON_HEADERS: [(&str, &str); 2] = [
    ("Content-Type", "application/json; charset=utf-8"),
    ("Cache-Control", "no-store"),
];
fn mutation(
    result: Result<bool, crate::stations::Error>,
) -> (StatusCode, [(&'static str, &'static str); 2], String) {
    match result {
        Ok(restart) => {
            if restart {
                RESTARTING.store(1, Ordering::Relaxed);
                RESTART.signal(());
            }
            (
                StatusCode::OK,
                JSON_HEADERS,
                serde_json::json!({"ok":true,"restart":restart}).to_string(),
            )
        }
        Err(error) => (
            StatusCode::BAD_REQUEST,
            JSON_HEADERS,
            serde_json::json!({"ok":false,"error":error.message()}).to_string(),
        ),
    }
}
async fn state<B: ConfigBackend>(radio: &Radio<B>) -> ([(&'static str, &'static str); 2], String) {
    let catalog = radio.snapshot().await;
    let (online, ssid, ip, networks) = WIFI.lock(|v| {
        let v = v.borrow();
        (v.online, v.ssid.clone(), v.ip.clone(), v.networks.clone())
    });
    (
        JSON_HEADERS,
        serde_json::json!({
            "catalog":catalog, "online":online, "ssid":ssid, "ip":ip,
            "networks":networks, "scanning":SCANNING.load(Ordering::Relaxed) != 0,
            "setup":MODE_SETUP.load(Ordering::Relaxed) != 0,
            "reading":crate::stream::STREAM.is_reading(), "ready":crate::stream::STREAM.is_ready(),
            "restarting":RESTARTING.load(Ordering::Relaxed) != 0,
            "wifi_message":status_message(), "wifi_status":STATUS.load(Ordering::Relaxed),
        })
        .to_string(),
    )
}

/// No listener is bound to STA; an AP client must open the explicit local address (no captive DNS).
pub async fn serve<B: ConfigBackend>(stack: Stack<'static>, radio: &Radio<B>) -> ! {
    let router =
        picoserve::Router::new()
            .route("/", get(page))
            .route(
                "/status",
                get(|| async {
                    (
                        [
                            ("Content-Type", "text/plain; charset=utf-8"),
                            ("Cache-Control", "no-store"),
                        ],
                        status_message(),
                    )
                }),
            )
            .route("/wifi", post(provision))
            .route(
                "/ui.css",
                get(|| async {
                    (
                        ("Content-Type", "text/css; charset=utf-8"),
                        include_str!("ui.css"),
                    )
                }),
            )
            .route(
                "/ui.js",
                get(|| async {
                    (
                        ("Content-Type", "application/javascript; charset=utf-8"),
                        include_str!("ui.js"),
                    )
                }),
            )
            .route("/api/state", get(move || async move { state(radio).await }))
            .route(
                "/api/scan",
                post(|| async {
                    if SCANNING.swap(1, Ordering::Relaxed) == 0
                        && SCAN_REQUESTS.try_send(()).is_err()
                    {
                        SCANNING.store(0, Ordering::Relaxed);
                    }
                    (StatusCode::ACCEPTED, "Recherche en cours")
                }),
            )
            .route(
                "/api/stations/save",
                post(move |Form(value): Form<StationForm>| async move {
                    mutation(
                        radio
                            .save(value.id, value.name, value.url, value.logo)
                            .await,
                    )
                }),
            )
            .route(
                "/api/stations/select",
                post(move |Form(value): Form<IdForm>| async move {
                    mutation(radio.select(value.id).await)
                }),
            )
            .route(
                "/api/stations/delete",
                post(move |Form(value): Form<IdForm>| async move {
                    mutation(radio.delete(value.id).await)
                }),
            );
    // A connection attempt + DHCP may take time. One bounded HTTP client, no keep-alive.
    let config = picoserve::Config::new(picoserve::Timeouts {
        start_read_request: embassy_time::Duration::from_secs(10),
        read_request: embassy_time::Duration::from_secs(90),
        write: embassy_time::Duration::from_secs(5),
        ..picoserve::Timeouts::const_default()
    });
    let mut http = [0; 2048];
    let mut rx = [0; 1024];
    let mut tx = [0; 1024];
    match picoserve::Server::new(&router, &config, &mut http)
        .listen_and_serve(0, stack, 80, &mut rx, &mut tx)
        .await {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    #[test]
    fn invalid_credentials_do_not_reach_the_wifi_owner() {
        let mut request = std::pin::pin!(provision(Form(credentials("", "password"))));
        match request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready((status, _, _)) => assert_eq!(status, StatusCode::BAD_REQUEST),
            Poll::Pending => panic!("invalid input must not queue a Wi-Fi operation"),
        }
    }
    fn credentials(ssid: &str, password: &str) -> Credentials {
        Credentials {
            ssid: ssid.into(),
            password: password.into(),
        }
    }
    #[test]
    fn credentials_are_checked_in_bytes_without_trimming() {
        assert!(credentials(" réseau ", "password").valid());
        assert!(credentials("open", "").valid());
        assert!(!credentials("", "password").valid());
        assert!(credentials(&"é".repeat(16), "password").valid());
        assert!(!credentials(&"é".repeat(17), "password").valid());
        assert!(!credentials("network", "short").valid());
        assert!(!credentials("network\0", "password").valid());
        assert!(credentials("network", &"a".repeat(64)).valid());
        assert!(!credentials("network", &"z".repeat(64)).valid());
    }
    #[test]
    fn browser_form_decodes_utf8_plus_and_password_symbols() {
        let form: Credentials =
            picoserve::url_encoded::deserialize_form(picoserve::url_encoded::UrlEncodedString(
                "ssid=R%C3%A9seau+salon&password=p%2B%26%3D%25word",
            ))
            .unwrap();
        assert_eq!(form.ssid, "Réseau salon");
        assert_eq!(form.password, "p+&=%word");
        assert!(form.valid());
        for invalid in [
            "ssid=test",
            "ssid=a&ssid=b&password=password",
            "ssid=a&password=password&other=x",
        ] {
            assert!(
                picoserve::url_encoded::deserialize_form::<Credentials>(
                    picoserve::url_encoded::UrlEncodedString(invalid)
                )
                .is_err()
            );
        }
    }
    #[test]
    fn station_forms_decode_optional_ids_and_encoded_stream_queries() {
        use picoserve::url_encoded::{UrlEncodedString, deserialize_form};
        let new: StationForm = deserialize_form(UrlEncodedString(
            "name=Jazz&url=http%3A%2F%2Fexample.org%2Flive%3Fx%3D1%26y%3D2&logo=",
        ))
        .unwrap();
        assert_eq!(new.id, None);
        assert_eq!(new.url, "http://example.org/live?x=1&y=2");
        let edit: StationForm = deserialize_form(UrlEncodedString(
            "id=2&name=Jazz&url=http%3A%2F%2Fexample.org%2Flive",
        ))
        .unwrap();
        assert_eq!(edit.id, Some(2));
        assert_eq!(edit.logo, "");
        assert!(deserialize_form::<IdForm>(UrlEncodedString("id=2&extra=1")).is_err());
    }
    #[test]
    fn ap_configuration_matches_the_page_contract() {
        let ap = access_point_config();
        assert_eq!(ap.ssid(), AP_SSID);
        assert_eq!(ap.channel(), 6);
    }
}
