//! The small extractors: request id, client address, extensions, and the parts
//! of the head that need no interpretation.
//!
//! None of these contributes to the OpenAPI document. That is correct rather
//! than lazy: "this handler looked at the request method" is not a fact a client
//! can act on, and putting it in the document would only make the document
//! longer. The one thing a client *could* act on — the correlation id — is
//! documented once, as a response header on every operation, by the middleware
//! that sets it rather than by [`RequestId`] here.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use moso_openapi::OperationBuilder;
use ulid::Ulid;

use crate::ctx::RequestCtx;
use crate::error::{Error, Result};
use crate::extract::Extract;

/// The request's correlation id.
///
/// Read from the `x-request-id` header when the client sent one, generated as a
/// ULID otherwise. Present on every request, echoed on every response, and
/// included in every error document and every log line — it is what makes
/// "the API returned a 500 at 14:32" into a searchable question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub Ulid);

impl RequestId {
    /// The id.
    pub fn into_inner(self) -> Ulid {
        self.0
    }
}

impl core::fmt::Display for RequestId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Extract for RequestId {
    fn describe(op: &mut OperationBuilder) {
        let _ = op;
    }

    async fn extract(parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        let _ = parts;
        Ok(RequestId(*ctx.request_id()))
    }
}

/// The client's IP address, honouring the configured proxy policy.
///
/// **Never** reads `X-Forwarded-For` unless `http.trusted_proxies` says the
/// peer is a trusted proxy. An IP taken from an untrusted header is a
/// client-controlled string, and rate limiters and audit logs built on one are
/// worse than useless — they are confidently wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientIp(pub IpAddr);

impl ClientIp {
    /// The address.
    pub fn into_inner(self) -> IpAddr {
        self.0
    }
}

impl Extract for ClientIp {
    fn describe(op: &mut OperationBuilder) {
        let _ = op;
    }

    async fn extract(parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        let peer = peer_address(parts).ok_or_else(|| {
            Error::internal_msg(
                "no peer address on this request: `ClientIp` needs the server to have been \
                 started with connection info, which `App::serve` does — a hand-rolled \
                 `axum::serve` must use `into_make_service_with_connect_info::<SocketAddr>()`",
            )
        })?;
        let trusted = &ctx.state().http().trusted_proxies;
        Ok(ClientIp(resolve_client_ip(
            peer.ip(),
            &parts.headers,
            trusted,
        )))
    }
}

/// The address the connection reports, if the server recorded one.
fn peer_address(parts: &http::request::Parts) -> Option<SocketAddr> {
    parts
        .extensions
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|info| info.0)
}

/// The header a proxy chain appends to.
const X_FORWARDED_FOR: http::HeaderName = http::HeaderName::from_static("x-forwarded-for");

/// Walk `X-Forwarded-For` right to left, stopping at the first address that is
/// not itself a trusted proxy.
///
/// Right to left is the only defensible direction. The rightmost entry was
/// appended by the proxy nearest us and is therefore the most trustworthy; the
/// leftmost is whatever the client claimed and is worth nothing. Walking left
/// to right — which several popular middlewares do — hands an attacker their
/// choice of IP by sending `X-Forwarded-For: 1.2.3.4`.
///
/// With no trusted proxies configured, the header is not consulted at all.
fn resolve_client_ip(peer: IpAddr, headers: &http::HeaderMap, trusted: &[String]) -> IpAddr {
    if trusted.is_empty() || !is_trusted(peer, trusted) {
        return peer;
    }
    let forwarded: Vec<IpAddr> = headers
        .get_all(X_FORWARDED_FOR)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|entry| parse_forwarded_entry(entry.trim()))
        .collect();
    for candidate in forwarded.iter().rev() {
        if !is_trusted(*candidate, trusted) {
            return *candidate;
        }
    }
    // Every hop in the chain is a trusted proxy, so the leftmost entry is the
    // furthest one we have evidence for.
    forwarded.first().copied().unwrap_or(peer)
}

/// Parse one `X-Forwarded-For` entry, which may carry a port or brackets.
fn parse_forwarded_entry(entry: &str) -> Option<IpAddr> {
    if let Ok(address) = entry.parse::<IpAddr>() {
        return Some(address);
    }
    if let Ok(socket) = entry.parse::<SocketAddr>() {
        return Some(socket.ip());
    }
    // `192.0.2.1:8080` without brackets, and `[2001:db8::1]` without a port.
    if let Some(stripped) = entry.strip_prefix('[')
        && let Some(inner) = stripped.split(']').next()
    {
        return inner.parse::<IpAddr>().ok();
    }
    entry.rsplit_once(':')?.0.parse::<IpAddr>().ok()
}

