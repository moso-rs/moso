//! `compression` — smaller responses, with the three exceptions that matter.
//!
//! Skipped for:
//!
//! - **already-compressed types** — `image/*`, `video/*`, `application/zip`,
//!   `application/gzip`. Recompressing costs CPU and grows the payload.
//! - **`text/event-stream`** — compression buffers, and a buffered event stream
//!   is a broken event stream.
//! - **small bodies** — below [`CompressionConfig::min_size`] the framing costs
//!   more than the saving.
//!
//! # BREACH
//!
//! Compressing a response that mixes a secret with attacker-controlled input
//! leaks the secret through its length. This layer does not attempt to detect
//! that; the mitigation is not to put a CSRF token in a compressible response
//! body, which is why Moso's CSRF token travels in a header. The documentation
//! says so rather than implying the layer has solved it.
//!
//! The slot requires the `compression` cargo feature, which pulls the codec
//! crates. The configuration type exists unconditionally so an application's
//! `with_middleware` block does not change shape with the feature. Enabling the
//! slot with the feature off is a boot error rather than a silent no-op.
//!
//! The layer is `tower_http::compression::CompressionLayer` with a predicate
//! built from [`CompressionConfig`]: the codecs, the `Accept-Encoding`
//! negotiation and the streaming encoder are exactly the parts nobody should
//! write twice.

use crate::router::Route;

/// A content encoding, in Moso's preference order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Encoding {
    /// Brotli. Best ratio, acceptable speed at low quality levels.
    Brotli,
    /// gzip. Universally supported.
    Gzip,
    /// deflate. Kept for completeness; nothing prefers it.
    Deflate,
}

impl Encoding {
    /// Preference order, best first.
    pub const PREFERENCE: [Encoding; 3] = [Encoding::Brotli, Encoding::Gzip, Encoding::Deflate];

    /// The `Content-Encoding` token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Encoding::Brotli => "br",
            Encoding::Gzip => "gzip",
            Encoding::Deflate => "deflate",
        }
    }

    /// Whether Moso compiles a codec for this encoding.
    ///
    /// `deflate` is listed in [`Encoding::PREFERENCE`] for completeness but is
    /// not compiled in: nothing prefers it over gzip, and the codec is another
    /// dependency for no gain. Offering it in the configuration and not on the
    /// wire would be a lie, so [`CompressionConfig::summary`] prints what is
    /// actually negotiable.
    pub const fn is_available(self) -> bool {
        match self {
            Encoding::Brotli => cfg!(feature = "compression"),
            Encoding::Gzip => cfg!(feature = "compression"),
            Encoding::Deflate => false,
        }
    }
}

/// How the `compression` slot behaves.
#[derive(Debug, Clone)]
pub struct CompressionConfig {
    /// Encodings to offer, in preference order.
    pub encodings: Vec<Encoding>,
    /// The smallest body worth compressing, in bytes.
    pub min_size: usize,
    /// Content types never compressed, as prefixes.
    pub skip_content_types: Vec<String>,
    /// The brotli quality level, 0 to 11.
    ///
    /// 4 by default. Level 11 is roughly ten times slower for a few percent,
    /// which is the wrong trade for a response generated per request.
    pub quality: u32,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            encodings: Encoding::PREFERENCE.to_vec(),
            min_size: 1024,
            skip_content_types: DEFAULT_SKIP_CONTENT_TYPES
                .iter()
                .map(|prefix| (*prefix).to_owned())
                .collect(),
            quality: 4,
        }
    }
}

impl CompressionConfig {
    /// Offer only these encodings.
    pub fn encodings(&mut self, encodings: impl IntoIterator<Item = Encoding>) -> &mut Self {
        self.encodings = encodings.into_iter().collect();
        self
    }

    /// Set the minimum compressible size.
    pub fn min_size(&mut self, bytes: usize) -> &mut Self {
        self.min_size = bytes;
        self
    }

    /// Never compress this content type, matched as a prefix.
    pub fn skip(&mut self, content_type: impl Into<String>) -> &mut Self {
        self.skip_content_types.push(content_type.into());
        self
    }

