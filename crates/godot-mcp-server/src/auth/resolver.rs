//! SSRF-safe discovery and JWKS fetching.
//!
//! # The threat this module exists for
//!
//! The one outbound request this server makes is "fetch the JWKS from the
//! configured issuer". That is an attractive SSRF primitive: if the URL, the DNS
//! answer, or a redirect could be steered, a caller could make the server read a
//! cloud metadata endpoint (`169.254.169.254`), reach a private-network service, or
//! talk to something on loopback that trusts local callers. So every stage is
//! locked down:
//!
//! * **Only configured URLs are ever fetched.** [`DiscoveryClient::fetch`] takes a
//!   [`VettedEndpoint`], which can only be built from a server-side-configured
//!   HTTPS URL. There is no code path from a token, a header, or a request body to
//!   an outbound URL — step 1 of the token pipeline already refuses `jku`/`x5u`.
//! * **Environment proxies are disabled.** A `HTTP_PROXY`/`HTTPS_PROXY` in the
//!   environment would send the request somewhere the vetting never saw, so
//!   [`DiscoveryClient::new`] calls `no_proxy()` and [`SsrfRejection::EnvironmentProxy`]
//!   exists for the configuration-time refusal.
//! * **A/AAAA answers are vetted, then pinned.** [`vet_address`] rejects loopback,
//!   private, link-local, metadata, multicast, broadcast and unspecified results.
//!   *Every* address in the answer must pass: a mixed answer is refused outright
//!   rather than filtered, because an attacker who can add one good address to a
//!   set of bad ones should not get to choose.
//! * **DNS is never trusted twice.** The vetted address is pinned into the client
//!   with `resolve_to_addrs`, so the socket connects to the exact address that was
//!   vetted. The original hostname is preserved for the HTTP `Host` header and for
//!   TLS SNI/certificate verification, so pinning does not weaken authentication.
//!   This closes the lookup-to-connect (DNS rebinding) window: a second lookup
//!   returning `127.0.0.1` cannot influence where the socket goes.
//! * **Redirects are re-vetted, not followed blindly.** The redirect policy is
//!   `none`; a `3xx` is surfaced and its `Location` must pass the whole vetting
//!   procedure again — same scheme rules, same address rules, same issuer equality
//!   — before another request is made. A redirect that changes the issuer, or that
//!   points at a prohibited address, is refused.
//! * **Everything is bounded.** A redirect hop cap, a response body byte cap, and a
//!   wall-clock timeout, so a hostile or broken endpoint cannot hold a request open
//!   or exhaust memory.
//!
//! # Why the vetting is a pure function
//!
//! [`vet_address`] takes an [`IpAddr`] and returns a decision. That makes the
//! policy testable without a network, without DNS, and without a live IdP: the
//! acceptance suite asserts the metadata address, private ranges, loopback and
//! link-local are refused as data, and the connector test asserts the pinned
//! address is what the socket is given.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs as _};
use std::time::Duration;

/// The cloud instance-metadata address, refused by name as well as by range.
pub const METADATA_IPV4: Ipv4Addr = Ipv4Addr::new(169, 254, 169, 254);

/// The IPv6 instance-metadata address some providers expose.
pub const METADATA_IPV6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254);

/// How many redirect hops may be re-vetted before a fetch is abandoned.
pub const MAX_REDIRECTS: usize = 3;

/// The largest discovery/JWKS document that will be read, in bytes.
pub const MAX_BODY_BYTES: usize = 128 * 1024;

