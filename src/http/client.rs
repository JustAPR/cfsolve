//! Browser HTTP transport, ordered request headers, and stream weights.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
#[cfg(feature = "internals")]
use bytes::Bytes;
use serde::Deserialize;
use url::Url;
use wreq::cookie::{Cookie, Jar};
use wreq::header::HeaderMap;
use wreq::http2::{StreamDependency, StreamId};
use wreq::redirect::Policy;
use wreq::tls::trust::CertStore;
use wreq::ws::{WebSocket, WebSocketRequestBuilder};
use wreq::{Client, Proxy, RequestBuilder, Response};
#[cfg(feature = "internals")]
use wreq::{Method, StatusCode};
use wreq_util::{Emulation, Platform, Profile};

#[cfg(feature = "internals")]
use super::decode;
use super::header_order::{HeaderOrder, RequestKind, Slot};
use crate::engine::Engine;

/// HTTP client configured from the browser fingerprint. Shared by all session requests to
/// preserve cookies and transport settings.
pub struct BrowserClient {
    pub(super) http: Client,
    /// The cookies the browser holds, `cf_clearance` among them once a challenge page clears.
    pub(super) jar: Arc<Jar>,
    /// User agent and client hints from the profile, overridden by `transport.headers`.
    pub(super) own: HeaderMap,
    pub(super) requests: Requests,
    pub(super) engine: Engine,
}

/// Browser request settings beyond the transport profile, which vary by browser version.
#[derive(Clone, Debug, Deserialize)]
pub struct Requests {
    /// HTTP/2 stream weights, from 1 to 256, indexed by RFC 9218 urgency. Missing `priority`
    /// implies urgency 3. Unspecified urgencies, or an absent map, use the transport profile's
    /// weight.
    #[serde(default)]
    pub weights: BTreeMap<u8, u16>,
    /// The headers each kind of request carries, in the browser's order.
    pub headers: HeaderOrder,
}

impl Requests {
    /// Reads request settings from tracked profile text.
    #[cfg(test)]
    pub(crate) fn from_profile(profile: &str) -> Self {
        let profile: serde_json::Value =
            serde_json::from_str(profile).expect("a tracked profile is JSON");
        let requests = profile
            .pointer("/transport/requests")
            .expect("a tracked profile describes its requests");
        Self::deserialize(requests).expect("a tracked profile's requests are valid")
    }
}

impl BrowserClient {
    /// Creates a client from a wreq-util transport profile. `headers` replace the profile's
    /// user agent and client hints, `hello` adds ClientHello fields for newer browsers, and
    /// `proxy` is an `http://`, `https://`, `socks5://`, or `socks5h://` URL. `trusted_root` is
    /// PEM certificate text to trust instead of the built-in roots.
    #[expect(clippy::too_many_arguments)]
    pub fn try_new(
        profile: Profile,
        platform: Platform,
        headers: HeaderMap,
        hello: &ClientHello,
        requests: Requests,
        engine: Engine,
        proxy: Option<&str>,
        trusted_root: Option<&[u8]>,
    ) -> Result<Self> {
        use wreq::IntoEmulation;

        let mut emulation = Emulation::builder()
            .profile(profile)
            .platform(platform)
            .build()
            .into_emulation();
        if let Some(tls) = &mut emulation.tls_options {
            if !hello.advertised_sigalgs_prefix.is_empty() {
                tls.advertised_sigalgs_prefix =
                    Some(hello.advertised_sigalgs_prefix.clone().into());
            }
            tls.grease_sigalgs = hello.grease_sigalgs;
            if let Some(ids) = &hello.requested_trust_anchors {
                let ids = decode_hex(ids).context("trust anchor IDs must be hex")?;
                tls.requested_trust_anchors = Some(ids.into());
            }
        }
        for weight in requests.weights.values() {
            ensure!(
                (1..=256).contains(weight),
                "HTTP/2 weight {weight} is not from 1 to 256"
            );
        }
        let mut own = emulation.headers.clone();
        for (name, value) in &headers {
            own.insert(name, value.clone());
        }
        requests.headers.check_own(&own)?;
        let jar = Arc::new(Jar::default());
        let mut builder = Client::builder()
            .emulation(emulation)
            .default_headers(headers)
            .cookie_provider(Arc::clone(&jar))
            .redirect(Policy::limited(5))
            .timeout(Duration::from_secs(30));
        if let Some(proxy) = proxy {
            let normalized = normalize_proxy(proxy);
            if !normalized.is_empty() {
                builder = builder.proxy(Proxy::all(&normalized)?);
            }
        }
        if let Some(pem) = trusted_root {
            let store = CertStore::from_pem_certs([pem]).context("reading the trusted root")?;
            builder = builder.tls_cert_store(store);
        }
        Ok(Self {
            http: builder.build()?,
            jar,
            own,
            requests,
            engine,
        })
    }