/// Whether `address` falls inside any configured CIDR range.
///
/// A bare address with no `/prefix` is treated as a single host, which is the
/// spelling everyone reaches for first.
fn is_trusted(address: IpAddr, trusted: &[String]) -> bool {
    trusted.iter().any(|entry| cidr_contains(entry, address))
}

/// Whether `cidr` — `10.0.0.0/8`, `2001:db8::/32`, or a bare address — contains
/// `address`.
///
/// An unparseable entry matches nothing. Refusing to match is the safe failure
/// here: a typo in `http.trusted_proxies` must not accidentally trust the
/// internet, and the boot report is where a bad entry gets named.
pub(crate) fn cidr_contains(cidr: &str, address: IpAddr) -> bool {
    let (network, prefix) = match cidr.split_once('/') {
        Some((network, prefix)) => match prefix.trim().parse::<u8>() {
            Ok(prefix) => (network.trim(), Some(prefix)),
            Err(_) => return false,
        },
        None => (cidr.trim(), None),
    };
    match (network.parse::<IpAddr>(), address) {
        (Ok(IpAddr::V4(network)), IpAddr::V4(address)) => {
            let prefix = prefix.unwrap_or(32);
            prefix <= 32 && masked_v4(network, prefix) == masked_v4(address, prefix)
        }
        (Ok(IpAddr::V6(network)), IpAddr::V6(address)) => {
            let prefix = prefix.unwrap_or(128);
            prefix <= 128 && masked_v6(network, prefix) == masked_v6(address, prefix)
        }
        _ => false,
    }
}

fn masked_v4(address: Ipv4Addr, prefix: u8) -> u32 {
    let bits = u32::from(address);
    if prefix == 0 {
        0
    } else {
        bits & (u32::MAX << (32 - u32::from(prefix)))
    }
}

fn masked_v6(address: Ipv6Addr, prefix: u8) -> u128 {
    let bits = u128::from(address);
    if prefix == 0 {
        0
    } else {
        bits & (u128::MAX << (128 - u32::from(prefix)))
    }
}

/// The peer socket address, as the connection reports it.
///
/// Requires the server to have been started with connection info, which
/// `App::serve` does. Prefer [`ClientIp`], which knows about proxies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectInfo<T = SocketAddr>(pub T);

impl<T> ConnectInfo<T> {
    /// The connection information.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: Clone + Send + Sync + 'static> Extract for ConnectInfo<T> {
    fn describe(op: &mut OperationBuilder) {
        let _ = op;
    }

    async fn extract(parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        let _ = ctx;
        parts
            .extensions
            .get::<axum::extract::ConnectInfo<T>>()
            .map(|info| ConnectInfo(info.0.clone()))
            .ok_or_else(|| {
                Error::internal_msg(format!(
                    "no `ConnectInfo<{}>` on this request: the server was started without \
                     connection info. `App::serve` installs it; a hand-rolled `axum::serve` \
                     needs `into_make_service_with_connect_info::<{0}>()`",
                    core::any::type_name::<T>()
                ))
            })
    }
}

/// A value a middleware inserted into the request extensions.
///
/// The one supported channel from middleware to handler: middleware runs before
/// extraction, so it cannot use [`Depends`](crate::Depends), and a typed
/// extension is how a `TenantLayer` hands a `Tenant` down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Extension<T>(pub T);

impl<T> Extension<T> {
    /// The value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: Clone + Send + Sync + 'static> Extract for Extension<T> {
    fn describe(op: &mut OperationBuilder) {
        let _ = op;
    }

    async fn extract(parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        if let Some(value) = parts.extensions.get::<T>() {
            return Ok(Extension(value.clone()));
        }
        // Middleware runs before extraction, so the context's snapshot of the
        // extensions is the same set — but a layer added *inside* the handler
        // adapter only reaches `parts`, and a dependency only reaches the
        // context. Checking both means neither ordering surprises anyone.
        if let Some(value) = ctx.extension::<T>() {
            return Ok(Extension(value));
        }
        Err(Error::internal_msg(format!(
            "no `{}` in the request extensions: the layer that inserts it is not mounted on \
             this route. Add it with `Router::layer(..)`, or take `Option<Extension<{0}>>` if \
             its absence is expected",
            core::any::type_name::<T>()
        )))
    }
}