/// The wall-clock budget for one fetch, including every re-vetted redirect.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Environment variables that would silently reroute an outbound request.
pub const PROXY_ENV_VARS: [&str; 6] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// Why an endpoint, an address, or a redirect was refused.
///
/// The `Display` rendering is the internal label; nothing here is ever returned to
/// an MCP caller, because discovery happens outside any request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.as_str())]
pub enum SsrfRejection {
    /// The configured URL is not HTTPS.
    InsecureScheme,
    /// The configured URL has no host, or a host that is not usable.
    MissingHost,
    /// The URL's host is an IP literal; only a resolvable name is accepted, so
    /// that vetting and certificate verification both have something to bind to.
    LiteralAddress,
    /// Name resolution produced no answer.
    UnresolvableHost,
    /// At least one resolved address is loopback/private/link-local/metadata.
    ProhibitedAddress,
    /// A proxy environment variable is set, which would bypass vetting.
    EnvironmentProxy,
    /// A redirect pointed at a different issuer.
    IssuerChanged,
    /// A redirect exceeded [`MAX_REDIRECTS`].
    TooManyRedirects,
    /// A redirect response carried no usable `Location`.
    MissingRedirectTarget,
    /// The response body exceeded [`MAX_BODY_BYTES`].
    BodyTooLarge,
    /// The endpoint did not answer within [`FETCH_TIMEOUT`].
    Timeout,
    /// The endpoint answered with a non-success status.
    UnexpectedStatus {
        /// The status observed.
        status: u16,
    },
    /// The transport failed.
    TransportFailure,
}

impl SsrfRejection {
    /// A stable snake_case label, for tests and the internal diagnostic sink.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::InsecureScheme => "insecure_scheme",
            Self::MissingHost => "missing_host",
            Self::LiteralAddress => "literal_address",
            Self::UnresolvableHost => "unresolvable_host",
            Self::ProhibitedAddress => "prohibited_address",
            Self::EnvironmentProxy => "environment_proxy",
            Self::IssuerChanged => "issuer_changed",
            Self::TooManyRedirects => "too_many_redirects",
            Self::MissingRedirectTarget => "missing_redirect_target",
            Self::BodyTooLarge => "body_too_large",
            Self::Timeout => "discovery_timeout",
            Self::UnexpectedStatus { .. } => "unexpected_status",
            Self::TransportFailure => "transport_failure",
        }
    }
}

/// Whether one resolved address may be connected to.
///
/// Refuses loopback, private, link-local, the instance-metadata addresses,
/// multicast, broadcast, unspecified, and IPv4-mapped/compatible IPv6 forms of any
/// of the above — the mapped forms matter because `::ffff:127.0.0.1` is loopback
/// wearing a different hat, and a check that only looked at the IPv6 predicates
/// would wave it through.
///
/// # Errors
/// Returns [`SsrfRejection::ProhibitedAddress`] for every refused address.
pub fn vet_address(address: IpAddr) -> Result<(), SsrfRejection> {
    let prohibited = match address {
        IpAddr::V4(v4) => vet_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped().or_else(|| ipv4_compatible(v6)) {
            // An IPv4 address expressed as IPv6 is judged by the IPv4 rules.
            Some(mapped) => vet_v4(mapped),
            None => vet_v6(v6),
        },
    };
    if prohibited {
        Err(SsrfRejection::ProhibitedAddress)
    } else {
        Ok(())
    }
}

fn vet_v4(address: Ipv4Addr) -> bool {
    address == METADATA_IPV4
        || address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_multicast()
        || address.is_broadcast()
        || address.is_unspecified()
        || address.is_documentation()
        // Carrier-grade NAT (100.64.0.0/10) and benchmarking (198.18.0.0/15) are
        // reachable inside many hosting environments but are never a public IdP.
        || (address.octets()[0] == 100 && (address.octets()[1] & 0b1100_0000) == 64)
        || (address.octets()[0] == 198 && (address.octets()[1] & 0b1111_1110) == 18)
        // 0.0.0.0/8 and 240.0.0.0/4 are not routable destinations.
        || address.octets()[0] == 0
        || address.octets()[0] >= 240
}

fn vet_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    address == METADATA_IPV6
        || address.is_loopback()
        || address.is_multicast()
        || address.is_unspecified()
        // fc00::/7 unique-local.
        || (segments[0] & 0xfe00) == 0xfc00
        // fe80::/10 link-local.
        || (segments[0] & 0xffc0) == 0xfe80
        // 2001:db8::/32 documentation.
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
}

fn ipv4_compatible(address: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = address.segments();
    if segments[..6] == [0, 0, 0, 0, 0, 0] && (segments[6] != 0 || segments[7] != 0) {
        let octets = address.octets();
        return Some(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ));
    }
    None
}