    #[must_use]
    pub fn engine(&self) -> Engine {
        self.engine
    }

    /// The cookies the browser would send to `url`.
    #[must_use]
    pub fn cookies(&self, url: &Url) -> Vec<Cookie<'static>> {
        self.jar.matches(url.as_str()).collect()
    }

    /// Stores the cookie a `Set-Cookie` header from `url` sets.
    pub fn add_cookie(&self, set_cookie: &str, url: &Url) {
        self.jar.add(set_cookie, url.as_str());
    }

    /// Creates a test browser using HTTP and the tracked iOS Safari 26 request profile.
    #[cfg(test)]
    pub(super) fn from_http(http: Client) -> Self {
        Self {
            http,
            jar: Arc::default(),
            own: HeaderMap::new(),
            requests: Requests::from_profile(include_str!("../../profiles/safari-26-ios.json")),
            engine: Engine::WebKit,
        }
    }

    /// Checks whether the profile defines a header order for `kind`. Required request kinds
    /// must have one.
    #[must_use]
    pub(super) fn orders(&self, kind: RequestKind) -> bool {
        self.requests.headers.get(kind).is_some()
    }

    /// Sends a request with the profile's header order and stream weight for `kind`, replacing
    /// client defaults. `supplied` provides header values; `None` omits a header.
    ///
    /// # Panics
    ///
    /// Panics if the profile has no header order for `kind`. Only optional kinds may be absent.
    pub(super) async fn send(
        &self,
        request: RequestBuilder,
        kind: RequestKind,
        supplied: &[(&str, Option<&str>)],
    ) -> Result<Response, wreq::Error> {
        let (http, request) = self.ordered(request, kind, supplied).build_split();
        let mut request = request?;
        let urgency = request
            .headers()
            .get("priority")
            .and_then(|priority| priority.to_str().ok())
            .map_or(3, urgency);
        if let Some(&weight) = self.requests.weights.get(&urgency) {
            // The wire weight is one less than the configured weight. The stream exclusively
            // depends on the connection while no other stream is open.
            let dependency = StreamDependency::new(StreamId::zero(), (weight - 1) as u8, true);
            request.extensions_mut().insert(dependency);
        }
        RequestBuilder::from_parts(http, request).send().await
    }

    /// Opens a WebSocket to `url` using the profile's handshake headers. The client appends the
    /// handshake key.
    ///
    /// # Panics
    ///
    /// Panics if the profile has no `websocket` header order.
    pub(super) async fn websocket(
        &self,
        url: &Url,
        supplied: &[(&str, Option<&str>)],
    ) -> Result<WebSocket, wreq::Error> {
        let request = self.ordered(
            self.http.get(url.as_str()),
            RequestKind::WebSocket,
            supplied,
        );
        WebSocketRequestBuilder::new(request)
            .send()
            .await?
            .into_websocket()
            .await
    }

    /// Replaces default headers with the profile's order for `kind`. Takes values from `supplied`,
    /// omitting headers whose value is `None`.
    fn ordered(
        &self,
        request: RequestBuilder,
        kind: RequestKind,
        supplied: &[(&str, Option<&str>)],
    ) -> RequestBuilder {
        debug_assert!(
            supplied.len() == kind.supplied().len()
                && supplied
                    .iter()
                    .all(|(name, _)| kind.supplied().contains(name)),
            "a `{kind}` request supplies {:?}, not {supplied:?}",
            kind.supplied()
        );
        let order = self
            .requests
            .headers
            .get(kind)
            .unwrap_or_else(|| panic!("the profile orders no `{kind}` requests"));
        let mut request = request.default_headers(false);
        for slot in order {
            request = match slot {
                Slot::Fixed(name, value) => request.header(name, value),
                // Request-supplied values override browser defaults, including an explicit
                // omission.
                Slot::Named(name) => match supplied.iter().find(|(given, _)| name == given) {
                    Some((_, Some(value))) => request.header(name, *value),
                    Some((_, None)) => request,
                    None => match self.own.get(name) {
                        Some(value) => request.header(name, value),
                        None => request,
                    },
                },
            };
        }
        request
    }

    /// Forwards a capture-proxy request without following redirects or decoding the response.
    /// Keeps the response's wire bytes. Uses an empty cookie jar so earlier sessions cannot add
    /// cookies.
    ///
    /// Sends only the browser's headers. Client defaults include high-entropy client hints that
    /// Cloudflare incorporates into challenge options and tokens.
    #[cfg(feature = "internals")]
    pub(crate) async fn proxy_fetch(
        &self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<Proxied> {
        let response = decode::preserve_encoding(self.http.request(method, url))
            .default_headers(false)
            .redirect(Policy::none())
            .cookie_provider(Jar::default())
            .headers(headers)
            .body(body)
            .send()
            .await?;
        Ok(Proxied {
            status: response.status(),
            headers: response.headers().clone(),
            body: response.bytes().await?,
        })
    }
}

