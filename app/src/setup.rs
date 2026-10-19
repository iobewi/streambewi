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
    extract::{Form, Query},
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
/// A station change applied without a restart (MSC boot only): handled by the producer.
pub(crate) static SWITCH: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static SCANNING: AtomicU8 = AtomicU8::new(0);
pub(crate) static RESTARTING: AtomicU8 = AtomicU8::new(0);
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
pub(crate) fn in_setup() -> bool {
    MODE_SETUP.load(Ordering::Relaxed) != 0
}
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

type HtmlReply = (StatusCode, (&'static str, &'static str), &'static str);

/// Credentials are only accepted in the setup (AP) boot, where the client had to join the WPA2 access
/// point. In normal operation the server is on the LAN and has no authentication.
fn outside_setup(setup: bool) -> Option<HtmlReply> {
    (!setup).then_some((
        StatusCode::FORBIDDEN,
        ("Content-Type", "text/html; charset=utf-8"),
        "<html lang=fr><meta charset=utf-8><p>Le Wi-Fi se configure depuis le réseau StreamBeWI-Setup (bouton BOOT) ou par USB avec Improv.</p><a href=/>Retour</a></html>",
    ))
}

async fn provision(Form(credentials): Form<Credentials>) -> HtmlReply {
    match outside_setup(in_setup()) {
        Some(refusal) => refusal,
        None => accept(credentials).await,
    }
}

async fn accept(credentials: Credentials) -> HtmlReply {
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
                if MODE_SETUP.load(Ordering::Relaxed) != 0 {
                    RESTART.signal(());
                } else {
                    SWITCH.signal(());
                }
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
            "restarting":RESTARTING.load(Ordering::Relaxed) != 0, "persisted":!radio.is_unsaved(),
            "wifi_message":status_message(), "wifi_status":STATUS.load(Ordering::Relaxed),
        })
        .to_string(),
    )
}

/// The body is read into a fixed buffer: a longer request is rejected before anything is allocated.
type LogPolicyBody = heapless::String<{ iobewi_log_config::POLICY_BYTES_MAX }>;
#[derive(Deserialize)]
struct LogsQuery {
    #[serde(default)]
    n: Option<usize>,
}
/// Recent sanitized log lines, oldest first. Bounded by `HISTORY_LINES` and by `diag::RESPONSE_MAX`; nothing
/// is consumed. The answer is written once, into one exact allocation, while the history is borrowed
/// (no await); it is sent after the borrow ended. If that allocation fails the answer is a 503, not a halt.
fn logs<B: ConfigBackend>(
    diag: &crate::diag::Diag<B>,
    n: Option<usize>,
) -> (StatusCode, [(&'static str, &'static str); 2], String) {
    use crate::diag::{DEFAULT_LINES, HISTORY_LINES};
    let n = n.unwrap_or(DEFAULT_LINES).clamp(1, HISTORY_LINES);
    let mut body = String::new();
    match diag.write_recent(n, &mut body) {
        Ok(()) => (StatusCode::OK, JSON_HEADERS, body),
        // An empty body: even a short message could fail to allocate when the heap is this short.
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, JSON_HEADERS, String::new()),
    }
}
/// The active policy as the restricted YAML of `iobewi-log-config`, and whether it may be changed here.
fn log_config<B: ConfigBackend>(
    diag: &crate::diag::Diag<B>,
) -> ([(&'static str, &'static str); 2], String) {
    use crate::diag::Source;
    let (yaml, source, load_error) = diag.policy_yaml();
    (
        JSON_HEADERS,
        serde_json::json!({
            "yaml": yaml, "source": if source == Source::Persisted { "persisted" } else { "default" },
            "load_error": load_error, "writable": in_setup(),
            "max_bytes": iobewi_log_config::POLICY_BYTES_MAX,
        })
        .to_string(),
    )
}
/// Replaces the policy. Setup (AP) boot only: the LAN can read the diagnostic but never change it.
async fn put_log_config<B: ConfigBackend>(
    diag: &crate::diag::Diag<B>,
    body: &str,
) -> (StatusCode, [(&'static str, &'static str); 2], String) {
    use crate::diag::PutError;
    // Boxed: the persistence future (policy copies, YAML buffer) lives on the heap for the one AP request
    // instead of inflating the task future of the whole web server.
    match alloc::boxed::Box::pin(diag.replace_yaml(body, in_setup())).await {
        Ok(generation) => (
            StatusCode::OK,
            JSON_HEADERS,
            serde_json::json!({"ok": true, "generation": generation}).to_string(),
        ),
        Err(error) => {
            let (status, message) = match error {
                PutError::NotAllowed => (
                    StatusCode::FORBIDDEN,
                    "La politique ne se modifie que depuis le réseau de configuration StreamBeWI-Setup.",
                ),
                PutError::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "Politique trop longue."),
                PutError::Invalid => (
                    StatusCode::BAD_REQUEST,
                    "Politique refusée : la politique active est conservée.",
                ),
                PutError::Storage => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Écriture impossible : la politique active est conservée.",
                ),
            };
            (
                status,
                JSON_HEADERS,
                serde_json::json!({"ok": false, "error": message}).to_string(),
            )
        }
    }
}

