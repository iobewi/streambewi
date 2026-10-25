//! Product provisioning page on the AP network only. Radio and DHCP are provided by the target.
//! HTTP uses picoserve; both UIs submit to the single owner of the Wi-Fi manager.
use crate::brightness::{self, BRIGHTNESS, Brightness};
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
use embassy_time::Instant;
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

async fn page() -> ([(&'static str, &'static str); 1], Typed<Page>) {
    (NO_STORE, Typed(HTML, Page(status_message())))
}

/// The page is one answer: the stylesheet and the two scripts are part of it. A browser that opened its
/// connections in a burst used to lose one of them to a worker that was still busy (a refused connection, no
/// listener left), and a page without `ui.js` stays on its first text. The sources stay separate files
/// (`ui.css`, `catalog.js`, `ui.js`, tested on their own); they are spliced in where `setup.html` marks them,
/// and written piece by piece from flash, so a request allocates nothing for the page.
struct Page(&'static str);

impl Page {
    /// `setup.html` cut at its markers, with what goes between the pieces. The scripts sit at the end of the
    /// body: an inline script is not deferred, and `ui.js` needs the whole document, then `StreamCatalog`.
    fn parts(&self) -> [&'static str; 9] {
        let (head, rest) = PAGE.split_once("<!--CSS-->").unwrap_or((PAGE, ""));
        let (body, rest) = rest.split_once("<!--S-->").unwrap_or((rest, ""));
        let (middle, rest) = rest.split_once("<!--CATALOG-->").unwrap_or((rest, ""));
        let (between, tail) = rest.split_once("<!--UI-->").unwrap_or((rest, ""));
        [
            head,
            include_str!("ui.css"),
            body,
            self.0,
            middle,
            include_str!("catalog.js"),
            between,
            include_str!("ui.js"),
            tail,
        ]
    }
}

impl picoserve::response::Content for Page {
    fn content_type(&self) -> &'static str {
        HTML
    }
    fn content_length(&self) -> usize {
        self.parts().iter().map(|part| part.len()).sum()
    }
    async fn write_content<W: picoserve::io::Write>(self, mut writer: W) -> Result<(), W::Error> {
        for part in self.parts() {
            writer.write_all(part.as_bytes()).await?;
        }
        Ok(())
    }
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

/// A body with its own `Content-Type`. picoserve adds `text/plain` for a `&str` or a `String`, so a
/// `("Content-Type", …)` header next to such a body made two contradictory headers on the wire, and a browser
/// may pick the first. The type travels with the body instead, and the headers tuple never names it.
struct Typed<C>(&'static str, C);
impl<C: picoserve::response::Content> picoserve::response::Content for Typed<C> {
    fn content_type(&self) -> &'static str {
        self.0
    }
    fn content_length(&self) -> usize {
        self.1.content_length()
    }
    async fn write_content<W: picoserve::io::Write>(self, writer: W) -> Result<(), W::Error> {
        self.1.write_content(writer).await
    }
}
const HTML: &str = "text/html; charset=utf-8";
const NO_STORE: [(&str, &str); 1] = [("Cache-Control", "no-store")];
fn json(body: String) -> Typed<String> {
    Typed("application/json; charset=utf-8", body)
}

type HtmlReply = (StatusCode, Typed<&'static str>);

/// Credentials are only accepted in the setup (AP) boot, where the client had to join the WPA2 access
/// point. In normal operation the server is on the LAN and has no authentication.
fn outside_setup(setup: bool) -> Option<HtmlReply> {
    (!setup).then_some((StatusCode::FORBIDDEN, Typed(HTML, "<html lang=fr><meta charset=utf-8><p>Le Wi-Fi se configure depuis le réseau StreamBeWI-Setup (bouton BOOT) ou par USB avec Improv.</p><a href=/>Retour</a></html>")))
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
            Typed(
                HTML,
                "<html lang=fr><meta charset=utf-8><p>SSID : 1 à 32 octets. Mot de passe : vide pour un réseau ouvert, 8 à 63 octets ou 64 chiffres hexadécimaux.</p><a href=/>Réessayer</a></html>",
            ),
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
            Typed(
                HTML,
                "Une configuration Wi-Fi est déjà en cours. Patientez puis rechargez la page.",
            ),
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
        Typed(
            HTML,
            "<html lang=fr><meta charset=utf-8><p>Vérification en cours. Si votre connexion change, rejoignez le réseau puis rechargez la page.</p><a href=/>Retour</a></html>",
        ),
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
fn mutation(
    result: Result<bool, crate::stations::Error>,
) -> (StatusCode, [(&'static str, &'static str); 1], Typed<String>) {
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
                NO_STORE,
                json(serde_json::json!({"ok":true,"restart":restart}).to_string()),
            )
        }
        Err(error) => (
            StatusCode::BAD_REQUEST,
            NO_STORE,
            json(serde_json::json!({"ok":false,"error":error.message()}).to_string()),
        ),
    }
}
/// The LED brightness setting as the page reads it: `saved` is what the configuration holds, `preview` what
/// is being tried (never saved), `applied` what the LED shows; `busy` while a save is asked for or being
/// written, `error` after a failed save. Hidden by the page when `available` is false.
fn led_json(status: &brightness::Status) -> serde_json::Value {
    serde_json::json!({
        "available": status.available,
        "saved": status.saved,
        "preview": status.preview,
        "applied": status.applied,
        "busy": status.busy,
        "error": status.error.map(brightness::SaveError::message),
    })
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PercentForm {
    percent: u8,
}
type JsonReply = (StatusCode, [(&'static str, &'static str); 1], Typed<String>);
fn led_answer(status: StatusCode, body: serde_json::Value) -> JsonReply {
    (status, NO_STORE, json(body.to_string()))
}
/// What a LED request does without a LED: nothing, and says so.
fn led_unavailable(brightness: &Brightness) -> Option<JsonReply> {
    (!brightness.status(Instant::now()).available).then(|| {
        led_answer(
            StatusCode::CONFLICT,
            serde_json::json!({"ok":false,"error":"Cet appareil n’a pas de LED de statut."}),
        )
    })
}
fn led_invalid() -> JsonReply {
    led_answer(
        StatusCode::BAD_REQUEST,
        serde_json::json!({"ok":false,"error":"La luminosité va de 0 à 100 %."}),
    )
}
/// Tries a brightness on the LED for a while. Nothing is written.
fn led_preview(brightness: &Brightness, percent: u8) -> JsonReply {
    if let Some(answer) = led_unavailable(brightness) {
        return answer;
    }
    match brightness.preview(percent, Instant::now()) {
        Ok(()) => led_answer(StatusCode::OK, serde_json::json!({"ok":true})),
        Err(brightness::Invalid) => led_invalid(),
    }
}
/// Back to the saved brightness.
fn led_cancel(brightness: &Brightness) -> JsonReply {
    if let Some(answer) = led_unavailable(brightness) {
        return answer;
    }
    brightness.cancel();
    led_answer(StatusCode::OK, serde_json::json!({"ok":true}))
}
/// Asks to keep a brightness. The answer comes at once, the write comes later (the disk may have to leave
/// the bus first): the page reads `led.busy`, `led.saved` and `led.error` in `/api/state`.
fn led_save(brightness: &Brightness, percent: u8) -> JsonReply {
    if let Some(answer) = led_unavailable(brightness) {
        return answer;
    }
    match brightness.request_save(percent) {
        Ok(()) => led_answer(
            StatusCode::ACCEPTED,
            serde_json::json!({"ok":true,"pending":true}),
        ),
        Err(brightness::Invalid) => led_invalid(),
    }
}
async fn state<B: ConfigBackend>(
    radio: &Radio<B>,
) -> ([(&'static str, &'static str); 1], Typed<String>) {
    let catalog = radio.snapshot().await;
    let (online, ssid, ip, networks) = WIFI.lock(|v| {
        let v = v.borrow();
        (v.online, v.ssid.clone(), v.ip.clone(), v.networks.clone())
    });
    (
        NO_STORE,
        json(serde_json::json!({
            "catalog":catalog, "online":online, "ssid":ssid, "ip":ip,
            "networks":networks, "scanning":SCANNING.load(Ordering::Relaxed) != 0,
            "setup":MODE_SETUP.load(Ordering::Relaxed) != 0,
            "reading":crate::stream::STREAM.is_reading(), "ready":crate::stream::STREAM.is_ready(),
            "restarting":RESTARTING.load(Ordering::Relaxed) != 0, "persisted":!radio.is_unsaved(),
            "wifi_message":status_message(), "wifi_status":STATUS.load(Ordering::Relaxed),
            "led":led_json(&BRIGHTNESS.status(Instant::now())),
        }).to_string()),
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
) -> (StatusCode, [(&'static str, &'static str); 1], Typed<String>) {
    use crate::diag::{DEFAULT_LINES, HISTORY_LINES};
    let n = n.unwrap_or(DEFAULT_LINES).clamp(1, HISTORY_LINES);
    let mut body = String::new();
    match diag.write_recent(n, &mut body) {
        Ok(()) => (StatusCode::OK, NO_STORE, json(body)),
        // An empty body: even a short message could fail to allocate when the heap is this short.
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            NO_STORE,
            json(String::new()),
        ),
    }
}
/// The active policy as the restricted YAML of `iobewi-log-config`, and whether it may be changed here.
fn log_config<B: ConfigBackend>(
    diag: &crate::diag::Diag<B>,
) -> ([(&'static str, &'static str); 1], Typed<String>) {
    use crate::diag::Source;
    let (yaml, source, load_error) = diag.policy_yaml();
    (
        NO_STORE,
        json(serde_json::json!({
            "yaml": yaml, "source": if source == Source::Persisted { "persisted" } else { "default" },
            "load_error": load_error, "writable": in_setup(),
            "max_bytes": iobewi_log_config::POLICY_BYTES_MAX,
        }).to_string()),
    )
}
/// Replaces the policy. Setup (AP) boot only: the LAN can read the diagnostic but never change it.
async fn put_log_config<B: ConfigBackend>(
    diag: &crate::diag::Diag<B>,
    body: &str,
) -> (StatusCode, [(&'static str, &'static str); 1], Typed<String>) {
    use crate::diag::PutError;
    // Boxed: the persistence future (policy copies, YAML buffer) lives on the heap for the one AP request
    // instead of inflating the task future of the whole web server.
    match alloc::boxed::Box::pin(diag.replace_yaml(body, in_setup())).await {
        Ok(generation) => (
            StatusCode::OK,
            NO_STORE,
            json(serde_json::json!({"ok": true, "generation": generation}).to_string()),
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
                NO_STORE,
                json(serde_json::json!({"ok": false, "error": message}).to_string()),
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
        // Declared first so that it adds no frame to the other routes (the LED routes never reach the flash).
        .nest(
            "/api/led",
            picoserve::Router::new()
                .route(
                    "/preview",
                    post(|Form(value): Form<PercentForm>| async move {
                        led_preview(&BRIGHTNESS, value.percent)
                    }),
                )
                .route("/cancel", post(|| async { led_cancel(&BRIGHTNESS) }))
                .route(
                    "/save",
                    post(|Form(value): Form<PercentForm>| async move {
                        led_save(&BRIGHTNESS, value.percent)
                    }),
                ),
        )
        .route("/", get(page))
        .route(
            "/status",
            get(|| async {
                (
                    NO_STORE,
                    Typed("text/plain; charset=utf-8", status_message()),
                )
            }),
        )
        .route("/wifi", post(provision))
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

/// How long a silent connection (no request byte) may hold a worker before the server closes it.
const START_READ_REQUEST: embassy_time::Duration = embassy_time::Duration::from_secs(10);
/// Bound on reading a request once it has started, **and on the closing handshake**: picoserve ends a connection
/// by sending its FIN and then waits, for at most this long, for the client to close its own side before the
/// worker listens again. A client that keeps its socket open (a browser's idle or lingering connection) holds the
/// worker for the whole time, and a connection arriving meanwhile is refused (RST, no backlog): it was 90 s (#28).
const READ_REQUEST: embassy_time::Duration = embassy_time::Duration::from_secs(5);
const WRITE: embassy_time::Duration = embassy_time::Duration::from_secs(5);

/// One bounded HTTP client at a time, no keep-alive. A connection attempt + DHCP may take time.
fn config() -> picoserve::Config {
    picoserve::Config::new(picoserve::Timeouts {
        start_read_request: START_READ_REQUEST,
        read_request: READ_REQUEST,
        write: WRITE,
        ..picoserve::Timeouts::const_default()
    })
}

pub async fn serve<B: ConfigBackend>(
    stack: Stack<'static>,
    radio: &Radio<B>,
    diag: &crate::diag::Diag<B>,
) -> ! {
    let router = app(radio, diag);
    let config = config();
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
            Poll::Ready((status, _)) => assert_eq!(status, StatusCode::BAD_REQUEST),
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
        let secure: StationForm = deserialize_form(UrlEncodedString(
            "name=Jazz&url=https%3A%2F%2Fexample.org%3A8443%2Flive.mp3%3Fk%3Dv&logo=",
        ))
        .unwrap();
        assert_eq!(secure.url, "https://example.org:8443/live.mp3?k=v");
        assert!(deserialize_form::<IdForm>(UrlEncodedString("id=2&extra=1")).is_err());
    }
    #[test]
    fn the_page_runs_the_catalog_filter_before_the_ui_once_the_document_is_there() {
        let parts = Page("").parts();
        let page: String = parts.concat();
        let body_end = page.rfind("</body>").expect("a closed body");
        let catalog = page
            .find(include_str!("catalog.js"))
            .expect("catalog.js is in the page");
        let ui = page
            .find(include_str!("ui.js"))
            .expect("ui.js is in the page");
        assert!(catalog < ui, "ui.js uses StreamCatalog");
        assert!(include_str!("ui.js").contains("StreamCatalog.sort"));
        // An inline script is not deferred: both come after the markup `ui.js` looks up, not in the head.
        let markup = page
            .find("id=\"stations\"")
            .expect("the favourites section");
        assert!(markup < catalog && ui < body_end);
        assert!(page.find("<style>").unwrap() < page.find("<main>").unwrap());
    }
    #[test]
    fn the_page_needs_no_other_download_and_leaves_no_marker() {
        let page: String = Page("ok").parts().concat();
        for marker in ["<!--CSS-->", "<!--S-->", "<!--CATALOG-->", "<!--UI-->"] {
            assert!(!page.contains(marker), "{marker} was not replaced");
        }
        for external in [
            "<link",
            "src=\"/",
            "src='/",
            "href=\"/ui",
            "href=\"/catalog",
        ] {
            assert!(
                !page.contains(external),
                "the page still fetches {external}"
            );
        }
        // The sources are inert text in their element: nothing in them can end it early.
        assert!(!include_str!("ui.css").contains("</style"));
        for script in [include_str!("ui.js"), include_str!("catalog.js")] {
            assert!(!script.contains("</script") && !script.contains("<!--"));
        }
        assert!(page.contains(">ok</p>"));
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
                        NO_STORE,
                        Typed("text/plain; charset=utf-8", status_message()),
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
            (
                "POST",
                "/api/stations/save",
                "name=C&url=https%3A%2F%2Fexample.org%2Fc.mp3",
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

    /// The LED routes through the real router: their statuses, their refusal without a LED, and what each
    /// one does to the brightness. Nothing here reaches the configuration.
    #[test]
    fn the_led_routes_preview_cancel_and_ask_for_a_save_and_refuse_without_a_led() {
        embassy_futures::block_on(async {
            let (radio, diag) = fixtures().await;
            let router = app(&radio, &diag);
            let call = |method, path, body| call(&router, method, path, body);
            let led = || BRIGHTNESS.status(Instant::now());

            // No LED task: every LED request is refused and changes nothing.
            BRIGHTNESS.set_available(false);
            for path in ["/api/led/preview", "/api/led/save"] {
                assert_eq!(call("POST", path, "percent=40").0, 409, "{path}");
            }
            assert_eq!(call("POST", "/api/led/cancel", "").0, 409);
            assert_eq!(led().preview, None);
            assert!(!led().busy);
            let state = call("GET", "/api/state", "").1;
            assert!(state.contains("\"available\":false"), "{state}");

            BRIGHTNESS.set_available(true);
            BRIGHTNESS.loaded(36);
            // A preview: shown, not saved, nothing asked of the configuration.
            assert_eq!(call("POST", "/api/led/preview", "percent=80").0, 200);
            assert_eq!(led().preview, Some(80));
            assert_eq!(led().saved, 36);
            assert!(!led().busy, "a preview asks for no save");
            // A value out of range, or not a number, is refused and changes nothing.
            assert_eq!(call("POST", "/api/led/preview", "percent=101").0, 400);
            for body in ["percent=abc", "percent=-1", "percent=300", "", "bogus=1"] {
                let status = call("POST", "/api/led/preview", body).0;
                assert!((400..500).contains(&status), "{body}: {status}");
            }
            assert_eq!(led().preview, Some(80));
            assert_eq!(call("POST", "/api/led/save", "percent=101").0, 400);
            assert!(!led().busy);
            // The cancel goes back to the saved value.
            assert_eq!(call("POST", "/api/led/cancel", "").0, 200);
            assert_eq!(led().preview, None);
            assert_eq!(led().applied, 36);
            // A save is accepted at once, as pending: the write is not done here.
            assert_eq!(call("POST", "/api/led/save", "percent=0").0, 202);
            assert!(led().busy);
            assert_eq!(
                led().saved,
                36,
                "the saved value changes when the write is done"
            );
            let state = call("GET", "/api/state", "").1;
            assert!(state.contains("\"busy\":true"), "{state}");
            for (method, path) in [("GET", "/api/led/preview"), ("PUT", "/api/led/save")] {
                assert_eq!(call(method, path, "").0, 405, "{method} {path}");
            }
            assert_eq!(call("POST", "/api/led/nothing", "").0, 404);
            BRIGHTNESS.set_available(false);
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

    /// What the browser receives, not what the handlers meant to send: one `Content-Type` per answer, the
    /// right one, and the exact bytes of the embedded file. picoserve adds `text/plain` to a `&str` or a
    /// `String` body, so naming the type in the headers as well made two contradictory headers.
    #[test]
    fn every_answer_carries_exactly_one_correct_content_type() {
        fn split(raw: &str) -> (Vec<(String, String)>, &str) {
            let (head, body) = raw.split_once("\r\n\r\n").expect("a complete answer");
            let headers = head
                .lines()
                .skip(1)
                .filter_map(|l| l.split_once(':'))
                .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
                .collect();
            (headers, body)
        }
        let expected: &[(&str, &str, &str)] = &[
            ("GET", "/", "text/html; charset=utf-8"),
            ("GET", "/status", "text/plain; charset=utf-8"),
            ("GET", "/api/state", "application/json; charset=utf-8"),
            ("GET", "/api/logs", "application/json; charset=utf-8"),
            ("GET", "/api/logs/config", "application/json; charset=utf-8"),
            (
                "POST",
                "/api/stations/save",
                "application/json; charset=utf-8",
            ),
            (
                "POST",
                "/api/stations/save",
                "application/json; charset=utf-8",
            ),
            (
                "POST",
                "/api/stations/select",
                "application/json; charset=utf-8",
            ),
            ("POST", "/wifi", "text/html; charset=utf-8"),
            ("PUT", "/api/logs/config", "application/json; charset=utf-8"),
        ];
        embassy_futures::block_on(async {
            let (radio, diag) = fixtures().await;
            let router = app(&radio, &diag);
            for (method, path, kind) in expected {
                let body = match *path {
                    "/api/stations/save" => FORM,
                    "/api/stations/select" => "id=1",
                    "/wifi" => "ssid=home&password=correct-horse",
                    "/api/logs/config" if *method == "PUT" => "log:\n  default_level: warn\n",
                    _ => "",
                };
                let (_, raw) = call(&router, method, path, body);
                let (headers, payload) = split(&raw);
                let types: Vec<&String> = headers
                    .iter()
                    .filter(|(n, _)| n == "content-type")
                    .map(|(_, v)| v)
                    .collect();
                assert_eq!(types, [&kind.to_string()], "{method} {path}: {raw:?}");
                let length: usize = headers
                    .iter()
                    .find(|(n, _)| n == "content-length")
                    .map(|(_, v)| v.parse().unwrap())
                    .unwrap_or_else(|| panic!("{method} {path}: no Content-Length"));
                assert_eq!(length, payload.len(), "{method} {path}: length");
            }
            // The page goes out as one answer, the three sources in it byte for byte, nothing cached.
            let (_, raw) = call(&router, "GET", "/", "");
            let (_, page) = split(&raw);
            for file in [
                include_str!("ui.css"),
                include_str!("catalog.js"),
                include_str!("ui.js"),
            ] {
                assert!(page.contains(file));
            }
            assert!(
                raw.contains("cache-control: no-store") || raw.contains("Cache-Control: no-store")
            );
            // The files it used to link are not routes any more.
            for path in ["/ui.css", "/ui.js", "/catalog.js"] {
                assert_eq!(call(&router, "GET", path, "").0, 404, "{path}");
            }
            // Whatever the route, a refusal or an unknown path never carries two types either.
            for (method, path) in [("GET", "/nothing"), ("GET", "/wifi"), ("DELETE", "/ui.css")] {
                let (_, raw) = call(&router, method, path, "");
                let n = split(&raw)
                    .0
                    .iter()
                    .filter(|(n, _)| n == "content-type")
                    .count();
                assert!(n <= 1, "{method} {path}: {n} content types");
            }
        });
    }
}

/// How long a worker stays unavailable after a client that does not behave: picoserve runs on a tokio socket here
/// (the firmware uses the embassy one; the connection logic is the same), with the firmware's own timeouts.
/// One worker as on the board, one connection at a time; a probe request measures when it is free again.
#[cfg(test)]
mod worker_release_tests {
    use super::*;
    use std::time::{Duration, Instant};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        task::LocalSet,
    };

    const WORKERS: usize = 1;
    const GET: &[u8] = b"GET /api/state HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n";

    async fn workers(listener: TcpListener) {
        let listener = std::rc::Rc::new(listener);
        for _ in 0..WORKERS {
            let listener = listener.clone();
            tokio::task::spawn_local(async move {
                let router = picoserve::Router::new()
                    .route("/api/state", picoserve::routing::get(|| async { "{}" }));
                let config = config();
                let mut buffer = [0; 2048];
                loop {
                    let (socket, _) = listener.accept().await.unwrap();
                    let _ = picoserve::Server::new_tokio(&router, &config, &mut buffer)
                        .serve(socket)
                        .await;
                }
            });
        }
    }

    /// A well-behaved client: one request, read the answer to the end, close. Returns the wait until the answer.
    async fn probe(addr: std::net::SocketAddr) -> Duration {
        let started = Instant::now();
        let mut socket = TcpStream::connect(addr).await.unwrap();
        socket.write_all(GET).await.unwrap();
        let mut answer = Vec::new();
        socket.read_to_end(&mut answer).await.unwrap();
        assert!(answer.starts_with(b"HTTP/1.1 200"), "{answer:?}");
        started.elapsed()
    }

    async fn run<F: std::future::Future<Output = ()>>(
        scenario: impl FnOnce(std::net::SocketAddr) -> F,
    ) {
        LocalSet::new()
            .run_until(async {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                workers(listener).await;
                scenario(addr).await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_normal_request_is_answered_at_once() {
        run(|addr| async move {
            assert!(probe(addr).await < Duration::from_secs(1));
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clients_that_keep_their_socket_open_after_the_answer_free_the_workers_within_read_request()
     {
        run(|addr| async move {
            let mut held = Vec::new();
            for _ in 0..WORKERS {
                let mut socket = TcpStream::connect(addr).await.unwrap();
                socket.write_all(GET).await.unwrap();
                let mut answer = Vec::new();
                socket.read_to_end(&mut answer).await.unwrap(); // the server's FIN is in; ours is not sent
                held.push(socket);
            }
            let waited = probe(addr).await;
            eprintln!("answer-then-hold: next request served after {waited:?}");
            assert!(
                waited >= Duration::from_secs(4),
                "{waited:?}: they were not held at all"
            );
            assert!(waited < Duration::from_secs(8), "{waited:?}");
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn partially_sent_requests_free_the_workers_within_read_request() {
        run(|addr| async move {
            let mut held = Vec::new();
            for _ in 0..WORKERS {
                let mut socket = TcpStream::connect(addr).await.unwrap();
                socket
                    .write_all(b"GET /api/state HTTP/1.1\r\nHost: t\r\n")
                    .await
                    .unwrap();
                held.push(socket);
            }
            let waited = probe(addr).await;
            eprintln!("partial request: next request served after {waited:?}");
            assert!(waited < Duration::from_secs(8), "{waited:?}");
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn silent_connections_free_the_workers_within_start_and_read_request() {
        run(|addr| async move {
            let mut held = Vec::new();
            for _ in 0..WORKERS {
                held.push(TcpStream::connect(addr).await.unwrap());
            }
            let waited = probe(addr).await;
            eprintln!("silent connections: next request served after {waited:?}");
            assert!(
                waited >= Duration::from_secs(9),
                "{waited:?}: not held for the start timeout"
            );
            assert!(waited < Duration::from_secs(17), "{waited:?}");
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_slow_request_within_the_limit_is_still_served() {
        run(|addr| async move {
            let mut socket = TcpStream::connect(addr).await.unwrap();
            socket.write_all(&GET[..20]).await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
            socket.write_all(&GET[20..]).await.unwrap();
            let mut answer = Vec::new();
            socket.read_to_end(&mut answer).await.unwrap();
            assert!(answer.starts_with(b"HTTP/1.1 200"), "{answer:?}");
        })
        .await;
    }
}