/// A response fetched for the capture proxy, with its wire bytes intact.
#[cfg(feature = "internals")]
pub(crate) struct Proxied {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Reads RFC 9218 urgency from `priority`, defaulting to 3.
fn urgency(priority: &str) -> u8 {
    priority
        .split(',')
        .find_map(|item| item.trim().strip_prefix("u=")?.parse().ok())
        .unwrap_or(3)
}

/// Additional ClientHello fields from the fingerprint's browser, beyond the installed wreq-util
/// profiles.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ClientHello {
    /// Signature algorithm code points prepended to the profile's list, such as Chrome's ML-DSA
    /// values. They are advertised but never accepted.
    pub advertised_sigalgs_prefix: Vec<u16>,
    /// Whether `signature_algorithms` starts with a GREASE value.
    pub grease_sigalgs: bool,
    /// Hex-encoded, length-prefixed list of IDs for the `trust_anchors` extension.
    pub requested_trust_anchors: Option<String>,
}

/// Bytes written as pairs of hexadecimal digits.
fn decode_hex(text: &str) -> Result<Vec<u8>> {
    ensure!(
        text.len().is_multiple_of(2) && text.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{text:?} is not pairs of hex digits"
    );
    let byte = |start: usize| u8::from_str_radix(&text[start..start + 2], 16).expect("hex digits");
    Ok((0..text.len()).step_by(2).map(byte).collect())
}