    /// Whether a content type should be compressed.
    pub fn compresses(&self, content_type: &str) -> bool {
        !self
            .skip_content_types
            .iter()
            .any(|prefix| content_type.starts_with(prefix.as_str()))
    }

    /// The encodings this build can actually negotiate.
    pub fn available(&self) -> Vec<Encoding> {
        self.encodings
            .iter()
            .copied()
            .filter(|encoding| encoding.is_available())
            .collect()
    }

    /// A one-line summary for `moso middleware`.
    pub fn summary(&self) -> String {
        let offered = self.available();
        if offered.is_empty() {
            return "no codecs compiled in".to_owned();
        }
        format!(
            "{} min={}",
            offered
                .iter()
                .map(|encoding| encoding.as_str())
                .collect::<Vec<_>>()
                .join(","),
            self.min_size
        )
    }
}

/// Content types never worth compressing.
pub const DEFAULT_SKIP_CONTENT_TYPES: &[&str] = &[
    "image/",
    "video/",
    "audio/",
    "text/event-stream",
    "application/zip",
    "application/gzip",
    "application/x-brotli",
    "application/pdf",
];

/// Whether a response with these headers should be compressed.
///
/// Shared by the predicate and by the tests, so what the layer does and what
/// the documentation claims are the same function.
pub fn should_compress(config: &CompressionConfig, headers: &http::HeaderMap) -> bool {
    // Already encoded: compressing again grows the payload and breaks the
    // client's decoder chain.
    if headers.contains_key(http::header::CONTENT_ENCODING) {
        return false;
    }

    if let Some(content_type) = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        // `application/json; charset=utf-8` must match the `application/json`
        // prefix rules, so the parameters come off first.
        let media_type = content_type
            .split(';')
            .next()
            .unwrap_or(content_type)
            .trim();
        if !config.compresses(media_type) {
            return false;
        }
    }

    // An unknown length is a streaming body; compressing it is the case the
    // layer exists for, so absence means yes.
    match headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
    {
        Some(length) => length >= config.min_size,
        None => true,
    }
}

/// Wrap `service` in `tower_http`'s compression layer.
#[cfg(feature = "compression")]
pub fn layer(config: &CompressionConfig, service: Route) -> Route {
    use std::sync::Arc;

    use tower::{Layer as _, ServiceExt as _};
    use tower_http::compression::{CompressionLayer, CompressionLevel};

    let config = Arc::new(config.clone());
    let predicate = {
        let config = Arc::clone(&config);
        move |_status: http::StatusCode,
              _version: http::Version,
              headers: &http::HeaderMap,
              _extensions: &http::Extensions| should_compress(&config, headers)
    };

    let layer = CompressionLayer::new()
        .br(config.encodings.contains(&Encoding::Brotli))
        .gzip(config.encodings.contains(&Encoding::Gzip))
        // `Precise` is clamped to each algorithm's maximum, so a configuration
        // of 11 means "best brotli" and still means something sane for gzip.
        .quality(CompressionLevel::Precise(
            i32::try_from(config.quality).unwrap_or(4),
        ))
        .compress_when(predicate);

    // The compressed body is a different type, so it goes back through
    // `IntoResponse` to become an `axum::body::Body` again.
    Route::new(
        layer
            .layer(service)
            .map_response(crate::IntoResponse::into_response),
    )
}

