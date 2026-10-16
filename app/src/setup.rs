//! Product provisioning page on the AP network only. Radio and DHCP are provided by the target.
//! HTTP uses picoserve; both UIs submit to the single owner of the Wi-Fi manager.
use alloc::string::String;
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use embassy_net::Stack;
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel, signal::Signal,
};
use iobewi_wifi_core::AccessPointConfig;
use picoserve::{
    extract::Form,
    response::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;

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

pub(crate) fn status_word() -> &'static str {
    match STATUS.load(Ordering::Relaxed) {
        1 => "pending",
        2 => "saved",
        3 => "connection",
        4 => "storage",
        _ => "none",
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
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    REQUESTS
        .send(Request {
            id,
            ssid: credentials.ssid,
            password: credentials.password,
        })
        .await;
    // Only one HTTP connection is served. An expired/disconnected request may leave a late reply;
    // the sequence number prevents a later request from accepting that stale result.
    let outcome = loop {
        let (reply_id, outcome) = RESULT.wait().await;
        if reply_id == id {
            break outcome;
        }
    };
    match outcome {
        Outcome::Saved => (
            StatusCode::OK,
            ("Content-Type", "text/html; charset=utf-8"),
            "<html lang=fr><meta charset=utf-8><h1>Wi-Fi enregistré</h1><p>Débranchez puis rebranchez StreamBeWI pour écouter la radio. Le disque USB apparaîtra après connexion et préchargement du flux.</p></html>",
        ),
        Outcome::Connection => (
            StatusCode::BAD_REQUEST,
            ("Content-Type", "text/html; charset=utf-8"),
            "<html lang=fr><meta charset=utf-8><p>Connexion impossible ou enregistrement Wi-Fi échoué. Vérifiez le réseau et le mot de passe.</p><a href=/>Réessayer</a></html>",
        ),
        Outcome::Storage => (
            StatusCode::SERVICE_UNAVAILABLE,
            ("Content-Type", "text/html; charset=utf-8"),
            "<html lang=fr><meta charset=utf-8><p>Wi-Fi enregistré, mais activation USB non enregistrée. Réessayez avant de redémarrer.</p><a href=/>Réessayer</a></html>",
        ),
    }
}

/// No listener is bound to STA; an AP client must open the explicit local address (no captive DNS).
pub async fn serve(stack: Stack<'static>) -> ! {
    let router = picoserve::Router::new()
        .route(
            "/",
            get(|| async { (("Content-Type", "text/html; charset=utf-8"), PAGE) }),
        )
        .route(
            "/status",
            get(|| async { (("Content-Type", "text/plain; charset=utf-8"), status_word()) }),
        )
        .route("/wifi", post(provision));
    // A connection attempt + DHCP may take time. One bounded HTTP client, no keep-alive.
    let config = picoserve::Config::new(picoserve::Timeouts {
        start_read_request: embassy_time::Duration::from_secs(10),
        read_request: embassy_time::Duration::from_secs(90),
        write: embassy_time::Duration::from_secs(5),
        ..picoserve::Timeouts::const_default()
    });
    let mut http = [0; 1024];
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
    fn ap_configuration_matches_the_page_contract() {
        let ap = access_point_config();
        assert_eq!(ap.ssid(), AP_SSID);
        assert_eq!(ap.channel(), 6);
    }
}