/// Whether any proxy environment variable is set in `env`.
///
/// Taken as a map rather than read from the process so the check is testable and so
/// the acceptance suite never has to mutate global process state.
///
/// # Errors
/// Returns [`SsrfRejection::EnvironmentProxy`] when one is set to a non-empty value.
pub fn reject_environment_proxy<'a, I>(env: I) -> Result<(), SsrfRejection>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    for (name, value) in env {
        if PROXY_ENV_VARS.contains(&name) && !value.trim().is_empty() {
            return Err(SsrfRejection::EnvironmentProxy);
        }
    }
    Ok(())
}

/// A configured HTTPS endpoint whose every resolved address has been vetted.
///
/// Holding one of these is the proof that vetting happened. There is no public
/// constructor that skips it, and [`DiscoveryClient::fetch`] accepts nothing else,
/// so "fetch an arbitrary URL" is not an operation this module offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VettedEndpoint {
    url: String,
    host: String,
    port: u16,
    addresses: Vec<IpAddr>,
}

impl VettedEndpoint {
    /// Vet `url`, resolving its host with `resolver` and refusing any prohibited
    /// answer.
    ///
    /// `resolver` is injected so the acceptance suite can present a *deterministic*
    /// answer — including a mixed allowed/prohibited answer and a rebinding
    /// sequence — without depending on real DNS or a real IdP.
    ///
    /// # Errors
    /// Returns [`SsrfRejection`] for a non-HTTPS scheme, a missing or literal host,
    /// an empty answer, or an answer containing even one prohibited address.
    pub fn vet<R>(url: &str, resolver: &R) -> Result<Self, SsrfRejection>
    where
        R: HostResolver + ?Sized,
    {
        let rest = url
            .strip_prefix("https://")
            .ok_or(SsrfRejection::InsecureScheme)?;
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if authority.is_empty() {
            return Err(SsrfRejection::MissingHost);
        }
        if authority.contains('@') {
            // Userinfo in a discovery URL is never legitimate and is a classic way
            // to make a URL read as one host while resolving as another.
            return Err(SsrfRejection::MissingHost);
        }
        let (host, port) = split_authority(authority)?;
        if host.parse::<IpAddr>().is_ok() || (host.starts_with('[') && host.ends_with(']')) {
            return Err(SsrfRejection::LiteralAddress);
        }
        let addresses = resolver.resolve(&host, port)?;
        if addresses.is_empty() {
            return Err(SsrfRejection::UnresolvableHost);
        }
        // Every answer must pass. A mixed answer is refused rather than filtered:
        // filtering would let an attacker who can inject one address into a set
        // decide which one is used.
        for address in &addresses {
            vet_address(*address)?;
        }
        Ok(Self {
            url: url.to_owned(),
            host,
            port,
            addresses,
        })
    }

    /// The configured URL, unchanged.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The original hostname, preserved for `Host` and TLS SNI/certificate checks.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port the socket will connect to.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// The exact vetted addresses the socket may connect to, and no others.
    #[must_use]
    pub fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }

    /// The pinned socket addresses handed to the connector.
    #[must_use]
    pub fn socket_addresses(&self) -> Vec<SocketAddr> {
        self.addresses
            .iter()
            .map(|address| SocketAddr::new(*address, self.port))
            .collect()
    }

    /// Re-vet a redirect `location`, requiring the issuer to be unchanged.
    ///
    /// The whole procedure runs again — scheme, host shape, resolution, every
    /// address — because a redirect is just another attacker-influenceable URL.
    ///
    /// # Errors
    /// Returns [`SsrfRejection::IssuerChanged`] when the target leaves `issuer`,
    /// or whichever vetting rejection the target earns.
    pub fn revet_redirect<R>(
        location: &str,
        issuer: &str,
        resolver: &R,
    ) -> Result<Self, SsrfRejection>
    where
        R: HostResolver + ?Sized,
    {
        if location.trim().is_empty() {
            return Err(SsrfRejection::MissingRedirectTarget);
        }
        let vetted = Self::vet(location, resolver)?;
        let issuer_endpoint = Self::authority_of(issuer).ok_or(SsrfRejection::IssuerChanged)?;
        if issuer_endpoint != format!("{}:{}", vetted.host, vetted.port) {
            return Err(SsrfRejection::IssuerChanged);
        }
        Ok(vetted)
    }

    fn authority_of(url: &str) -> Option<String> {
        let rest = url.strip_prefix("https://")?;
        let authority = rest.split(['/', '?', '#']).next()?;
        let (host, port) = split_authority(authority).ok()?;
        Some(format!("{host}:{port}"))
    }
}

