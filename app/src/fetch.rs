//! Reaching a station: the plain-TCP connector and the redirect policy.
//!
//! One path serves `http://` and `https://` (see `stream::produce`): the URL is parsed by
//! `iobewi-http-client`, a `Connector` opens the connection (this module's [`TcpConnector`] for `http`,
//! the target's TLS connector for `https`), and a redirect is a new target reached with a new
//! connection, hence for `https` a new certificate verification of whatever host it names.
use alloc::{string::String, vec::Vec};
use core::fmt;
use embassy_net::{
    IpEndpoint, Stack,
    dns::DnsQueryType,
    tcp::{ConnectError, Error as TcpIoError, TcpSocket},
};
use iobewi_http_client::{Target, TargetError};
use iobewi_net_io::{Close, Connector, ErrorType, Read, Write};

/// Redirects followed for one attempt; the sixth is refused.
pub const MAX_REDIRECTS: usize = 5;

/// The statuses a radio `GET` follows. 300, 304, 305 and 306 are not redirects of a stream.
pub fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Why a redirect was not followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// More than [`MAX_REDIRECTS`] redirects.
    TooMany,
    /// The target was already visited by this attempt.
    Loop,
    /// `https` to `http`.
    Downgrade,
    /// A `Location` that is absent, relative or unusable.
    Malformed,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooMany => "too many redirects",
            Self::Loop => "redirect loop",
            Self::Downgrade => "redirect from https to http refused",
            Self::Malformed => "unusable Location",
        })
    }
}

/// The targets one attempt has been through, from the station's own URL.
pub struct Chain {
    seen: Vec<Target>,
}

fn same(a: &Target, b: &Target) -> bool {
    a.tls == b.tls && a.port == b.port && a.path == b.path && a.host.eq_ignore_ascii_case(&b.host)
}

impl Chain {
    pub fn new(first: &Target) -> Self {
        Self {
            seen: alloc::vec![first.clone()],
        }
    }

    /// Where `Location` leads from `from` (the last target), if it may be followed.
    pub fn follow(&mut self, from: &Target, location: Option<&str>) -> Result<Target, Refusal> {
        let location = location.ok_or(Refusal::Malformed)?;
        let next = from.redirect(location).map_err(|e| match e {
            TargetError::Downgrade => Refusal::Downgrade,
            _ => Refusal::Malformed,
        })?;
        if self.seen.iter().any(|t| same(t, &next)) {
            return Err(Refusal::Loop);
        }
        if self.seen.len() > MAX_REDIRECTS {
            return Err(Refusal::TooMany);
        }
        self.seen.push(next.clone());
        Ok(next)
    }
}

/// Plain TCP to `host:port`, name resolved by the stack's DNS client. No guarantee of any kind:
/// the target decides, per URL scheme, which connector a station gets.
pub struct TcpConnector {
    pub stack: Stack<'static>,
}

#[derive(Debug)]
pub enum TcpError {
    Dns,
    NoAddress,
    Connect(ConnectError),
}

impl fmt::Display for TcpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dns => f.write_str("dns: lookup failed"),
            Self::NoAddress => f.write_str("dns: no IPv4 address"),
            Self::Connect(e) => write!(f, "tcp: connect failed ({e:?})"),
        }
    }
}

pub struct TcpConnection<'a>(TcpSocket<'a>);

impl ErrorType for TcpConnection<'_> {
    type Error = TcpIoError;
}
impl Read for TcpConnection<'_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.0.read(buf).await
    }
}
impl Write for TcpConnection<'_> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.0.write(buf).await
    }
    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.0.flush().await
    }
}
impl Close for TcpConnection<'_> {
    /// Queue the FIN; dropping the connection releases the socket.
    async fn close(&mut self) -> Result<(), Self::Error> {
        self.0.close();
        Ok(())
    }
}

impl Connector for TcpConnector {
    type Error = TcpError;
    type Connection<'a> = TcpConnection<'a>;

    async fn connect<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        rx: &'a mut [u8],
        tx: &'a mut [u8],
    ) -> Result<Self::Connection<'a>, Self::Error> {
        let addresses = self
            .stack
            .dns_query(host, DnsQueryType::A)
            .await
            .map_err(|_| TcpError::Dns)?;
        let address = addresses.first().ok_or(TcpError::NoAddress)?;
        let mut socket = TcpSocket::new(self.stack, rx, tx);
        socket
            .connect(IpEndpoint::new(*address, port))
            .await
            .map_err(TcpError::Connect)?;
        Ok(TcpConnection(socket))
    }
}