/// Pass `service` through: the `compression` feature is off, so there are no
/// codecs to negotiate with.
///
/// Reaching here means the slot was enabled without the feature, which
/// [`MiddlewareStack::validate`](crate::MiddlewareStack::validate) reports as a
/// boot error. The pass-through exists so that `build_unchecked` still runs.
#[cfg(not(feature = "compression"))]
pub fn layer(config: &CompressionConfig, service: Route) -> Route {
    let _ = config;
    service
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                http::HeaderName::from_static(name),
                HeaderValue::try_from(*value).expect("value"),
            );
        }
        map
    }

    #[test]
    fn brotli_is_preferred() {
        assert_eq!(Encoding::PREFERENCE[0], Encoding::Brotli);
        assert_eq!(Encoding::Brotli.as_str(), "br");
    }

    #[test]
    fn event_streams_and_images_are_skipped() {
        let config = CompressionConfig::default();
        assert!(!config.compresses("text/event-stream"));
        assert!(!config.compresses("image/png"));
        assert!(config.compresses("application/json"));
    }

    #[test]
    fn deflate_is_never_offered() {
        assert!(!Encoding::Deflate.is_available());
        let offered = CompressionConfig::default().available();
        assert!(!offered.contains(&Encoding::Deflate));
    }

    #[test]
    fn the_summary_lists_what_is_actually_negotiable() {
        let summary = CompressionConfig::default().summary();
        if cfg!(feature = "compression") {
            assert_eq!(summary, "br,gzip min=1024");
        } else {
            assert_eq!(summary, "no codecs compiled in");
        }
    }

    #[test]
    fn a_content_type_with_parameters_still_matches_the_prefix() {
        let config = CompressionConfig::default();
        assert!(should_compress(
            &config,
            &headers(&[("content-type", "application/json; charset=utf-8")])
        ));
        assert!(!should_compress(
            &config,
            &headers(&[("content-type", "text/event-stream; charset=utf-8")])
        ));
    }

    #[test]
    fn a_small_body_is_not_worth_the_framing() {
        let config = CompressionConfig::default();
        assert!(!should_compress(
            &config,
            &headers(&[
                ("content-type", "application/json"),
                ("content-length", "10")
            ])
        ));
        assert!(should_compress(
            &config,
            &headers(&[
                ("content-type", "application/json"),
                ("content-length", "4096")
            ])
        ));
    }

    #[test]
    fn an_unknown_length_is_compressed() {
        // A streaming body is exactly the case worth compressing.
        assert!(should_compress(
            &CompressionConfig::default(),
            &headers(&[("content-type", "application/json")])
        ));
    }

    #[test]
    fn an_already_encoded_body_is_left_alone() {
        assert!(!should_compress(
            &CompressionConfig::default(),
            &headers(&[
                ("content-type", "application/json"),
                ("content-encoding", "gzip")
            ])
        ));
    }

    #[cfg(feature = "compression")]
    #[tokio::test]
    async fn a_large_json_body_comes_back_compressed() {
        use crate::{Request, Response};
        use std::convert::Infallible;
        use tower::ServiceExt as _;

        let body = "x".repeat(8192);
        let inner = Route::new(tower::service_fn(move |_req: Request| {
            let body = body.clone();
            async move {
                let mut response = Response::new(axum::body::Body::from(body));
                response.headers_mut().insert(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                Ok::<_, Infallible>(response)
            }
        }));

        let request = http::Request::builder()
            .header(http::header::ACCEPT_ENCODING, "gzip")
            .body(axum::body::Body::empty())
            .expect("request");

        let response = layer(&CompressionConfig::default(), inner)
            .oneshot(request)
            .await
            .expect("infallible");
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("gzip")
        );
    }

    #[cfg(feature = "compression")]
    #[tokio::test]
    async fn an_event_stream_is_never_compressed() {
        use crate::{Request, Response};
        use std::convert::Infallible;
        use tower::ServiceExt as _;

        let inner = Route::new(tower::service_fn(|_req: Request| async {
            let mut response = Response::new(axum::body::Body::from("data: hi\n\n".repeat(1000)));
            response.headers_mut().insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            Ok::<_, Infallible>(response)
        }));

        let request = http::Request::builder()
            .header(http::header::ACCEPT_ENCODING, "gzip, br")
            .body(axum::body::Body::empty())
            .expect("request");

        let response = layer(&CompressionConfig::default(), inner)
            .oneshot(request)
            .await
            .expect("infallible");
        assert!(
            !response
                .headers()
                .contains_key(http::header::CONTENT_ENCODING)
        );
    }
}