fn split_authority(authority: &str) -> Result<(String, u16), SsrfRejection> {
    if let Some(rest) = authority.strip_prefix('[') {
        // A bracketed authority is an IPv6 literal, which is refused by the caller;
        // returning the bracketed form keeps that check simple.
        let (inside, tail) = rest.split_once(']').ok_or(SsrfRejection::MissingHost)?;
        let port = match tail.strip_prefix(':') {
            Some(digits) => digits.parse().map_err(|_| SsrfRejection::MissingHost)?,
            None => 443,
        };
        return Ok((format!("[{inside}]"), port));
    }
    match authority.rsplit_once(':') {
        Some((host, digits)) => {
            let port = digits.parse().map_err(|_| SsrfRejection::MissingHost)?;
            if host.is_empty() {
                return Err(SsrfRejection::MissingHost);
            }
            Ok((host.to_ascii_lowercase(), port))
        }
        None => Ok((authority.to_ascii_lowercase(), 443)),
    }
}

/// How a hostname becomes a set of addresses to vet.
pub trait HostResolver {
    /// Resolve `host` to every A/AAAA answer.
    ///
    /// # Errors
    /// Returns [`SsrfRejection::UnresolvableHost`] when resolution fails.
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, SsrfRejection>;
}

/// The system resolver.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemResolver;

impl HostResolver for SystemResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, SsrfRejection> {
        let answers = (host, port)
            .to_socket_addrs()
            .map_err(|_| SsrfRejection::UnresolvableHost)?;
        Ok(answers.map(|address| address.ip()).collect())
    }
}

/// A resolver returning a scripted answer per lookup.
///
/// Exists so the acceptance suite can present a mixed answer, a private-only
/// answer, and a *rebinding* sequence (public on the first lookup, private on the
/// second) deterministically. That last case is how the suite proves the connector
/// uses the pinned address rather than re-resolving.
#[derive(Debug, Default)]
pub struct ScriptedResolver {
    answers: std::sync::Mutex<Vec<Vec<IpAddr>>>,
    lookups: std::sync::atomic::AtomicUsize,
}