/// No listener is bound to STA; an AP client must open the explicit local address (no captive DNS).
/// The HTTP routes. Route order is nesting depth in picoserve (each route wraps the ones declared before
/// it, and the poll frame of a nested group is several KiB), so the station mutations, which end in a
/// flash commit, live in their own nested router: the nested service is called once the prefix matched,
/// without the frames of the other routes. Keep the diagnostic routes flat, and check the stack depth of
/// every route that reaches the flash after any change here (docs/diagnostic-logs.md).
fn app<'a, B: ConfigBackend>(
    radio: &'a Radio<B>,
    diag: &'a crate::diag::Diag<B>,
) -> picoserve::Router<impl picoserve::routing::PathRouter + 'a> {
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
                if SCANNING.swap(1, Ordering::Relaxed) == 0 && SCAN_REQUESTS.try_send(()).is_err() {
                    SCANNING.store(0, Ordering::Relaxed);
                }
                (StatusCode::ACCEPTED, "Recherche en cours")
            }),
        )
        .route(
            "/api/logs",
            get(move |Query(query): Query<LogsQuery>| async move { logs(diag, query.n) }),
        )
        .route(
            "/api/logs/config",
            get(move || async move { log_config(diag) })
                .put(move |body: LogPolicyBody| async move { put_log_config(diag, &body).await }),
        )
        .nest(
            "/api/stations",
            picoserve::Router::new()
                .route(
                    "/save",
                    post(move |Form(value): Form<StationForm>| async move {
                        mutation(
                            radio
                                .save(value.id, value.name, value.url, value.logo)
                                .await,
                        )
                    }),
                )
                .route(
                    "/select",
                    post(move |Form(value): Form<IdForm>| async move {
                        mutation(radio.select(value.id).await)
                    }),
                )
                .route(
                    "/delete",
                    post(move |Form(value): Form<IdForm>| async move {
                        mutation(radio.delete(value.id).await)
                    }),
                ),
        )
}