/// `scheme://host[:port]`, for logs: never the path or the query, which can carry a token.
pub fn origin(target: &Target) -> String {
    alloc::format!(
        "{}://{}",
        if target.tls { "https" } else { "http" },
        target.authority()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(url: &str) -> Target {
        Target::parse(url).unwrap()
    }

    #[test]
    fn exactly_the_five_redirect_statuses_are_followed() {
        for s in [301, 302, 303, 307, 308] {
            assert!(is_redirect(s), "{s}");
        }
        for s in [200, 204, 300, 304, 305, 306, 400, 404, 500] {
            assert!(!is_redirect(s), "{s}");
        }
    }

    #[test]
    fn a_chain_of_five_redirects_is_followed_the_sixth_is_refused() {
        let mut cur = t("https://a.example/0");
        let mut chain = Chain::new(&cur);
        for i in 1..=MAX_REDIRECTS {
            cur = chain
                .follow(&cur, Some(&alloc::format!("/{i}")))
                .unwrap_or_else(|e| panic!("hop {i}: {e}"));
        }
        assert_eq!(chain.follow(&cur, Some("/6")), Err(Refusal::TooMany));
    }

    #[test]
    fn a_loop_is_detected_even_through_another_host() {
        let a = t("https://a.example/s");
        let mut chain = Chain::new(&a);
        let b = chain.follow(&a, Some("https://b.example/s")).unwrap();
        assert_eq!(
            chain.follow(&b, Some("https://a.example/s")),
            Err(Refusal::Loop)
        );
        // The same target spelt with another case of the host is the same target.
        assert_eq!(
            chain.follow(&b, Some("https://A.EXAMPLE/s")),
            Err(Refusal::Loop)
        );
        // A redirect to itself.
        assert_eq!(chain.follow(&b, Some("/s")), Err(Refusal::Loop));
    }

    #[test]
    fn same_path_on_another_port_or_scheme_is_not_a_loop() {
        let a = t("http://a.example/s");
        let mut chain = Chain::new(&a);
        let b = chain.follow(&a, Some("https://a.example/s")).unwrap();
        assert!(b.tls);
        assert!(chain.follow(&b, Some("https://a.example:8443/s")).is_ok());
    }

    #[test]
    fn https_never_falls_back_to_http() {
        let a = t("https://a.example/s");
        let mut chain = Chain::new(&a);
        for loc in ["http://a.example/s", "http://other.example/x"] {
            assert_eq!(
                chain.follow(&a, Some(loc)),
                Err(Refusal::Downgrade),
                "{loc}"
            );
        }
        // The other way is allowed: http may upgrade.
        let p = t("http://a.example/s");
        assert!(
            Chain::new(&p)
                .follow(&p, Some("https://a.example/s"))
                .unwrap()
                .tls
        );
    }

    #[test]
    fn a_missing_or_unusable_location_is_refused() {
        let a = t("https://a.example/s");
        let mut chain = Chain::new(&a);
        for loc in [
            None,
            Some(""),
            Some("relative/x"),
            Some("//evil.example/x"),
            Some("ftp://x/"),
        ] {
            assert_eq!(chain.follow(&a, loc), Err(Refusal::Malformed), "{loc:?}");
        }
    }

    #[test]
    fn a_redirect_to_a_new_host_is_a_new_target_to_verify_afresh() {
        let a = t("https://a.example/s");
        let b = Chain::new(&a)
            .follow(&a, Some("https://cdn.b.example:8443/live?k=v#x"))
            .unwrap();
        assert_eq!(
            (b.host.as_str(), b.port, b.path.as_str()),
            ("cdn.b.example", 8443, "/live?k=v")
        );
    }

    #[test]
    fn the_logged_origin_hides_the_path_and_the_query() {
        assert_eq!(
            origin(&t("https://a.example/live?token=s3cret")),
            "https://a.example"
        );
        assert_eq!(
            origin(&t("http://a.example:8000/x")),
            "http://a.example:8000"
        );
    }
}