/// The matched route pattern — `/users/{id}`, not `/users/42`.
///
/// What a metrics label must use. A label built from the raw path is the
/// classic cardinality explosion: one time series per user id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MatchedPath(pub std::sync::Arc<str>);

impl MatchedPath {
    /// The pattern.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Extract for MatchedPath {
    fn describe(op: &mut OperationBuilder) {
        let _ = op;
    }

    async fn extract(parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        if let Some(matched) = parts.extensions.get::<axum::extract::MatchedPath>() {
            return Ok(MatchedPath(matched.as_str().into()));
        }
        if let Some(matched) = ctx.matched_path() {
            return Ok(MatchedPath(matched.into()));
        }
        Err(Error::internal_msg(
            "this request matched no route pattern, so there is no matched path to read. A \
             fallback handler has none by definition; use `Uri` there instead",
        ))
    }
}

macro_rules! impl_extract_for_head_part {
    ($ty:ty, $field:ident) => {
        impl Extract for $ty {
            fn describe(op: &mut OperationBuilder) {
                let _ = op;
            }

            fn extract<'a>(
                parts: &'a mut http::request::Parts,
                ctx: &'a RequestCtx,
            ) -> impl Future<Output = Result<Self>> + Send + 'a {
                async move {
                    let _ = ctx;
                    Ok(parts.$field.clone())
                }
            }
        }
    };
}

impl_extract_for_head_part!(http::Method, method);
impl_extract_for_head_part!(http::Uri, uri);
impl_extract_for_head_part!(http::Version, version);

/// The [`RequestCtx`] itself, for a handler that needs the whole context.
///
/// Rare and deliberately unergonomic to reach for: needing the context usually
/// means the logic belongs in a [`Dependency`](crate::Dependency), where it can
/// be memoised, documented and tested on its own.
impl Extract for RequestCtx {
    fn describe(op: &mut OperationBuilder) {
        let _ = op;
    }

    async fn extract(parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        let _ = parts;
        Ok(ctx.clone())
    }
}

/// Nothing, extracted successfully.
///
/// Lets a handler declare a [`Dependency`](crate::Dependency) purely for its
/// side effect and its documentation — `_: Depends<RateLimited<5>>` — without
/// inventing a placeholder type.
impl Extract for () {
    fn describe(op: &mut OperationBuilder) {
        let _ = op;
    }

    async fn extract(parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        let _ = (parts, ctx);
        Ok(())
    }
}