pub async fn serve<B: ConfigBackend>(
    stack: Stack<'static>,
    radio: &Radio<B>,
    diag: &crate::diag::Diag<B>,
) -> ! {
    let router = app(radio, diag);
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
        let mut request = std::pin::pin!(accept(credentials("", "password")));
        match request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready((status, _, _)) => assert_eq!(status, StatusCode::BAD_REQUEST),
            Poll::Pending => panic!("invalid input must not queue a Wi-Fi operation"),
        }
    }
    #[test]
    fn logs_query_accepts_a_line_count_and_rejects_the_rest_without_panicking() {
        let parse = |q: &str| -> Result<Option<usize>, ()> {
            picoserve::url_encoded::deserialize_form::<LogsQuery>(
                picoserve::url_encoded::UrlEncodedString(q),
            )
            .map(|v| v.n)
            .map_err(|_| ())
        };
        assert_eq!(parse("n=32"), Ok(Some(32)));
        assert_eq!(parse(""), Ok(None));
        for bad in ["n=", "n=abc", "n=-1", "n=99999999999999999999"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn wifi_credentials_are_refused_outside_the_setup_boot() {
        let refusal = outside_setup(false).expect("normal boot must refuse");
        assert_eq!(refusal.0, StatusCode::FORBIDDEN);
        assert!(outside_setup(true).is_none());
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

/// The real picoserve server over an in-memory socket, with the production routes (`app`) and the previous
/// flat route list as a reference: the nesting of the station routes must not change a single answer.
#[cfg(test)]
mod routing_tests {
    use super::*;
    use crate::{diag::Diag, stations};
    use alloc::{collections::BTreeMap, rc::Rc};
    use core::{convert::Infallible, future::Future};
    use iobewi_config_space::{Budget, ConfigManager, Snapshot};
    use picoserve::{
        io::{BaseWrite, ErrorType, Read, Socket, Write},
        mem::{BorrowedBuffer, BorrowedCursor},
        time::{Duration, Timer},
    };

    #[derive(Clone, Default)]
    struct Backend(Rc<RefCell<BTreeMap<String, Vec<u8>>>>);
    impl ConfigBackend for Backend {
        type Error = ();
        fn capacity_units(&self) -> usize {
            16384
        }
        fn reservation_units(&self, _: &str, budget: Budget) -> Option<usize> {
            Some(budget.max_bytes())
        }
        async fn load(&self, name: &str) -> Result<Option<Snapshot>, ()> {
            Ok(self.0.borrow().get(name).map(|data| Snapshot {
                generation: 1,
                data: data.clone(),
            }))
        }
        async fn commit(&self, name: &str, data: &[u8]) -> Result<u64, ()> {
            self.0.borrow_mut().insert(name.into(), data.to_vec());
            Ok(1)
        }
        async fn clear(&self, _: &str) -> Result<u64, ()> {
            unreachable!()
        }
    }

    struct TestRuntime;
    struct NoTimer;
    impl Timer<TestRuntime> for NoTimer {
        fn delay(&self, _: Duration) -> impl Future<Output = ()> {
            core::future::pending()
        }
    }

    struct Pipe {
        request: Vec<u8>,
        position: usize,
        response: Rc<RefCell<Vec<u8>>>,
    }
    struct PipeRead<'a> {
        data: &'a [u8],
        position: &'a mut usize,
    }
    struct PipeWrite<'a>(&'a Rc<RefCell<Vec<u8>>>);
    impl ErrorType for PipeRead<'_> {
        type Error = Infallible;
    }
    impl ErrorType for PipeWrite<'_> {
        type Error = Infallible;
    }
    impl Read for PipeRead<'_> {
        async fn read(&mut self, buffer: &mut [u8]) -> Result<usize, Infallible> {
            let n = (self.data.len() - *self.position).min(buffer.len());
            buffer[..n].copy_from_slice(&self.data[*self.position..*self.position + n]);
            *self.position += n;
            Ok(n)
        }
    }
    impl BaseWrite for PipeWrite<'_> {
        async fn write(&mut self, data: &[u8]) -> Result<usize, Infallible> {
            self.0.borrow_mut().extend_from_slice(data);
            Ok(data.len())
        }
        async fn flush(&mut self) -> Result<(), Infallible> {
            Ok(())
        }
    }
    impl Write for PipeWrite<'_> {
        async fn write_with<F: FnOnce(BorrowedCursor<'_>) -> R, R>(
            &mut self,
            f: F,
        ) -> Result<R, Infallible> {
            let mut storage = [0; 1024];
            let mut buffer = BorrowedBuffer::new(&mut storage);
            let output = f(buffer.unfilled());
            self.0.borrow_mut().extend_from_slice(buffer.filled());
            Ok(output)
        }
    }
    impl<Runtime> Socket<Runtime> for Pipe {
        type Error = Infallible;
        type ReadHalf<'a> = PipeRead<'a>;
        type WriteHalf<'a> = PipeWrite<'a>;
        fn split(&mut self) -> (PipeRead<'_>, PipeWrite<'_>) {
            (
                PipeRead {
                    data: &self.request,
                    position: &mut self.position,
                },
                PipeWrite(&self.response),
            )
        }
        async fn abort<T: Timer<Runtime>>(
            self,
            _: &picoserve::Timeouts,
            _: &T,
        ) -> Result<(), picoserve::Error<Infallible>> {
            Ok(())
        }
        async fn shutdown<T: Timer<Runtime>>(
            self,
            _: &picoserve::Timeouts,
            _: &T,
        ) -> Result<(), picoserve::Error<Infallible>> {
            Ok(())
        }
    }

    fn call<P: picoserve::routing::PathRouter>(
        router: &picoserve::Router<P>,
        method: &str,
        path: &str,
        body: &str,
    ) -> (u16, String) {
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let response = Rc::new(RefCell::new(Vec::new()));
        let pipe = Pipe {
            request: request.into_bytes(),
            position: 0,
            response: response.clone(),
        };
        let timeout = Duration::from_secs(10);
        let config = picoserve::Config::new(picoserve::Timeouts {
            start_read_request: timeout,
            persistent_start_read_request: timeout,
            read_request: timeout,
            write: timeout,
        });
        let mut http = [0; 2048];
        let served = embassy_futures::block_on(
            picoserve::Server::<TestRuntime, _, _, _>::custom(router, NoTimer, &config, &mut http)
                .serve(pipe),
        );
        assert!(served.is_ok(), "{method} {path}: server error");
        let raw = String::from_utf8(response.borrow().clone()).unwrap();
        let status = raw
            .strip_prefix("HTTP/1.1 ")
            .and_then(|rest| rest.get(..3))
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("{method} {path}: no status line in {raw:?}"));
        (status, raw)
    }

    /// The route list as it was before the station routes were nested (declaration order included).
    fn flat_reference<'a, B: ConfigBackend>(
        radio: &'a Radio<B>,
        diag: &'a Diag<B>,
    ) -> picoserve::Router<impl picoserve::routing::PathRouter + 'a> {
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
            .route("/api/state", get(move || async move { state(radio).await }))
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
            )
            .route(
                "/api/logs",
                get(move |Query(query): Query<LogsQuery>| async move { logs(diag, query.n) }),
            )
            .route(
                "/api/logs/config",
                get(move || async move { log_config(diag) }).put(
                    move |body: LogPolicyBody| async move { put_log_config(diag, &body).await },
                ),
            )
    }

    async fn fixtures() -> (Radio<Backend>, Diag<Backend>) {
        let backend = Backend::default();
        let mut manager = ConfigManager::new(backend);
        let radio = Radio::load(
            manager
                .claim(stations::SPACE, stations::CONFIG_BUDGET)
                .unwrap(),
        )
        .await
        .unwrap();
        let diag = Diag::load(
            manager
                .claim(crate::diag::SPACE, crate::diag::CONFIG_BUDGET)
                .unwrap(),
            "streambewi",
        )
        .await;
        (radio, diag)
    }

    const FORM: &str = "name=Test&url=http%3A%2F%2Fexample.org%2Fa.mp3&logo=";

    /// Same requests, same order, against the nested production router and the flat reference: the
    /// status line, headers and body must be identical, including the refusals.
    #[test]
    fn nested_station_routes_answer_exactly_like_the_flat_ones() {
        let requests: &[(&str, &str, &str)] = &[
            ("POST", "/api/stations/save", FORM),
            (
                "POST",
                "/api/stations/save",
                "name=B&url=http%3A%2F%2Fexample.org%2Fb.mp3",
            ),
            ("POST", "/api/stations/save", "name=&url="),
            ("POST", "/api/stations/save", "bogus=1"),
            ("POST", "/api/stations/select", "id=2"),
            ("POST", "/api/stations/select", "id=99"),
            ("POST", "/api/stations/select", "id=abc"),
            ("POST", "/api/stations/delete", "id=3"),
            ("POST", "/api/stations/delete", "id=99"),
            ("GET", "/api/stations/save", ""),
            ("GET", "/api/stations/select", ""),
            ("PUT", "/api/stations/delete", ""),
            ("DELETE", "/api/stations/save", ""),
            ("POST", "/api/stations/foo", ""),
            ("GET", "/api/stations/foo", ""),
            ("POST", "/api/stations", ""),
            ("GET", "/api/stations", ""),
            ("GET", "/api/stations/", ""),
            ("POST", "/api/stations/save/extra", FORM),
            ("POST", "/api/stationsXYZ", ""),
            ("POST", "/api/stationsXYZ/save", FORM),
            ("GET", "/api/logs", ""),
            ("GET", "/api/logs?n=2", ""),
            ("GET", "/api/logs?n=abc", ""),
            ("POST", "/api/logs", ""),
            ("GET", "/api/logs/config", ""),
            ("PUT", "/api/logs/config", "log:\n  default_level: warn\n"),
            ("DELETE", "/api/logs/config", ""),
            ("POST", "/wifi", "ssid=home&password=correct-horse"),
            ("GET", "/wifi", ""),
            ("GET", "/", ""),
            ("GET", "/status", ""),
            ("GET", "/nothing", ""),
        ];
        embassy_futures::block_on(async {
            let (radio, diag) = fixtures().await;
            let (reference_radio, reference_diag) = fixtures().await;
            let nested = app(&radio, &diag);
            let flat = flat_reference(&reference_radio, &reference_diag);
            for (method, path, body) in requests {
                let (status, raw) = (
                    call(&nested, method, path, body),
                    call(&flat, method, path, body),
                );
                assert_eq!(status.0, raw.0, "{method} {path}: status");
                assert_eq!(status.1, raw.1, "{method} {path}: answer");
            }
            assert_eq!(radio.snapshot().await, reference_radio.snapshot().await);
        });
    }

    /// The expected contract, stated on its own so a regression of both routers would still be caught.
    #[test]
    fn station_and_diagnostic_routes_keep_their_statuses() {
        embassy_futures::block_on(async {
            let (radio, diag) = fixtures().await;
            let router = app(&radio, &diag);
            let status = |method, path, body| call(&router, method, path, body).0;
            assert_eq!(status("POST", "/api/stations/save", FORM), 200);
            assert_eq!(radio.snapshot().await.stations.len(), 2);
            assert_eq!(status("POST", "/api/stations/select", "id=2"), 200);
            assert_eq!(radio.snapshot().await.selected, 2);
            assert_eq!(status("POST", "/api/stations/save", FORM), 200);
            assert_eq!(status("POST", "/api/stations/delete", "id=3"), 200);
            assert_eq!(radio.snapshot().await.stations.len(), 2);
            for path in [
                "/api/stations/foo",
                "/api/stations",
                "/api/stations/",
                "/api/stationsXYZ",
                "/api/stationsXYZ/save",
                "/api/stations/save/extra",
            ] {
                assert_eq!(status("POST", path, FORM), 404, "POST {path}");
                assert_eq!(status("GET", path, ""), 404, "GET {path}");
            }
            for (method, path) in [
                ("GET", "/api/stations/save"),
                ("GET", "/api/stations/select"),
                ("PUT", "/api/stations/delete"),
                ("DELETE", "/api/stations/save"),
                ("POST", "/api/logs"),
                ("DELETE", "/api/logs/config"),
                ("GET", "/wifi"),
            ] {
                assert_eq!(status(method, path, ""), 405, "{method} {path}");
            }
            assert_eq!(status("GET", "/api/logs", ""), 200);
            assert_eq!(status("GET", "/api/logs?n=32", ""), 200);
            assert_eq!(status("GET", "/api/logs/config", ""), 200);
            assert_eq!(status("GET", "/api/state", ""), 200);
            // Outside the setup boot the LAN may read the policy but neither write it nor send credentials.
            assert_eq!(
                status("PUT", "/api/logs/config", "log:\n  default_level: warn\n"),
                403
            );
            assert_eq!(
                status("POST", "/wifi", "ssid=home&password=correct-horse"),
                403
            );
        });
    }
}