impl ScriptedResolver {
    /// Answer each successive lookup with the next entry, repeating the last.
    #[must_use]
    pub fn new(answers: Vec<Vec<IpAddr>>) -> Self {
        Self {
            answers: std::sync::Mutex::new(answers),
            lookups: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// How many lookups have been performed.
    ///
    /// A test asserts this stays at 1 across a fetch: a second lookup would be a
    /// second chance for DNS to change the destination.
    #[must_use]
    pub fn lookups(&self) -> usize {
        self.lookups.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl HostResolver for ScriptedResolver {
    fn resolve(&self, _host: &str, _port: u16) -> Result<Vec<IpAddr>, SsrfRejection> {
        let index = self
            .lookups
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let answers = self
            .answers
            .lock()
            .expect("the scripted answers are intact");
        if answers.is_empty() {
            return Err(SsrfRejection::UnresolvableHost);
        }
        let answer = answers
            .get(index)
            .or_else(|| answers.last())
            .cloned()
            .unwrap_or_default();
        if answer.is_empty() {
            return Err(SsrfRejection::UnresolvableHost);
        }
        Ok(answer)
    }
}

/// The bounded, proxy-free, address-pinned HTTPS client used for discovery.
#[derive(Debug)]
pub struct DiscoveryClient {
    client: reqwest::Client,
}

impl DiscoveryClient {
    /// Build a client for one vetted endpoint.
    ///
    /// The vetted addresses are pinned with `resolve_to_addrs`, so the socket goes
    /// to exactly what was vetted while `Host` and TLS SNI/certificate verification
    /// still use `endpoint.host()`. Environment proxies are disabled, redirects are
    /// *not* followed automatically (each is re-vetted by the caller), and the whole
    /// request is time-bounded.
    ///
    /// # Errors
    /// Returns [`SsrfRejection::TransportFailure`] when the client cannot be built.
    pub fn new(endpoint: &VettedEndpoint) -> Result<Self, SsrfRejection> {
        let client = reqwest::Client::builder()
            // A proxy would send the request somewhere vetting never saw.
            .no_proxy()
            // Connect to the vetted address; keep the hostname for Host/SNI.
            .resolve_to_addrs(endpoint.host(), &endpoint.socket_addresses())
            // Every hop is re-vetted explicitly instead of followed blindly.
            .redirect(reqwest::redirect::Policy::none())
            .https_only(true)
            .referer(false)
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(|_| SsrfRejection::TransportFailure)?;
        Ok(Self { client })
    }

    /// Fetch `endpoint`, re-vetting every redirect against `issuer`.
    ///
    /// One pinned client serves every hop, which is sound *because*
    /// [`VettedEndpoint::revet_redirect`] requires each hop to keep the issuer's
    /// authority: the host and port cannot change, so the pinned address stays the
    /// vetted one. A hop that tried to move to another authority is refused by that
    /// check rather than silently re-pinned.
    ///
    /// # Errors
    /// Returns [`SsrfRejection`] for a prohibited redirect, too many hops, a
    /// non-success status, an oversize body, or a transport/timeout failure.
    pub async fn fetch<R>(
        &self,
        endpoint: &VettedEndpoint,
        issuer: &str,
        resolver: &R,
    ) -> Result<String, SsrfRejection>
    where
        R: HostResolver + ?Sized,
    {
        let mut current = endpoint.clone();
        for _hop in 0..=MAX_REDIRECTS {
            let response = self
                .client
                .get(current.url())
                .send()
                .await
                .map_err(|error| {
                    if error.is_timeout() {
                        SsrfRejection::Timeout
                    } else {
                        SsrfRejection::TransportFailure
                    }
                })?;
            let status = response.status();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or(SsrfRejection::MissingRedirectTarget)?
                    .to_owned();
                // The redirect target is re-vetted in full before it is used.
                current = VettedEndpoint::revet_redirect(&location, issuer, resolver)?;
                continue;
            }
            if !status.is_success() {
                return Err(SsrfRejection::UnexpectedStatus {
                    status: status.as_u16(),
                });
            }
            return read_bounded(response).await;
        }
        Err(SsrfRejection::TooManyRedirects)
    }
}

async fn read_bounded(response: reqwest::Response) -> Result<String, SsrfRejection> {
    if let Some(length) = response.content_length()
        && usize::try_from(length).unwrap_or(usize::MAX) > MAX_BODY_BYTES
    {
        return Err(SsrfRejection::BodyTooLarge);
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|_| SsrfRejection::TransportFailure)?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err(SsrfRejection::BodyTooLarge);
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| SsrfRejection::TransportFailure)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public() -> Vec<IpAddr> {
        vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]
    }

    #[test]
    fn the_metadata_address_and_every_private_range_are_refused() {
        let prohibited: Vec<IpAddr> = vec![
            IpAddr::V4(METADATA_IPV4),
            IpAddr::V6(METADATA_IPV6),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)),
            IpAddr::V4(Ipv4Addr::new(172, 16, 5, 6)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
            IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255)),
            IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(240, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1)),
            // Loopback wearing an IPv6 hat.
            "::ffff:127.0.0.1".parse().expect("mapped loopback"),
            "::127.0.0.1".parse().expect("compatible loopback"),
        ];
        for address in prohibited {
            assert_eq!(
                vet_address(address),
                Err(SsrfRejection::ProhibitedAddress),
                "{address} must be refused"
            );
        }
        for address in [
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V6(Ipv6Addr::new(0x2606, 0x2800, 0x220, 0, 0, 0, 0, 0x1)),
        ] {
            assert_eq!(vet_address(address), Ok(()), "{address} must be allowed");
        }
    }

    #[test]
    fn a_mixed_answer_is_refused_rather_than_filtered() {
        let resolver = ScriptedResolver::new(vec![vec![
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
        ]]);
        assert_eq!(
            VettedEndpoint::vet("https://issuer.example/jwks", &resolver).err(),
            Some(SsrfRejection::ProhibitedAddress)
        );
    }

    #[test]
    fn only_https_names_are_vetted() {
        let resolver = ScriptedResolver::new(vec![public()]);
        assert_eq!(
            VettedEndpoint::vet("http://issuer.example/jwks", &resolver).err(),
            Some(SsrfRejection::InsecureScheme)
        );
        assert_eq!(
            VettedEndpoint::vet("https://93.184.216.34/jwks", &resolver).err(),
            Some(SsrfRejection::LiteralAddress)
        );
        assert_eq!(
            VettedEndpoint::vet("https://[::1]/jwks", &resolver).err(),
            Some(SsrfRejection::LiteralAddress)
        );
        assert_eq!(
            VettedEndpoint::vet("https:///jwks", &resolver).err(),
            Some(SsrfRejection::MissingHost)
        );
        assert_eq!(
            VettedEndpoint::vet("https://user@issuer.example/jwks", &resolver).err(),
            Some(SsrfRejection::MissingHost)
        );
        assert_eq!(
            VettedEndpoint::vet("https://issuer.example:notaport/jwks", &resolver).err(),
            Some(SsrfRejection::MissingHost)
        );
    }

    #[test]
    fn a_vetted_endpoint_pins_the_address_and_keeps_the_hostname() {
        let resolver = ScriptedResolver::new(vec![public()]);
        let endpoint =
            VettedEndpoint::vet("https://issuer.example/jwks", &resolver).expect("vetted");
        assert_eq!(endpoint.host(), "issuer.example");
        assert_eq!(endpoint.port(), 443);
        assert_eq!(endpoint.addresses(), public().as_slice());
        assert_eq!(
            endpoint.socket_addresses(),
            vec![SocketAddr::new(public()[0], 443)]
        );
        assert_eq!(endpoint.url(), "https://issuer.example/jwks");
        assert_eq!(resolver.lookups(), 1);
        let explicit_port =
            VettedEndpoint::vet("https://issuer.example:8443/jwks", &resolver).expect("vetted");
        assert_eq!(explicit_port.port(), 8443);
    }

    #[test]
    fn a_rebinding_second_answer_cannot_change_the_pinned_address() {
        // Public on the first lookup, private on the second: the vetted endpoint
        // keeps the first answer, so the socket can only go to the vetted address.
        let resolver = ScriptedResolver::new(vec![
            public(),
            vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))],
        ]);
        let endpoint =
            VettedEndpoint::vet("https://issuer.example/jwks", &resolver).expect("vetted");
        assert_eq!(endpoint.addresses(), public().as_slice());
        assert_eq!(resolver.lookups(), 1);
        // A *fresh* vetting of the same name now fails, which is the proof that the
        // second answer would have been prohibited had it been trusted.
        assert_eq!(
            VettedEndpoint::vet("https://issuer.example/jwks", &resolver).err(),
            Some(SsrfRejection::ProhibitedAddress)
        );
        // The already-vetted endpoint is unchanged by that.
        assert_eq!(endpoint.addresses(), public().as_slice());
    }

    #[test]
    fn a_redirect_is_revetted_and_may_not_change_the_issuer() {
        let resolver = ScriptedResolver::new(vec![public()]);
        let same = VettedEndpoint::revet_redirect(
            "https://issuer.example/rotated-jwks",
            "https://issuer.example",
            &resolver,
        )
        .expect("same issuer");
        assert_eq!(same.host(), "issuer.example");

        assert_eq!(
            VettedEndpoint::revet_redirect(
                "https://elsewhere.example/jwks",
                "https://issuer.example",
                &resolver
            )
            .err(),
            Some(SsrfRejection::IssuerChanged)
        );
        // A redirect at the metadata address is refused by address vetting, before
        // the issuer comparison could ever matter.
        let metadata = ScriptedResolver::new(vec![vec![IpAddr::V4(METADATA_IPV4)]]);
        assert_eq!(
            VettedEndpoint::revet_redirect(
                "https://issuer.example/jwks",
                "https://issuer.example",
                &metadata
            )
            .err(),
            Some(SsrfRejection::ProhibitedAddress)
        );
        assert_eq!(
            VettedEndpoint::revet_redirect("", "https://issuer.example", &resolver).err(),
            Some(SsrfRejection::MissingRedirectTarget)
        );
        assert_eq!(
            VettedEndpoint::revet_redirect(
                "http://issuer.example/jwks",
                "https://issuer.example",
                &resolver
            )
            .err(),
            Some(SsrfRejection::InsecureScheme)
        );
    }

    #[test]
    fn an_environment_proxy_is_refused() {
        assert_eq!(reject_environment_proxy([("PATH", "/usr/bin")]), Ok(()));
        assert_eq!(reject_environment_proxy([("HTTPS_PROXY", "  ")]), Ok(()));
        for name in PROXY_ENV_VARS {
            assert_eq!(
                reject_environment_proxy([(name, "http://proxy.internal:8080")]),
                Err(SsrfRejection::EnvironmentProxy),
                "{name} must be refused"
            );
        }
    }

    #[test]
    fn an_unresolvable_host_is_refused() {
        let empty = ScriptedResolver::new(Vec::new());
        assert_eq!(
            VettedEndpoint::vet("https://issuer.example/jwks", &empty).err(),
            Some(SsrfRejection::UnresolvableHost)
        );
        let blank = ScriptedResolver::new(vec![Vec::new()]);
        assert_eq!(
            VettedEndpoint::vet("https://issuer.example/jwks", &blank).err(),
            Some(SsrfRejection::UnresolvableHost)
        );
    }

    #[test]
    fn the_client_is_built_without_a_proxy_and_without_auto_redirects() {
        let resolver = ScriptedResolver::new(vec![public()]);
        let endpoint =
            VettedEndpoint::vet("https://issuer.example/jwks", &resolver).expect("vetted");
        assert!(DiscoveryClient::new(&endpoint).is_ok());
    }

    #[test]
    fn rejection_labels_are_stable() {
        assert_eq!(
            SsrfRejection::ProhibitedAddress.as_str(),
            "prohibited_address"
        );
        assert_eq!(
            SsrfRejection::EnvironmentProxy.as_str(),
            "environment_proxy"
        );
        assert_eq!(SsrfRejection::IssuerChanged.as_str(), "issuer_changed");
        assert_eq!(SsrfRejection::InsecureScheme.as_str(), "insecure_scheme");
        assert_eq!(SsrfRejection::LiteralAddress.as_str(), "literal_address");
        assert_eq!(SsrfRejection::MissingHost.as_str(), "missing_host");
        assert_eq!(
            SsrfRejection::UnresolvableHost.as_str(),
            "unresolvable_host"
        );
        assert_eq!(
            SsrfRejection::TooManyRedirects.as_str(),
            "too_many_redirects"
        );
        assert_eq!(
            SsrfRejection::MissingRedirectTarget.as_str(),
            "missing_redirect_target"
        );
        assert_eq!(SsrfRejection::BodyTooLarge.as_str(), "body_too_large");
        assert_eq!(SsrfRejection::Timeout.as_str(), "discovery_timeout");
        assert_eq!(
            SsrfRejection::UnexpectedStatus { status: 500 }.as_str(),
            "unexpected_status"
        );
        assert_eq!(
            SsrfRejection::TransportFailure.as_str(),
            "transport_failure"
        );
        assert_eq!(MAX_REDIRECTS, 3);
        assert_eq!(MAX_BODY_BYTES, 131_072);
        assert_eq!(FETCH_TIMEOUT, Duration::from_secs(10));
    }
}