/// An optional extractor: `None` instead of an error.
///
/// `Option<T>` succeeds with `None` when `T` would have failed, which is the
/// right shape for "read the bearer token if there is one". It swallows the
/// error deliberately, so use it only where *absence* and *malformed* mean the
/// same thing to the handler.
impl<T: Extract + 'static> Extract for Option<T> {
    fn describe(op: &mut OperationBuilder) {
        <T as Extract>::describe(op);
    }

    const PROVIDER_REQ: &'static [crate::di::ProviderReq] = <T as Extract>::PROVIDER_REQ;

    async fn extract(parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        Ok(<T as Extract>::extract(parts, ctx).await.ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forwarded(values: &[&str]) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        for value in values {
            headers.append(X_FORWARDED_FOR, http::HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    fn ip(value: &str) -> IpAddr {
        value.parse().expect("a literal address")
    }

    #[test]
    fn request_id_displays_as_its_ulid() {
        let id = Ulid::nil();
        assert_eq!(RequestId(id).to_string(), id.to_string());
    }

    #[test]
    fn with_no_trusted_proxies_the_header_is_ignored() {
        let headers = forwarded(&["1.2.3.4"]);
        assert_eq!(
            resolve_client_ip(ip("10.0.0.9"), &headers, &[]),
            ip("10.0.0.9")
        );
    }

    #[test]
    fn an_untrusted_peer_cannot_forge_its_address() {
        let headers = forwarded(&["1.2.3.4"]);
        let trusted = ["10.0.0.0/8".to_owned()];
        assert_eq!(
            resolve_client_ip(ip("203.0.113.7"), &headers, &trusted),
            ip("203.0.113.7")
        );
    }

    #[test]
    fn a_trusted_peer_yields_the_rightmost_untrusted_hop() {
        let headers = forwarded(&["1.2.3.4, 198.51.100.9, 10.0.0.5"]);
        let trusted = ["10.0.0.0/8".to_owned()];
        assert_eq!(
            resolve_client_ip(ip("10.0.0.9"), &headers, &trusted),
            ip("198.51.100.9")
        );
    }

    #[test]
    fn a_spoofed_leftmost_entry_is_not_believed() {
        // The client sent `X-Forwarded-For: 1.2.3.4` hoping to be seen as
        // 1.2.3.4; the proxy appended the address it actually saw.
        let headers = forwarded(&["1.2.3.4, 203.0.113.7"]);
        let trusted = ["10.0.0.0/8".to_owned()];
        assert_eq!(
            resolve_client_ip(ip("10.0.0.9"), &headers, &trusted),
            ip("203.0.113.7")
        );
    }

    #[test]
    fn repeated_headers_are_concatenated_in_order() {
        let headers = forwarded(&["1.2.3.4", "203.0.113.7", "10.0.0.5"]);
        let trusted = ["10.0.0.0/8".to_owned()];
        assert_eq!(
            resolve_client_ip(ip("10.0.0.9"), &headers, &trusted),
            ip("203.0.113.7")
        );
    }

    #[test]
    fn an_all_trusted_chain_falls_back_to_its_leftmost_entry() {
        let headers = forwarded(&["10.0.0.1, 10.0.0.2"]);
        let trusted = ["10.0.0.0/8".to_owned()];
        assert_eq!(
            resolve_client_ip(ip("10.0.0.9"), &headers, &trusted),
            ip("10.0.0.1")
        );
    }

    #[test]
    fn an_empty_chain_falls_back_to_the_peer() {
        let trusted = ["10.0.0.0/8".to_owned()];
        assert_eq!(
            resolve_client_ip(ip("10.0.0.9"), &http::HeaderMap::new(), &trusted),
            ip("10.0.0.9")
        );
    }

    #[test]
    fn forwarded_entries_may_carry_a_port() {
        assert_eq!(parse_forwarded_entry("192.0.2.1"), Some(ip("192.0.2.1")));
        assert_eq!(
            parse_forwarded_entry("192.0.2.1:8080"),
            Some(ip("192.0.2.1"))
        );
        assert_eq!(
            parse_forwarded_entry("2001:db8::1"),
            Some(ip("2001:db8::1"))
        );
        assert_eq!(
            parse_forwarded_entry("[2001:db8::1]:443"),
            Some(ip("2001:db8::1"))
        );
        assert_eq!(parse_forwarded_entry("unknown"), None);
        assert_eq!(parse_forwarded_entry(""), None);
    }

    #[test]
    fn cidr_ranges_match_on_the_prefix_only() {
        assert!(cidr_contains("10.0.0.0/8", ip("10.255.1.2")));
        assert!(!cidr_contains("10.0.0.0/8", ip("11.0.0.1")));
        assert!(cidr_contains("192.168.1.0/24", ip("192.168.1.99")));
        assert!(!cidr_contains("192.168.1.0/24", ip("192.168.2.1")));
        assert!(cidr_contains("0.0.0.0/0", ip("8.8.8.8")));
        assert!(cidr_contains("2001:db8::/32", ip("2001:db8:1234::1")));
        assert!(!cidr_contains("2001:db8::/32", ip("2001:db9::1")));
    }

    #[test]
    fn a_bare_address_is_a_single_host() {
        assert!(cidr_contains("10.0.0.9", ip("10.0.0.9")));
        assert!(!cidr_contains("10.0.0.9", ip("10.0.0.8")));
    }

    #[test]
    fn families_never_cross_and_bad_entries_match_nothing() {
        assert!(!cidr_contains("10.0.0.0/8", ip("::1")));
        assert!(!cidr_contains("2001:db8::/32", ip("10.0.0.1")));
        assert!(!cidr_contains("not-an-address/8", ip("10.0.0.1")));
        assert!(!cidr_contains("10.0.0.0/nope", ip("10.0.0.1")));
        assert!(!cidr_contains("10.0.0.0/99", ip("10.0.0.1")));
    }
}