/// Normalizes arbitrary proxy input strings into standard URI format.
///
/// Handles common proxy formats:
/// - `(Socks5)host:port` -> `socks5://host:port`
/// - `(Socks5)host:port:user:pass` -> `socks5://user:pass@host:port`
/// - `host:port:user:pass` -> `http://user:pass@host:port` (defaults to http)
/// - `user:pass:host:port` -> `http://user:pass@host:port`
/// - `host:port` -> `http://host:port`
/// - `user:pass@host:port` -> `http://user:pass@host:port`
/// - Comma/newline separated proxy lists (takes the first proxy entry)
pub fn normalize_proxy(raw: &str) -> String {
    let mut s = raw.trim();
    if s.is_empty() {
        return String::new();
    }

    // If multiple proxies are separated by comma, semicolon or newline, take the first one
    if let Some(first) = s.split([',', ';', '\n', '\r']).map(str::trim).find(|p| !p.is_empty()) {
        s = first;
    }

    let mut scheme = "http";

    // Check for leading tag like (socks5), [socks5], (http), [https], etc.
    if s.starts_with('(') || s.starts_with('[') {
        let close_char = if s.starts_with('(') { ')' } else { ']' };
        if let Some(idx) = s.find(close_char) {
            let tag = s[1..idx].trim().to_ascii_lowercase();
            match tag.as_str() {
                "socks5" | "socks" => scheme = "socks5",
                "socks5h" => scheme = "socks5h",
                "https" => scheme = "https",
                "http" => scheme = "http",
                _ => {}
            }
            s = s[idx + 1..].trim();
        }
    }

    // Check if s already has a scheme://
    if let Some(idx) = s.find("://") {
        let existing_scheme = &s[..idx];
        let rest = &s[idx + 3..];
        return format!("{}://{}", existing_scheme.to_ascii_lowercase(), rest.trim());
    }

    // Check if s starts with scheme: without slashes, e.g. "socks5:1.2.3.4:1080"
    for prefix in &["socks5h:", "socks5:", "https:", "http:", "socks:"] {
        if s.to_ascii_lowercase().starts_with(prefix) {
            let tag = prefix[..prefix.len() - 1].to_ascii_lowercase();
            match tag.as_str() {
                "socks5" | "socks" => scheme = "socks5",
                "socks5h" => scheme = "socks5h",
                "https" => scheme = "https",
                "http" => scheme = "http",
                _ => {}
            }
            s = s[prefix.len()..].trim();
            break;
        }
    }

    // If it already has an '@', e.g. user:pass@host:port
    if s.contains('@') {
        return format!("{scheme}://{s}");
    }

    // Split by ':'
    let parts: Vec<&str> = s.split(':').map(str::trim).collect();
    match parts.as_slice() {
        // 4 parts: host:port:user:pass OR user:pass:host:port
        [p0, p1, p2, p3] => {
            let p1_is_port = p1.parse::<u16>().is_ok();
            let p3_is_port = p3.parse::<u16>().is_ok();

            if p1_is_port && !p3_is_port {
                // host:port:user:pass
                let host = p0;
                let port = p1;
                let user = p2;
                let pass = p3;
                format!("{scheme}://{user}:{pass}@{host}:{port}")
            } else if p3_is_port && !p1_is_port {
                // user:pass:host:port
                let user = p0;
                let pass = p1;
                let host = p2;
                let port = p3;
                format!("{scheme}://{user}:{pass}@{host}:{port}")
            } else {
                // Default fallback: host:port:user:pass
                format!("{scheme}://{p2}:{p3}@{p0}:{p1}")
            }
        }
        // 2 parts: host:port
        [host, port] => {
            format!("{scheme}://{host}:{port}")
        }
        _ => {
            format!("{scheme}://{s}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urgency_reads_the_priority_dictionary_and_defaults_to_three() {
        assert_eq!(urgency("u=0, i"), 0);
        assert_eq!(urgency("i, u=4"), 4);
        assert_eq!(urgency("u=1"), 1);
        assert_eq!(urgency("i"), 3);
        assert_eq!(urgency(""), 3);
    }

    #[test]
    fn normalize_proxy_handles_custom_and_standard_formats() {
        // (Socks5)host:port
        assert_eq!(
            normalize_proxy("(Socks5)47.91.110.148:8443"),
            "socks5://47.91.110.148:8443"
        );

        // (Socks5)host:port:user:pass
        assert_eq!(
            normalize_proxy("(Socks5)ams.socks.ipvanish.com:1080:caEVXGbA:QJACVaY5k"),
            "socks5://caEVXGbA:QJACVaY5k@ams.socks.ipvanish.com:1080"
        );

        // host:port:user:pass with default http
        assert_eq!(
            normalize_proxy("107.172.241.122:12345:tickets:proxyon145"),
            "http://tickets:proxyon145@107.172.241.122:12345"
        );

        // user:pass:host:port with default http
        assert_eq!(
            normalize_proxy("tickets:proxyon145:107.172.241.122:12345"),
            "http://tickets:proxyon145@107.172.241.122:12345"
        );

        // host:port
        assert_eq!(normalize_proxy("1.2.3.4:8080"), "http://1.2.3.4:8080");

        // user:pass@host:port
        assert_eq!(
            normalize_proxy("admin:pass@1.2.3.4:8080"),
            "http://admin:pass@1.2.3.4:8080"
        );

        // Already standard URL
        assert_eq!(
            normalize_proxy("socks5://user:pass@1.2.3.4:1080"),
            "socks5://user:pass@1.2.3.4:1080"
        );
        assert_eq!(
            normalize_proxy("http://1.2.3.4:8080"),
            "http://1.2.3.4:8080"
        );

        // Comma-separated list takes first
        assert_eq!(
            normalize_proxy("(Socks5)47.91.110.148:8443,107.172.241.122:12345:tickets:proxyon145"),
            "socks5://47.91.110.148:8443"
        );
    }
}
