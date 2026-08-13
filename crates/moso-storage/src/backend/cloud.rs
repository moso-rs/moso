//! The three cloud backends, and the request signing behind them.
//!
//! All three speak plain HTTP with a signature, which is why they share a file
//! and a client rather than pulling three vendor SDKs into a crate whose job is
//! to move bytes. Every signing algorithm here is fully specified, published,
//! and does not change; a wrong signature fails loudly at the first request.
//!
//! | Provider | Requests | Time-limited URLs |
//! | --- | --- | --- |
//! | S3 | AWS Signature Version 4, HMAC-SHA256 | SigV4 in the query string |
//! | GCS | an OAuth 2.0 bearer token | V4 signed URL, RSA-SHA256 over the service account |
//! | Azure | a shared-key HMAC-SHA256 | a service SAS, HMAC-SHA256 |
//!
//! # No cryptographic primitive is implemented here
//!
//! Every signature is `ring`: `hmac::HMAC_SHA256` for S3 and Azure,
//! `signature::RSA_PKCS1_SHA256` with the OS CSPRNG for GCS, `digest::SHA256`
//! for every payload hash. What this file contains is the *canonical strings* —
//! the exact bytes each provider specifies as the input — and nothing below
//! them.
//!
//! # Where a GCS credential comes from
//!
//! Three shapes, and which one is configured decides what the backend can do:
//!
//! | `STORAGE_SECRET_KEY` | API calls | `signed_url` / `presigned_upload` |
//! | --- | --- | --- |
//! | a service-account JSON key | a token it mints itself, RS256 | **yes** |
//! | the literal `metadata` | the metadata server issues one | no — nothing to sign with |
//! | anything else | used as a bearer token verbatim | no — nothing to sign with |
//!
//! Workload identity and a supplied token are the two paths Google recommends
//! for code running on GCP, and neither can produce a signed URL: signing needs
//! the private key, and the whole point of workload identity is that the process
//! never holds one. [`GcsStorage::capabilities`] reports that honestly rather
//! than handing out a URL that 403s.

use std::ops::Range;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
#[cfg(feature = "gcs")]
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use moso_core::BoxFuture;
use moso_core::config::SecretString;
use moso_schema::Url;

use crate::{
    ByteStream, Listing, ObjectMeta, PutOpts, Result, Storage, StorageCapabilities, StorageKey,
};

// ---------------------------------------------------------------------------
// the shared client
// ---------------------------------------------------------------------------

/// The HTTP client every cloud backend shares.
///
/// One per process. sqlx has already chosen rustls' *ring* provider and two
/// providers in one process is a runtime panic, so the choice is made
/// explicitly here rather than left to whichever crate initialises first.
fn client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .user_agent(concat!("moso-storage/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default()
    })
}

/// Turn a transport failure into the taxonomy.
fn transport(backend: &'static str, error: reqwest::Error) -> crate::Error {
    crate::Error::unavailable(backend, error.to_string(), Some(Box::new(error)))
}

/// Turn a non-success status into the taxonomy.
///
/// The classification decides whether a job retries. A 404 is not found, a 403
/// is permanent, a 429 and a 5xx are worth another attempt, and everything else
/// is a refusal.
fn status_error(
    backend: &'static str,
    status: http::StatusCode,
    key: &str,
    body: &str,
) -> crate::Error {
    if status == http::StatusCode::NOT_FOUND {
        return crate::Error::not_found(key);
    }
    if status.as_u16() == 429 || status.is_server_error() {
        return crate::Error::unavailable(backend, format!("{status}: {body}"), None);
    }
    crate::Error::refused(backend, format!("{status}: {body}"))
}

/// Stream a `reqwest` response body as a [`ByteStream`].
fn response_stream(backend: &'static str, response: reqwest::Response) -> ByteStream {
    use futures_util::StreamExt as _;

    Box::pin(
        response
            .bytes_stream()
            .map(move |chunk| chunk.map_err(|error| transport(backend, error))),
    )
}

/// Percent-encode one key segment for a URL path.
///
/// A key may contain any byte a `StorageKey` allows, and several of them —
/// `+`, `?`, `#`, a space — change the meaning of a URL.
fn encode_path(key: &str) -> String {
    key.split('/')
        .map(|segment| {
            percent_encoding::utf8_percent_encode(segment, percent_encoding::NON_ALPHANUMERIC)
                .to_string()
                // The unreserved set from RFC 3986, restored so the URL is
                // readable in a console.
                .replace("%2D", "-")
                .replace("%2E", ".")
                .replace("%5F", "_")
                .replace("%7E", "~")
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Read the whole of a response body as text, for an error message.
async fn error_body(response: reqwest::Response) -> String {
    /// A provider's error document is XML or JSON and never large; anything
    /// past this is not going to help whoever reads the log.
    const LIMIT: usize = 4096;

    let mut text = response.text().await.unwrap_or_default();
    text.truncate(LIMIT);
    text
}

// ---------------------------------------------------------------------------
// S3
// ---------------------------------------------------------------------------

/// How an S3-compatible endpoint addresses buckets.
///
/// ```
/// use moso_storage::backend::AddressingStyle;
///
/// assert_eq!(AddressingStyle::default(), AddressingStyle::VirtualHosted);
/// ```
#[cfg(feature = "s3")]
#[cfg_attr(docsrs, doc(cfg(feature = "s3")))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AddressingStyle {
    /// `bucket.endpoint/key`. What S3 and R2 want.
    #[default]
    VirtualHosted,
    /// `endpoint/bucket/key`. What MinIO and most self-hosted gateways want.
    Path,
}

/// S3, and everything that speaks its API.
///
/// ```no_run
/// # use moso_core::config::SecretString;
/// # use moso_storage::backend::S3Storage;
/// # fn f(secret: SecretString) {
/// let _ = S3Storage::new("uploads", "eu-central-1", "AKIA…", secret);
/// # }
/// ```
#[cfg(feature = "s3")]
#[cfg_attr(docsrs, doc(cfg(feature = "s3")))]
#[derive(Debug)]
pub struct S3Storage {
    /// The bucket.
    bucket: String,
    /// The region, used in the signature whether or not the endpoint cares.
    region: String,
    /// The access key id.
    access_key: String,
    /// The secret access key, redacted in every `Debug` and log.
    secret_key: SecretString,
    /// A custom endpoint, for anything that is not AWS.
    endpoint: Option<String>,
    /// How buckets are addressed.
    addressing: AddressingStyle,
    /// A key prefix applied to every operation, so one bucket can host several
    /// applications without them being able to read each other's objects.
    prefix: Option<String>,
}

/// The smallest non-final multipart part S3 accepts.
#[cfg(feature = "s3")]
const S3_MIN_PART: u64 = 5 * 1024 * 1024;

#[cfg(feature = "s3")]
impl S3Storage {
    /// A backend for `bucket` in `region`.
    ///
    /// ```no_run
    /// # use moso_core::config::SecretString;
    /// # use moso_storage::backend::S3Storage;
    /// # fn f(secret: SecretString) {
    /// let _ = S3Storage::new("uploads", "us-east-1", "AKIA…", secret);
    /// # }
    /// ```
    #[must_use]
    pub fn new(
        bucket: impl Into<String>,
        region: impl Into<String>,
        access_key: impl Into<String>,
        secret_key: SecretString,
    ) -> Self {
        Self {
            bucket: bucket.into(),
            region: region.into(),
            access_key: access_key.into(),
            secret_key,
            endpoint: None,
            addressing: AddressingStyle::VirtualHosted,
            prefix: None,
        }
    }

    /// Point at a non-AWS endpoint, switching to path addressing.
    ///
    /// ```no_run
    /// # use moso_core::config::SecretString;
    /// # use moso_storage::backend::S3Storage;
    /// # fn f(secret: SecretString) {
    /// let _ = S3Storage::new("b", "auto", "k", secret).endpoint("http://127.0.0.1:9000");
    /// # }
    /// ```
    #[must_use]
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into().trim_end_matches('/').to_owned());
        // Every self-hosted gateway wants path addressing, and a virtual-hosted
        // request against one fails with a DNS error nobody can read.
        self.addressing = AddressingStyle::Path;
        self
    }

    /// Choose the addressing style explicitly.
    ///
    /// ```no_run
    /// # use moso_core::config::SecretString;
    /// # use moso_storage::backend::{AddressingStyle, S3Storage};
    /// # fn f(secret: SecretString) {
    /// let _ = S3Storage::new("b", "auto", "k", secret).addressing(AddressingStyle::Path);
    /// # }
    /// ```
    #[must_use]
    pub fn addressing(mut self, style: AddressingStyle) -> Self {
        self.addressing = style;
        self
    }

    /// Prefix every key, so one bucket can host several applications.
    ///
    /// ```no_run
    /// # use moso_core::config::SecretString;
    /// # use moso_storage::backend::S3Storage;
    /// # fn f(secret: SecretString) {
    /// let _ = S3Storage::new("b", "auto", "k", secret).prefix("shop");
    /// # }
    /// ```
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into().trim_matches('/').to_owned());
        self
    }

    /// The key as the bucket sees it, with the application prefix applied.
    fn scoped(&self, key: &str) -> String {
        match &self.prefix {
            Some(prefix) if !prefix.is_empty() => format!("{prefix}/{key}"),
            _ => key.to_owned(),
        }
    }

    /// The host and the path a request goes to.
    fn target(&self, key: &str) -> (String, String) {
        let scoped = self.scoped(key);
        let path = if scoped.is_empty() {
            "/".to_owned()
        } else {
            format!("/{}", encode_path(&scoped))
        };

        match (&self.endpoint, self.addressing) {
            (Some(endpoint), AddressingStyle::Path) => {
                let host = endpoint
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .to_owned();
                (host, format!("/{}{path}", self.bucket))
            }
            (Some(endpoint), AddressingStyle::VirtualHosted) => {
                let host = endpoint
                    .trim_start_matches("https://")
                    .trim_start_matches("http://");
                (format!("{}.{host}", self.bucket), path)
            }
            (None, AddressingStyle::Path) => (
                format!("s3.{}.amazonaws.com", self.region),
                format!("/{}{path}", self.bucket),
            ),
            (None, AddressingStyle::VirtualHosted) => (
                format!("{}.s3.{}.amazonaws.com", self.bucket, self.region),
                path,
            ),
        }
    }

    /// Whether the endpoint is plain HTTP — only ever true for a local MinIO.
    fn scheme(&self) -> &'static str {
        match self.endpoint.as_deref() {
            Some(endpoint) if endpoint.starts_with("http://") => "http",
            _ => "https",
        }
    }

    /// Build and send one signed request.
    async fn send(
        &self,
        method: &str,
        key: &str,
        query: &str,
        body: bytes::Bytes,
        extra: &[(&str, String)],
    ) -> Result<reqwest::Response> {
        let (host, path) = self.target(key);
        let payload_hash = crate::backend::sha256_hex(&body);

        let mut headers: Vec<(String, String)> = extra
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect();
        let signed = sigv4::sign(
            method,
            &host,
            &path,
            query,
            &payload_hash,
            &mut headers,
            "s3",
            &self.region,
            &self.access_key,
            self.secret_key.expose(),
            chrono::Utc::now(),
        );

        let url = if query.is_empty() {
            format!("{}://{host}{path}", self.scheme())
        } else {
            format!("{}://{host}{path}?{query}", self.scheme())
        };

        let mut request = client().request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET),
            &url,
        );
        for (name, value) in signed {
            request = request.header(name, value);
        }
        if !body.is_empty() {
            request = request.body(body);
        }

        request.send().await.map_err(|error| transport("s3", error))
    }

    /// Read an XML element's text content, non-recursively.
    ///
    /// A hand-rolled reader rather than an XML parser: S3's list response is
    /// flat, the elements are fixed, and a parser is thirty crates.
    fn xml_values(document: &str, tag: &str) -> Vec<String> {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let mut out = Vec::new();
        let mut rest = document;
        while let Some(start) = rest.find(&open) {
            let after = &rest[start + open.len()..];
            let Some(end) = after.find(&close) else {
                break;
            };
            out.push(unescape_xml(&after[..end]));
            rest = &after[end + close.len()..];
        }
        out
    }
}

/// Undo the five XML entity escapes.
#[cfg(feature = "s3")]
fn unescape_xml(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(feature = "s3")]
impl Storage for S3Storage {
    fn name(&self) -> &'static str {
        "s3"
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            ranges: true,
            signed_urls: true,
            presigned_upload: true,
            multipart: true,
            min_part_size: S3_MIN_PART,
            server_side_copy: true,
            // `If-None-Match: *` on `PutObject` is supported by S3 and by R2,
            // and is the only conditional write in the API.
            conditional_writes: true,
            public_objects: true,
            metadata: true,
            max_object_size: 5 * 1024 * 1024 * 1024 * 1024,
            delimited_listing: true,
        }
    }

    fn put<'a>(
        &'a self,
        key: &'a StorageKey,
        body: ByteStream,
        mut opts: PutOpts,
    ) -> BoxFuture<'a, Result<ObjectMeta>> {
        Box::pin(async move {
            // SigV4 signs a hash of the payload, so a single `PutObject` has to
            // know its bytes. Anything above the multipart threshold goes
            // through `multipart_start` instead, which signs each part
            // separately and never holds more than one part in memory.
            let bytes = crate::collect_bounded(body, MAX_SINGLE_PUT, "s3").await?;

            if opts.sniffs()
                && let Some(sniffed) = crate::sniff(&bytes)
            {
                opts.set_content_type(sniffed);
            }

            let digest = crate::backend::sha256_hex(&bytes);
            if let Some(expected) = opts.expected_checksum()
                && expected.digest() != digest
            {
                return Err(crate::Error::Checksum {
                    key: key.to_string(),
                    expected: expected.digest().to_owned(),
                    actual: digest,
                });
            }

            let mut extra = vec![("content-type".to_owned(), opts.content_type().to_owned())];
            if let Some(cache) = opts.cache_control_value() {
                extra.push(("cache-control".to_owned(), cache.to_owned()));
            }
            if let Some(content_disposition) = opts.content_disposition_value() {
                extra.push((
                    "content-disposition".to_owned(),
                    content_disposition.to_owned(),
                ));
            }
            if opts.visibility_value() == crate::Visibility::Public {
                extra.push(("x-amz-acl".to_owned(), "public-read".to_owned()));
            }
            if opts.refuses_overwrite() {
                extra.push(("if-none-match".to_owned(), "*".to_owned()));
            }
            for (name, value) in opts.metadata_pairs() {
                extra.push((format!("x-amz-meta-{name}"), value.clone()));
            }
            let extra: Vec<(&str, String)> = extra
                .iter()
                .map(|(name, value)| (name.as_str(), value.clone()))
                .collect();

            let size = bytes.len() as u64;
            let response = self.send("PUT", key.as_str(), "", bytes, &extra).await?;
            let status = response.status();
            let etag = response
                .headers()
                .get(http::header::ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);

            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("s3", status, key.as_str(), &body));
            }

            Ok(crate::object::meta_from(
                key,
                size,
                &opts,
                Some(crate::Checksum::sha256(digest)),
                etag,
            ))
        })
    }

    fn get<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<ByteStream>> {
        Box::pin(async move {
            let response = self
                .send("GET", key.as_str(), "", bytes::Bytes::new(), &[])
                .await?;
            let status = response.status();
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("s3", status, key.as_str(), &body));
            }
            Ok(response_stream("s3", response))
        })
    }

    fn get_range<'a>(
        &'a self,
        key: &'a StorageKey,
        range: Range<u64>,
    ) -> BoxFuture<'a, Result<ByteStream>> {
        Box::pin(async move {
            // HTTP ranges are inclusive at both ends; `Range<u64>` is not.
            let last = range.end.saturating_sub(1).max(range.start);
            let response = self
                .send(
                    "GET",
                    key.as_str(),
                    "",
                    bytes::Bytes::new(),
                    &[("range", format!("bytes={}-{last}", range.start))],
                )
                .await?;
            let status = response.status();
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("s3", status, key.as_str(), &body));
            }
            Ok(response_stream("s3", response))
        })
    }

    fn head<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<Option<ObjectMeta>>> {
        Box::pin(async move {
            let response = self
                .send("HEAD", key.as_str(), "", bytes::Bytes::new(), &[])
                .await?;
            let status = response.status();
            if status == http::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if !status.is_success() {
                return Err(status_error("s3", status, key.as_str(), ""));
            }
            Ok(Some(meta_from_headers(key, response.headers())))
        })
    }

    fn delete<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            // S3 answers 204 whether or not the object was there, so "was it
            // deleted" needs the `head` that precedes it.
            let existed = self.head(key).await?.is_some();
            let response = self
                .send("DELETE", key.as_str(), "", bytes::Bytes::new(), &[])
                .await?;
            let status = response.status();
            if !status.is_success() && status != http::StatusCode::NOT_FOUND {
                let body = error_body(response).await;
                return Err(status_error("s3", status, key.as_str(), &body));
            }
            Ok(existed)
        })
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        cursor: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            let scoped = self.scoped(prefix);
            let mut query = format!("list-type=2&prefix={}", encode_query(&scoped));
            if let Some(cursor) = cursor {
                query.push_str(&format!("&continuation-token={}", encode_query(cursor)));
            }

            let response = self
                .send("GET", "", &query, bytes::Bytes::new(), &[])
                .await?;
            let status = response.status();
            let document = response.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(status_error("s3", status, prefix, &document));
            }

            // Each `<Contents>` is one object; the fields inside are fixed.
            let mut objects = Vec::new();
            for block in document.split("<Contents>").skip(1) {
                let block = block.split("</Contents>").next().unwrap_or_default();
                let Some(raw) = Self::xml_values(block, "Key").into_iter().next() else {
                    continue;
                };
                let unscoped = self
                    .prefix
                    .as_deref()
                    .and_then(|prefix| raw.strip_prefix(&format!("{prefix}/")))
                    .unwrap_or(&raw);
                let Ok(key) = StorageKey::new(unscoped.to_owned()) else {
                    continue;
                };

                objects.push(ObjectMeta {
                    key,
                    size: Self::xml_values(block, "Size")
                        .first()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or_default(),
                    content_type: "application/octet-stream".to_owned(),
                    etag: Self::xml_values(block, "ETag").into_iter().next(),
                    modified_at: Self::xml_values(block, "LastModified")
                        .first()
                        .and_then(|value| value.parse::<chrono::DateTime<chrono::Utc>>().ok()),
                    checksum: None,
                    metadata: std::collections::BTreeMap::new(),
                    cache_control: None,
                    content_disposition: None,
                    public: false,
                });
            }

            Ok(Listing {
                objects,
                prefixes: Self::xml_values(&document, "Prefix"),
                cursor: Self::xml_values(&document, "NextContinuationToken")
                    .into_iter()
                    .next(),
            })
        })
    }

    fn copy<'a>(
        &'a self,
        from: &'a StorageKey,
        to: &'a StorageKey,
    ) -> BoxFuture<'a, Result<ObjectMeta>> {
        Box::pin(async move {
            let source = format!("/{}/{}", self.bucket, self.scoped(from.as_str()));
            let response = self
                .send(
                    "PUT",
                    to.as_str(),
                    "",
                    bytes::Bytes::new(),
                    &[("x-amz-copy-source", encode_path(&source))],
                )
                .await?;
            let status = response.status();
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("s3", status, from.as_str(), &body));
            }
            self.head(to)
                .await?
                .ok_or_else(|| crate::Error::not_found(to.as_str()))
        })
    }

    fn signed_url<'a>(
        &'a self,
        key: &'a StorageKey,
        ttl: std::time::Duration,
    ) -> BoxFuture<'a, Result<Url>> {
        Box::pin(async move {
            let (host, path) = self.target(key.as_str());
            let query = sigv4::presign(
                "GET",
                &host,
                &path,
                "s3",
                &self.region,
                &self.access_key,
                self.secret_key.expose(),
                ttl.as_secs().max(1),
                chrono::Utc::now(),
            );
            Url::parse_http(&format!("{}://{host}{path}?{query}", self.scheme()))
                .map_err(|error| crate::Error::config(error.message().to_owned()))
        })
    }

    fn presigned_upload<'a>(
        &'a self,
        key: &'a StorageKey,
        policy: crate::UploadPolicy,
    ) -> BoxFuture<'a, Result<crate::PresignedPost>> {
        Box::pin(async move {
            // A presigned `PUT` rather than a POST policy document: it is the
            // form every S3-compatible gateway implements, and it needs no
            // multipart form on the client.
            let (host, path) = self.target(key.as_str());
            let query = sigv4::presign(
                "PUT",
                &host,
                &path,
                "s3",
                &self.region,
                &self.access_key,
                self.secret_key.expose(),
                policy.ttl().as_secs().max(1),
                chrono::Utc::now(),
            );

            let mut fields: Vec<(String, String)> = policy
                .fields()
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            for (name, value) in policy.metadata_pairs() {
                fields.push((format!("x-amz-meta-{name}"), value.clone()));
            }
            if policy.visibility_value() == crate::Visibility::Public {
                fields.push(("x-amz-acl".to_owned(), "public-read".to_owned()));
            }

            Ok(crate::PresignedPost {
                url: Url::parse_http(&format!("{}://{host}{path}?{query}", self.scheme()))
                    .map_err(|error| crate::Error::config(error.message().to_owned()))?,
                method: "PUT".to_owned(),
                fields,
                key: key.clone(),
                expires_at: chrono::Utc::now()
                    + chrono::Duration::seconds(policy.ttl().as_secs() as i64),
                max_size: policy.max_size(),
            })
        })
    }

    fn multipart_start<'a>(
        &'a self,
        key: &'a StorageKey,
        opts: PutOpts,
    ) -> BoxFuture<'a, Result<crate::MultipartUpload>> {
        Box::pin(async move {
            let response = self
                .send(
                    "POST",
                    key.as_str(),
                    "uploads=",
                    bytes::Bytes::new(),
                    &[("content-type", opts.content_type().to_owned())],
                )
                .await?;
            let status = response.status();
            let document = response.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(status_error("s3", status, key.as_str(), &document));
            }

            let upload_id = Self::xml_values(&document, "UploadId")
                .into_iter()
                .next()
                .ok_or_else(|| {
                    crate::Error::refused(
                        "s3",
                        "the CreateMultipartUpload response had no UploadId",
                    )
                })?;

            Ok(crate::MultipartUpload::new(
                key.clone(),
                upload_id,
                "s3",
                S3_MIN_PART,
                std::sync::Arc::new(S3Multipart {
                    bucket: self.bucket.clone(),
                    region: self.region.clone(),
                    access_key: self.access_key.clone(),
                    secret_key: self.secret_key.clone(),
                    endpoint: self.endpoint.clone(),
                    addressing: self.addressing,
                    prefix: self.prefix.clone(),
                }),
            ))
        })
    }

    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let response = self
                .send(
                    "GET",
                    "",
                    "list-type=2&max-keys=0",
                    bytes::Bytes::new(),
                    &[],
                )
                .await?;
            let status = response.status();
            if status.is_success() {
                return Ok(());
            }
            let body = error_body(response).await;
            Err(status_error("s3", status, "", &body))
        })
    }
}

/// The largest object this backend writes in one request.
///
/// Above it, a caller has to use [`Storage::multipart_start`]: SigV4 signs a
/// hash of the payload, so a single `PutObject` cannot stream, and buffering a
/// gigabyte to sign it is exactly what the peak-RSS criterion forbids.
#[cfg(feature = "s3")]
pub const MAX_SINGLE_PUT: u64 = 64 * 1024 * 1024;

/// The multipart half of the S3 backend.
#[cfg(feature = "s3")]
#[derive(Debug)]
struct S3Multipart {
    bucket: String,
    region: String,
    access_key: String,
    secret_key: SecretString,
    endpoint: Option<String>,
    addressing: AddressingStyle,
    prefix: Option<String>,
}

#[cfg(feature = "s3")]
impl S3Multipart {
    /// The backend this drives, rebuilt so the request code is shared.
    fn storage(&self) -> S3Storage {
        S3Storage {
            bucket: self.bucket.clone(),
            region: self.region.clone(),
            access_key: self.access_key.clone(),
            secret_key: self.secret_key.clone(),
            endpoint: self.endpoint.clone(),
            addressing: self.addressing,
            prefix: self.prefix.clone(),
        }
    }
}

#[cfg(feature = "s3")]
impl crate::multipart::MultipartDriver for S3Multipart {
    fn upload_part<'a>(
        &'a self,
        upload_id: &'a str,
        key: &'a StorageKey,
        number: crate::PartNumber,
        body: bytes::Bytes,
    ) -> BoxFuture<'a, Result<crate::CompletedPart>> {
        Box::pin(async move {
            let size = body.len() as u64;
            let query = format!(
                "partNumber={}&uploadId={}",
                number.get(),
                encode_query(upload_id),
            );
            let response = self
                .storage()
                .send("PUT", key.as_str(), &query, body, &[])
                .await?;
            let status = response.status();
            let etag = response
                .headers()
                .get(http::header::ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("s3", status, key.as_str(), &body));
            }
            Ok(crate::CompletedPart {
                number,
                etag: etag.unwrap_or_default(),
                size,
            })
        })
    }

    fn complete<'a>(
        &'a self,
        upload_id: &'a str,
        key: &'a StorageKey,
        parts: Vec<crate::CompletedPart>,
    ) -> BoxFuture<'a, Result<ObjectMeta>> {
        Box::pin(async move {
            let mut document = String::from("<CompleteMultipartUpload>");
            for part in &parts {
                document.push_str(&format!(
                    "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                    part.number.get(),
                    part.etag.replace('&', "&amp;").replace('<', "&lt;"),
                ));
            }
            document.push_str("</CompleteMultipartUpload>");

            let storage = self.storage();
            let response = storage
                .send(
                    "POST",
                    key.as_str(),
                    &format!("uploadId={}", encode_query(upload_id)),
                    bytes::Bytes::from(document),
                    &[("content-type", "application/xml".to_owned())],
                )
                .await?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            // S3 can answer 200 and *then* report a failure in the body, which
            // is the classic way a multipart upload silently loses data.
            if !status.is_success() || body.contains("<Error>") {
                return Err(status_error("s3", status, key.as_str(), &body));
            }

            storage
                .head(key)
                .await?
                .ok_or_else(|| crate::Error::not_found(key.as_str()))
        })
    }

    fn abort<'a>(&'a self, upload_id: &'a str, key: &'a StorageKey) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let response = self
                .storage()
                .send(
                    "DELETE",
                    key.as_str(),
                    &format!("uploadId={}", encode_query(upload_id)),
                    bytes::Bytes::new(),
                    &[],
                )
                .await?;
            let status = response.status();
            if status.is_success() || status == http::StatusCode::NOT_FOUND {
                return Ok(());
            }
            let body = error_body(response).await;
            Err(status_error("s3", status, key.as_str(), &body))
        })
    }
}

/// Percent-encode a query-string value.
#[cfg(feature = "cloud")]
fn encode_query(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC)
        .to_string()
        .replace("%2D", "-")
        .replace("%2E", ".")
        .replace("%5F", "_")
        .replace("%7E", "~")
}

/// Build metadata from a `HEAD` response's headers.
#[cfg(feature = "cloud")]
fn meta_from_headers(key: &StorageKey, headers: &http::HeaderMap) -> ObjectMeta {
    let text = |name: http::HeaderName| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };

    let mut metadata = std::collections::BTreeMap::new();
    for (name, value) in headers {
        for prefix in ["x-amz-meta-", "x-goog-meta-", "x-ms-meta-"] {
            if let Some(stripped) = name.as_str().strip_prefix(prefix)
                && let Ok(value) = value.to_str()
            {
                metadata.insert(stripped.to_owned(), value.to_owned());
            }
        }
    }

    ObjectMeta {
        key: key.clone(),
        size: text(http::header::CONTENT_LENGTH)
            .and_then(|value| value.parse().ok())
            .unwrap_or_default(),
        content_type: text(http::header::CONTENT_TYPE)
            .unwrap_or_else(|| "application/octet-stream".to_owned()),
        etag: text(http::header::ETAG),
        modified_at: text(http::header::LAST_MODIFIED)
            .and_then(|value| chrono::DateTime::parse_from_rfc2822(&value).ok())
            .map(|value| value.with_timezone(&chrono::Utc)),
        checksum: None,
        metadata,
        cache_control: text(http::header::CACHE_CONTROL),
        content_disposition: text(http::header::CONTENT_DISPOSITION),
        public: false,
    }
}

// ---------------------------------------------------------------------------
// AWS Signature Version 4
// ---------------------------------------------------------------------------

/// AWS Signature Version 4, in the two forms S3 needs.
///
/// About a hundred lines against a dependency that would pull the AWS SDK into
/// a crate whose job is to move bytes. The algorithm is fully specified and
/// does not change; a wrong signature fails loudly at the first request.
#[cfg(feature = "s3")]
mod sigv4 {
    use ring::hmac;

    /// Sign a request in place, returning the headers to send.
    ///
    /// `extra` is mutated: the signature covers every header it names, so the
    /// list has to be canonical before the signing string is built.
    #[expect(
        clippy::too_many_arguments,
        reason = "\
        every parameter is a distinct input to the signature and grouping them \
        into a struct would only move the list one line up"
    )]
    pub(super) fn sign(
        method: &str,
        host: &str,
        path: &str,
        query: &str,
        payload_hash: &str,
        extra: &mut Vec<(String, String)>,
        service: &str,
        region: &str,
        access_key: &str,
        secret_key: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Vec<(String, String)> {
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();

        extra.push(("host".to_owned(), host.to_owned()));
        extra.push(("x-amz-content-sha256".to_owned(), payload_hash.to_owned()));
        extra.push(("x-amz-date".to_owned(), amz_date.clone()));

        // Canonical order: lowercase name, sorted, values trimmed.
        let mut canonical: Vec<(String, String)> = extra
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        canonical.sort_by(|a, b| a.0.cmp(&b.0));

        let signed_headers = canonical
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let header_block = canonical
            .iter()
            .map(|(name, value)| format!("{name}:{value}\n"))
            .collect::<String>();

        let canonical_request = format!(
            "{method}\n{path}\n{}\n{header_block}\n{signed_headers}\n{payload_hash}",
            canonical_query(query),
        );

        let scope = format!("{date}/{region}/{service}/aws4_request");
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            super::crate_sha256_hex(canonical_request.as_bytes()),
        );
        let signature = super::hex(&signing_key(
            secret_key,
            &date,
            region,
            service,
            to_sign.as_bytes(),
        ));

        let mut headers = extra.clone();
        headers.push((
            "authorization".to_owned(),
            format!(
                "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, \
                 SignedHeaders={signed_headers}, Signature={signature}",
            ),
        ));
        headers
    }

    /// Presign a URL: the same signature, carried in the query string.
    #[expect(
        clippy::too_many_arguments,
        reason = "\
        as `sign`: every parameter is a distinct input to the signature"
    )]
    pub(super) fn presign(
        method: &str,
        host: &str,
        path: &str,
        service: &str,
        region: &str,
        access_key: &str,
        secret_key: &str,
        ttl_seconds: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> String {
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let scope = format!("{date}/{region}/{service}/aws4_request");

        // A presigned URL's expiry is capped at seven days by AWS; asking for
        // more produces a URL that is rejected on sight.
        let expires = ttl_seconds.min(7 * 24 * 3600);

        let query = format!(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256\
             &X-Amz-Credential={}\
             &X-Amz-Date={amz_date}\
             &X-Amz-Expires={expires}\
             &X-Amz-SignedHeaders=host",
            super::encode_query(&format!("{access_key}/{scope}")),
        );

        let canonical_request =
            format!("{method}\n{path}\n{query}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD",);
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            super::crate_sha256_hex(canonical_request.as_bytes()),
        );
        let signature = super::hex(&signing_key(
            secret_key,
            &date,
            region,
            service,
            to_sign.as_bytes(),
        ));

        format!("{query}&X-Amz-Signature={signature}")
    }

    /// Derive the daily signing key and sign with it.
    fn signing_key(
        secret_key: &str,
        date: &str,
        region: &str,
        service: &str,
        message: &[u8],
    ) -> Vec<u8> {
        let step = |key: &[u8], data: &[u8]| {
            hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
                .as_ref()
                .to_vec()
        };
        let key = step(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
        let key = step(&key, region.as_bytes());
        let key = step(&key, service.as_bytes());
        let key = step(&key, b"aws4_request");
        step(&key, message)
    }

    /// Sort a query string's parameters, as the canonical request requires.
    fn canonical_query(query: &str) -> String {
        if query.is_empty() {
            return String::new();
        }
        let mut pairs: Vec<(String, String)> = query
            .split('&')
            .map(|pair| {
                let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                (name.to_owned(), value.to_owned())
            })
            .collect();
        pairs.sort();
        pairs
            .into_iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("&")
    }
}

/// SHA-256, hex-encoded — re-exported into the `sigv4` module's scope.
#[cfg(feature = "cloud")]
fn crate_sha256_hex(bytes: &[u8]) -> String {
    crate::backend::sha256_hex(bytes)
}

/// Lowercase hex — re-exported into the `sigv4` module's scope.
#[cfg(feature = "cloud")]
fn hex(bytes: &[u8]) -> String {
    crate::backend::hex(bytes)
}

/// HMAC-SHA256, which is every Azure signature: the shared key on a request and
/// the service SAS on a URL.
#[cfg(feature = "azure")]
fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key), message)
        .as_ref()
        .to_vec()
}

// ---------------------------------------------------------------------------
// GCS
// ---------------------------------------------------------------------------

/// How a [`GcsStorage`] proves who it is.
///
/// Three shapes, because Google supports three and they are not
/// interchangeable: only the first holds a private key, and only a private key
/// can sign a URL.
#[cfg(feature = "gcs")]
enum GcsCredential {
    /// A service-account key: it mints its own tokens and signs its own URLs.
    ServiceAccount(Box<ServiceAccount>),
    /// The GCP metadata server issues a token per request. Workload identity.
    Metadata,
    /// An OAuth 2.0 access token supplied by the environment, used verbatim.
    Token(SecretString),
}

#[cfg(feature = "gcs")]
impl core::fmt::Debug for GcsCredential {
    /// Names the shape and never the secret.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ServiceAccount(account) => f
                .debug_tuple("ServiceAccount")
                .field(&account.client_email)
                .finish(),
            Self::Metadata => f.write_str("Metadata"),
            Self::Token(_) => f.write_str("Token(<redacted>)"),
        }
    }
}

/// Google Cloud Storage.
///
/// # Authentication
///
/// Pass one of three things as `service_account`:
///
/// - **a service-account JSON key**, verbatim, as
///   `gcloud iam service-accounts keys create` writes it. The backend mints its
///   own access tokens from it and can sign URLs;
/// - **the literal `"metadata"`**, which is workload identity: the GCP metadata
///   server issues a token per request and no private key ever exists in the
///   process, so nothing can be signed;
/// - **an access token**, e.g. from `gcloud auth print-access-token`, used
///   verbatim until it expires. Nothing can be signed with it either.
///
/// [`capabilities`](Storage::capabilities) reports which of the two it got, so
/// a caller can check before asking for a URL rather than being handed one that
/// 403s.
///
/// ```no_run
/// # #[cfg(feature = "gcs")] {
/// # use moso_core::config::SecretString;
/// # use moso_storage::backend::GcsStorage;
/// # fn f(token: SecretString) -> moso_storage::Result<()> {
/// let _ = GcsStorage::new("uploads", token)?;
/// # Ok(()) }
/// # }
/// ```
#[cfg(feature = "gcs")]
#[cfg_attr(docsrs, doc(cfg(feature = "gcs")))]
#[derive(Debug)]
pub struct GcsStorage {
    /// The bucket.
    bucket: String,
    /// What proves who this process is.
    credential: GcsCredential,
    /// A key prefix applied to every operation.
    prefix: Option<String>,
}

/// Where the GCP metadata server answers.
#[cfg(feature = "gcs")]
const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

/// The host a V4 signed URL points at, and the XML API's endpoint.
#[cfg(feature = "gcs")]
const GCS_HOST: &str = "storage.googleapis.com";

#[cfg(feature = "gcs")]
impl GcsStorage {
    /// A backend for `bucket`, authenticating with one of the three credentials.
    ///
    /// A JSON key is parsed and its private key is loaded **here**, so a
    /// malformed credential is a boot error naming what is wrong rather than a
    /// 401 at the first upload.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) when a JSON key is missing
    /// `client_email` or `private_key`, when the private key is not a PEM block
    /// this can read, or when `ring` rejects it — most often because it is
    /// shorter than the 2048 bits RSA signing requires.
    ///
    /// ```no_run
    /// # use moso_core::config::SecretString;
    /// # use moso_storage::backend::GcsStorage;
    /// # fn f(token: SecretString) -> moso_storage::Result<()> {
    /// let _ = GcsStorage::new("b", token)?;
    /// # Ok(()) }
    /// ```
    pub fn new(bucket: impl Into<String>, service_account: SecretString) -> Result<Self> {
        let value = service_account.expose().trim();

        let credential = if value.starts_with('{') {
            GcsCredential::ServiceAccount(Box::new(ServiceAccount::parse(value)?))
        } else if value.eq_ignore_ascii_case("metadata") {
            GcsCredential::Metadata
        } else {
            GcsCredential::Token(service_account)
        };

        Ok(Self {
            bucket: bucket.into(),
            credential,
            prefix: None,
        })
    }

    /// The service account, when one was configured.
    fn service_account(&self) -> Option<&ServiceAccount> {
        match &self.credential {
            GcsCredential::ServiceAccount(account) => Some(account),
            GcsCredential::Metadata | GcsCredential::Token(_) => None,
        }
    }

    /// The service account, or the error that explains what to configure.
    fn signing_account(&self) -> Result<&ServiceAccount> {
        self.service_account().ok_or_else(|| {
            crate::Error::config(
                "the gcs backend cannot sign a URL without a service-account key: a \
                 metadata-server token and a supplied access token are bearer credentials and hold \
                 no private key. Set `STORAGE_SECRET_KEY` to the service-account JSON, or serve the \
                 object through your own handler with `moso_storage::serve`",
            )
        })
    }

    /// The XML API path of one object: what a signed URL points at.
    ///
    /// Percent-encoded per segment, because `/` separates key segments and must
    /// survive while a `+`, a `#` or a space must not.
    fn object_path(&self, key: &StorageKey) -> String {
        format!(
            "/{}/{}",
            encode_path(&self.bucket),
            encode_path(&self.scoped(key.as_str())),
        )
    }

    /// A V4 signed URL for one object, with extra headers folded into the
    /// signature.
    ///
    /// Every `x-goog-*` header a client sends has to be signed — Google refuses
    /// the request otherwise — so the metadata a presigned upload carries is
    /// part of the canonical string rather than something added afterwards.
    fn signed_url_for(
        &self,
        method: &str,
        key: &StorageKey,
        headers: &[(String, String)],
        ttl: std::time::Duration,
    ) -> Result<(Url, Vec<(String, String)>)> {
        let account = self.signing_account()?;
        let path = self.object_path(key);
        let query = goog4::presign(
            account,
            method,
            GCS_HOST,
            &path,
            headers,
            ttl.as_secs().max(1),
            chrono::Utc::now(),
        )?;
        let url = Url::parse_http(&format!("https://{GCS_HOST}{path}?{query}"))
            .map_err(|error| crate::Error::config(error.message().to_owned()))?;
        Ok((url, headers.to_vec()))
    }

    /// Prefix every key.
    ///
    /// ```no_run
    /// # use moso_core::config::SecretString;
    /// # use moso_storage::backend::GcsStorage;
    /// # fn f(token: SecretString) -> moso_storage::Result<()> {
    /// let _ = GcsStorage::new("b", token)?.prefix("shop");
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into().trim_matches('/').to_owned());
        self
    }

    /// The key as the bucket sees it.
    fn scoped(&self, key: &str) -> String {
        match &self.prefix {
            Some(prefix) if !prefix.is_empty() => format!("{prefix}/{key}"),
            _ => key.to_owned(),
        }
    }

    /// The bearer token for one request.
    async fn bearer(&self) -> Result<String> {
        match &self.credential {
            GcsCredential::Token(token) => Ok(token.expose().to_owned()),
            GcsCredential::Metadata => {
                let response = client()
                    .get(METADATA_TOKEN_URL)
                    .header("metadata-flavor", "Google")
                    .send()
                    .await
                    .map_err(|error| transport("gcs", error))?;
                let token: OauthToken = response
                    .json()
                    .await
                    .map_err(|error| transport("gcs", error))?;
                Ok(token.access_token)
            }
            GcsCredential::ServiceAccount(account) => account.access_token().await,
        }
    }

    /// Send one authenticated request.
    async fn send(
        &self,
        method: reqwest::Method,
        url: &str,
        body: bytes::Bytes,
        extra: &[(&str, String)],
    ) -> Result<reqwest::Response> {
        let mut request = client()
            .request(method, url)
            .bearer_auth(self.bearer().await?);
        for (name, value) in extra {
            request = request.header(*name, value);
        }
        if !body.is_empty() {
            request = request.body(body);
        }
        request
            .send()
            .await
            .map_err(|error| transport("gcs", error))
    }
}

#[cfg(feature = "gcs")]
impl Storage for GcsStorage {
    fn name(&self) -> &'static str {
        "gcs"
    }

    fn capabilities(&self) -> StorageCapabilities {
        // A V4 signed URL is an RSA signature over the service account's private
        // key. A metadata-server token and a supplied access token are bearer
        // credentials and hold no key, so with either of those the honest answer
        // is `false` rather than a URL that 403s.
        let signs = self.service_account().is_some();

        StorageCapabilities {
            ranges: true,
            server_side_copy: true,
            public_objects: true,
            metadata: true,
            delimited_listing: true,
            max_object_size: 5 * 1024 * 1024 * 1024 * 1024,
            signed_urls: signs,
            presigned_upload: signs,
            multipart: false,
            min_part_size: 0,
            conditional_writes: true,
        }
    }

    fn put<'a>(
        &'a self,
        key: &'a StorageKey,
        body: ByteStream,
        mut opts: PutOpts,
    ) -> BoxFuture<'a, Result<ObjectMeta>> {
        Box::pin(async move {
            let bytes = crate::collect_bounded(body, MAX_GCS_PUT, "gcs").await?;
            if opts.sniffs()
                && let Some(sniffed) = crate::sniff(&bytes)
            {
                opts.set_content_type(sniffed);
            }

            let digest = crate::backend::sha256_hex(&bytes);
            if let Some(expected) = opts.expected_checksum()
                && expected.digest() != digest
            {
                return Err(crate::Error::Checksum {
                    key: key.to_string(),
                    expected: expected.digest().to_owned(),
                    actual: digest,
                });
            }

            let mut url = format!(
                "https://storage.googleapis.com/upload/storage/v1/b/{}/o?uploadType=media&name={}",
                encode_query(&self.bucket),
                encode_query(&self.scoped(key.as_str())),
            );
            if opts.refuses_overwrite() {
                url.push_str("&ifGenerationMatch=0");
            }

            let size = bytes.len() as u64;
            let response = self
                .send(
                    reqwest::Method::POST,
                    &url,
                    bytes,
                    &[("content-type", opts.content_type().to_owned())],
                )
                .await?;
            let status = response.status();
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("gcs", status, key.as_str(), &body));
            }

            Ok(crate::object::meta_from(
                key,
                size,
                &opts,
                Some(crate::Checksum::sha256(digest)),
                None,
            ))
        })
    }

    fn get<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<ByteStream>> {
        Box::pin(async move {
            let response = self
                .send(
                    reqwest::Method::GET,
                    &self.media_url(key),
                    bytes::Bytes::new(),
                    &[],
                )
                .await?;
            let status = response.status();
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("gcs", status, key.as_str(), &body));
            }
            Ok(response_stream("gcs", response))
        })
    }

    fn get_range<'a>(
        &'a self,
        key: &'a StorageKey,
        range: Range<u64>,
    ) -> BoxFuture<'a, Result<ByteStream>> {
        Box::pin(async move {
            let last = range.end.saturating_sub(1).max(range.start);
            let response = self
                .send(
                    reqwest::Method::GET,
                    &self.media_url(key),
                    bytes::Bytes::new(),
                    &[("range", format!("bytes={}-{last}", range.start))],
                )
                .await?;
            let status = response.status();
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("gcs", status, key.as_str(), &body));
            }
            Ok(response_stream("gcs", response))
        })
    }

    fn head<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<Option<ObjectMeta>>> {
        Box::pin(async move {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Object {
                size: Option<String>,
                content_type: Option<String>,
                etag: Option<String>,
                updated: Option<String>,
                cache_control: Option<String>,
                content_disposition: Option<String>,
                #[serde(default)]
                metadata: std::collections::BTreeMap<String, String>,
            }

            let response = self
                .send(
                    reqwest::Method::GET,
                    &self.object_url(key),
                    bytes::Bytes::new(),
                    &[],
                )
                .await?;
            let status = response.status();
            if status == http::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("gcs", status, key.as_str(), &body));
            }

            let object: Object = response
                .json()
                .await
                .map_err(|error| transport("gcs", error))?;
            Ok(Some(ObjectMeta {
                key: key.clone(),
                size: object
                    .size
                    .and_then(|size| size.parse().ok())
                    .unwrap_or_default(),
                content_type: object
                    .content_type
                    .unwrap_or_else(|| "application/octet-stream".to_owned()),
                etag: object.etag,
                modified_at: object
                    .updated
                    .and_then(|value| value.parse::<chrono::DateTime<chrono::Utc>>().ok()),
                checksum: None,
                metadata: object.metadata,
                cache_control: object.cache_control,
                content_disposition: object.content_disposition,
                public: false,
            }))
        })
    }

    fn delete<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let response = self
                .send(
                    reqwest::Method::DELETE,
                    &self.object_url(key),
                    bytes::Bytes::new(),
                    &[],
                )
                .await?;
            let status = response.status();
            if status == http::StatusCode::NOT_FOUND {
                return Ok(false);
            }
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("gcs", status, key.as_str(), &body));
            }
            Ok(true)
        })
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        cursor: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Page {
                #[serde(default)]
                items: Vec<Item>,
                next_page_token: Option<String>,
                #[serde(default)]
                prefixes: Vec<String>,
            }

            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Item {
                name: String,
                size: Option<String>,
                content_type: Option<String>,
                etag: Option<String>,
                updated: Option<String>,
            }

            let mut url = format!(
                "https://storage.googleapis.com/storage/v1/b/{}/o?prefix={}",
                encode_query(&self.bucket),
                encode_query(&self.scoped(prefix)),
            );
            if let Some(cursor) = cursor {
                url.push_str(&format!("&pageToken={}", encode_query(cursor)));
            }

            let response = self
                .send(reqwest::Method::GET, &url, bytes::Bytes::new(), &[])
                .await?;
            let status = response.status();
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("gcs", status, prefix, &body));
            }

            let page: Page = response
                .json()
                .await
                .map_err(|error| transport("gcs", error))?;

            let objects = page
                .items
                .into_iter()
                .filter_map(|item| {
                    let unscoped = self
                        .prefix
                        .as_deref()
                        .and_then(|prefix| item.name.strip_prefix(&format!("{prefix}/")))
                        .unwrap_or(&item.name);
                    let key = StorageKey::new(unscoped.to_owned()).ok()?;
                    Some(ObjectMeta {
                        key,
                        size: item
                            .size
                            .and_then(|size| size.parse().ok())
                            .unwrap_or_default(),
                        content_type: item
                            .content_type
                            .unwrap_or_else(|| "application/octet-stream".to_owned()),
                        etag: item.etag,
                        modified_at: item
                            .updated
                            .and_then(|value| value.parse::<chrono::DateTime<chrono::Utc>>().ok()),
                        checksum: None,
                        metadata: std::collections::BTreeMap::new(),
                        cache_control: None,
                        content_disposition: None,
                        public: false,
                    })
                })
                .collect();

            Ok(Listing {
                objects,
                prefixes: page.prefixes,
                cursor: page.next_page_token,
            })
        })
    }

    fn copy<'a>(
        &'a self,
        from: &'a StorageKey,
        to: &'a StorageKey,
    ) -> BoxFuture<'a, Result<ObjectMeta>> {
        Box::pin(async move {
            let url = format!(
                "https://storage.googleapis.com/storage/v1/b/{bucket}/o/{from}/rewriteTo/b/{bucket}/o/{to}",
                bucket = encode_query(&self.bucket),
                from = encode_query(&self.scoped(from.as_str())),
                to = encode_query(&self.scoped(to.as_str())),
            );
            let response = self
                .send(reqwest::Method::POST, &url, bytes::Bytes::new(), &[])
                .await?;
            let status = response.status();
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("gcs", status, from.as_str(), &body));
            }
            self.head(to)
                .await?
                .ok_or_else(|| crate::Error::not_found(to.as_str()))
        })
    }

    fn signed_url<'a>(
        &'a self,
        key: &'a StorageKey,
        ttl: std::time::Duration,
    ) -> BoxFuture<'a, Result<Url>> {
        Box::pin(async move {
            // A download needs nothing but `host` in the signature: the browser
            // sends no `x-goog-*` header on a plain GET.
            let (url, _fields) = self.signed_url_for("GET", key, &[], ttl)?;
            Ok(url)
        })
    }

    fn presigned_upload<'a>(
        &'a self,
        key: &'a StorageKey,
        policy: crate::UploadPolicy,
    ) -> BoxFuture<'a, Result<crate::PresignedPost>> {
        Box::pin(async move {
            // Google refuses a request carrying an `x-goog-*` header the
            // signature did not cover, so the metadata and the ACL are inputs to
            // the signature rather than fields bolted on afterwards.
            let mut headers: Vec<(String, String)> = policy
                .metadata_pairs()
                .iter()
                .map(|(name, value)| (format!("x-goog-meta-{name}"), value.clone()))
                .collect();
            if policy.visibility_value() == crate::Visibility::Public {
                headers.push(("x-goog-acl".to_owned(), "public-read".to_owned()));
            }

            let (url, mut fields) = self.signed_url_for("PUT", key, &headers, policy.ttl())?;
            for (name, value) in policy.fields() {
                fields.push((name.clone(), value.clone()));
            }

            Ok(crate::PresignedPost {
                url,
                method: "PUT".to_owned(),
                fields,
                key: key.clone(),
                expires_at: chrono::Utc::now()
                    + chrono::Duration::seconds(policy.ttl().as_secs() as i64),
                max_size: policy.max_size(),
            })
        })
    }

    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let url = format!(
                "https://storage.googleapis.com/storage/v1/b/{}",
                encode_query(&self.bucket),
            );
            let response = self
                .send(reqwest::Method::GET, &url, bytes::Bytes::new(), &[])
                .await?;
            let status = response.status();
            if status.is_success() {
                return Ok(());
            }
            let body = error_body(response).await;
            Err(status_error("gcs", status, "", &body))
        })
    }
}

#[cfg(feature = "gcs")]
impl GcsStorage {
    /// The JSON metadata URL of one object.
    fn object_url(&self, key: &StorageKey) -> String {
        format!(
            "https://storage.googleapis.com/storage/v1/b/{}/o/{}",
            encode_query(&self.bucket),
            encode_query(&self.scoped(key.as_str())),
        )
    }

    /// The media URL of one object.
    fn media_url(&self, key: &StorageKey) -> String {
        format!("{}?alt=media", self.object_url(key))
    }
}

/// The largest object the GCS backend writes in one request.
///
/// Above it, a resumable upload session is the right answer; this backend does
/// not open one, so the limit is enforced rather than silently exceeded.
#[cfg(feature = "gcs")]
pub const MAX_GCS_PUT: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// the GCS service account
// ---------------------------------------------------------------------------

/// An OAuth 2.0 token response, from the metadata server or the token endpoint.
#[cfg(feature = "gcs")]
#[derive(serde::Deserialize)]
struct OauthToken {
    /// The token itself.
    access_token: String,
    /// How many seconds it lasts. Absent from some metadata-server replies.
    #[serde(default)]
    expires_in: Option<i64>,
}

/// A service-account JSON key, as `gcloud` writes it.
#[cfg(feature = "gcs")]
#[derive(serde::Deserialize)]
struct ServiceAccountKey {
    /// The account's address, which is the signing identity.
    client_email: Option<String>,
    /// The RSA private key, PEM.
    private_key: Option<String>,
    /// Where to exchange an assertion for a token.
    token_uri: Option<String>,
}

/// A service-account key, parsed and ready to sign.
///
/// Holds the one private key in this crate, which is why it has a hand-written
/// [`Debug`] and why the key is loaded at construction: a credential that
/// cannot sign should fail at boot, next to the configuration that is wrong,
/// and not at the first presigned upload three hours later.
#[cfg(feature = "gcs")]
struct ServiceAccount {
    /// The account's address. Public information: it is in every signed URL.
    client_email: String,
    /// The RSA key pair the signature comes from.
    key: ring::signature::RsaKeyPair,
    /// Where to exchange a JWT assertion for an access token.
    token_uri: String,
    /// The last token minted, kept until it is nearly expired.
    cached: std::sync::Mutex<Option<CachedToken>>,
}

/// An access token and when it stops working.
#[cfg(feature = "gcs")]
struct CachedToken {
    /// The token.
    value: String,
    /// When it expires, as the token endpoint reported it.
    expires_at: chrono::DateTime<chrono::Utc>,
}

/// Google's default token endpoint, when the key does not name one.
#[cfg(feature = "gcs")]
const GOOGLE_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// The scope a storage token is minted for.
///
/// `devstorage.read_write` and not `cloud-platform`: a token that can read and
/// write objects is what this backend needs, and a token that can do everything
/// is what an exfiltrated one does.
#[cfg(feature = "gcs")]
const GCS_SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";

/// The grant type of an RFC 7523 assertion.
#[cfg(feature = "gcs")]
const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// How long before expiry a cached token is replaced.
///
/// A token handed to a request one second before it expires is a 401 for
/// whoever gets it.
#[cfg(feature = "gcs")]
const TOKEN_REFRESH_MARGIN: i64 = 60;

#[cfg(feature = "gcs")]
impl core::fmt::Debug for ServiceAccount {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ServiceAccount")
            .field("client_email", &self.client_email)
            .field("token_uri", &self.token_uri)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "gcs")]
impl ServiceAccount {
    /// Parse and load a service-account JSON key.
    fn parse(json: &str) -> Result<Self> {
        /// What every message points at, because it is the one file to look in.
        const WHERE: &str = "the service-account JSON in `STORAGE_SECRET_KEY`";

        let parsed: ServiceAccountKey = serde_json::from_str(json).map_err(|error| {
            crate::Error::config(format!(
                "{WHERE} is not valid JSON ({error}) — pass the file `gcloud iam service-accounts \
                 keys create` wrote, verbatim",
            ))
        })?;

        let client_email = parsed.client_email.ok_or_else(|| {
            crate::Error::config(format!(
                "{WHERE} has no `client_email`, so there is no identity to sign as",
            ))
        })?;
        let private_key = parsed.private_key.ok_or_else(|| {
            crate::Error::config(format!(
                "{WHERE} has no `private_key` — a key file downloaded as the `P12` type does not \
                 carry one; download the `JSON` type instead",
            ))
        })?;

        let key = load_rsa_key(&private_key).ok_or_else(|| {
            crate::Error::config(format!(
                "the `private_key` in {WHERE} is not an RSA private key `ring` can use — it must \
                 be an unencrypted PEM block of at least 2048 bits, which is what Google issues",
            ))
        })?;

        Ok(Self {
            client_email,
            key,
            token_uri: parsed
                .token_uri
                .unwrap_or_else(|| GOOGLE_TOKEN_URI.to_owned()),
            cached: std::sync::Mutex::new(None),
        })
    }

    /// Sign `message` with RSA PKCS#1 v1.5 over SHA-256.
    ///
    /// The one primitive both halves of this credential need: a V4 signed URL
    /// signs a canonical string, and a token assertion signs a JWT. Neither
    /// implements anything — `ring` does the work, seeded from the OS CSPRNG.
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        let mut signature = vec![0_u8; self.key.public().modulus_len()];
        self.key
            .sign(
                &ring::signature::RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                message,
                &mut signature,
            )
            .map_err(|_| {
                crate::Error::refused(
                    "gcs",
                    "the service-account key could not produce a signature",
                )
            })?;
        Ok(signature)
    }

    /// A usable access token, minting one when the cached one is nearly gone.
    ///
    /// Two concurrent refreshes are harmless — both mint a valid token and the
    /// second overwrites the first — so the lock is never held across the
    /// request, which would serialise every upload behind one HTTP round trip.
    async fn access_token(&self) -> Result<String> {
        let now = chrono::Utc::now();
        {
            let cached = self
                .cached
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(token) = cached.as_ref()
                && (token.expires_at - now).num_seconds() > TOKEN_REFRESH_MARGIN
            {
                return Ok(token.value.clone());
            }
        }

        let assertion = self.assertion(now)?;
        let body = format!(
            "grant_type={}&assertion={}",
            encode_query(JWT_BEARER_GRANT),
            encode_query(&assertion),
        );

        let response = client()
            .post(&self.token_uri)
            .header(
                http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|error| transport("gcs", error))?;
        let status = response.status();
        if !status.is_success() {
            let body = error_body(response).await;
            return Err(status_error("gcs", status, "", &body));
        }

        let token: OauthToken = response
            .json()
            .await
            .map_err(|error| transport("gcs", error))?;
        let expires_at = now + chrono::Duration::seconds(token.expires_in.unwrap_or(3600));

        {
            let mut cached = self
                .cached
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *cached = Some(CachedToken {
                value: token.access_token.clone(),
                expires_at,
            });
        }
        Ok(token.access_token)
    }

    /// The RFC 7523 assertion the token endpoint exchanges for a token.
    fn assertion(&self, now: chrono::DateTime<chrono::Utc>) -> Result<String> {
        let signing_input = jwt_signing_input(&self.client_email, &self.token_uri, now)?;
        let signature = self.sign(signing_input.as_bytes())?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature),
        ))
    }
}

/// The first two parts of the assertion: everything the signature covers.
///
/// Separate from [`ServiceAccount::assertion`] because it is the part that can
/// be *wrong* — a claim the token endpoint does not expect is a 400 with no
/// detail — and because it is pure, so a test can read it without a key.
#[cfg(feature = "gcs")]
fn jwt_signing_input(
    client_email: &str,
    token_uri: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<String> {
    /// The header is fixed, so it is a literal rather than a serialisation.
    const HEADER: &str = r#"{"alg":"RS256","typ":"JWT"}"#;

    let claims = serde_json::json!({
        "iss": client_email,
        "scope": GCS_SCOPE,
        "aud": token_uri,
        "iat": now.timestamp(),
        "exp": (now + chrono::Duration::hours(1)).timestamp(),
    });
    let claims = serde_json::to_vec(&claims).map_err(|error| {
        crate::Error::refused(
            "gcs",
            format!("the token assertion is not encodable: {error}"),
        )
    })?;

    Ok(format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(HEADER),
        URL_SAFE_NO_PAD.encode(claims),
    ))
}

/// Read a PEM private key into a `ring` key pair.
///
/// Google issues PKCS#8 (`BEGIN PRIVATE KEY`); a key converted by hand with
/// older OpenSSL tooling is PKCS#1 (`BEGIN RSA PRIVATE KEY`). Both are read,
/// because the second one is the shape somebody arrives with at three in the
/// morning and a refusal there is not a useful message.
#[cfg(feature = "gcs")]
fn load_rsa_key(pem: &str) -> Option<ring::signature::RsaKeyPair> {
    let der = STANDARD
        .decode(
            pem.lines()
                .filter(|line| !line.trim_start().starts_with("-----"))
                .flat_map(|line| line.chars().filter(|character| !character.is_whitespace()))
                .collect::<String>(),
        )
        .ok()?;

    if pem.contains("BEGIN RSA PRIVATE KEY") {
        ring::signature::RsaKeyPair::from_der(&der).ok()
    } else {
        ring::signature::RsaKeyPair::from_pkcs8(&der).ok()
    }
}

// ---------------------------------------------------------------------------
// Google Cloud Storage V4 signing
// ---------------------------------------------------------------------------

/// The V4 signing scheme, in the one form this crate needs.
///
/// The same shape as SigV4 — a canonical request, a string to sign, a scope —
/// with an RSA signature over the service account in place of the HMAC chain.
/// Google publishes the exact bytes; this reproduces them and signs with `ring`.
#[cfg(feature = "gcs")]
mod goog4 {
    use super::{ServiceAccount, encode_query};

    use crate::Result;

    /// The algorithm name, which appears twice: in the query and in the string
    /// to sign.
    const ALGORITHM: &str = "GOOG4-RSA-SHA256";

    /// The longest life Google accepts for a V4 signed URL.
    const MAX_EXPIRES: u64 = 7 * 24 * 3600;

    /// Everything about a V4 signed URL except the signature.
    ///
    /// Split out because this is the part that can be *wrong*, and because it
    /// is pure: a test reads the exact bytes Google will hash without needing a
    /// private key in the repository. The signature itself is `ring`'s.
    pub(super) struct Unsigned {
        /// The query string, minus `X-Goog-Signature`.
        pub query: String,
        /// The canonical request, which the string to sign hashes.
        pub canonical_request: String,
        /// The request timestamp, which appears in the query and the signature.
        pub timestamp: String,
        /// The credential scope, likewise.
        pub scope: String,
    }

    impl Unsigned {
        /// The exact bytes the RSA signature is taken over.
        pub(super) fn to_sign(&self) -> String {
            format!(
                "{ALGORITHM}\n{}\n{}\n{}",
                self.timestamp,
                self.scope,
                super::crate_sha256_hex(self.canonical_request.as_bytes()),
            )
        }
    }

    /// Build the signed query string for one request.
    ///
    /// `extra` is every header beyond `host` that the client will send. Google
    /// refuses a request carrying an `x-goog-*` header the signature did not
    /// cover, so a presigned upload's metadata has to arrive here rather than
    /// being appended to the URL afterwards.
    pub(super) fn presign(
        account: &ServiceAccount,
        method: &str,
        host: &str,
        path: &str,
        extra: &[(String, String)],
        ttl_seconds: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<String> {
        let unsigned = canonical(
            &account.client_email,
            method,
            host,
            path,
            extra,
            ttl_seconds,
            now,
        );
        let signature = super::hex(&account.sign(unsigned.to_sign().as_bytes())?);
        Ok(format!("{}&X-Goog-Signature={signature}", unsigned.query))
    }

    /// The canonical request, the string to sign, and the query they describe.
    pub(super) fn canonical(
        client_email: &str,
        method: &str,
        host: &str,
        path: &str,
        extra: &[(String, String)],
        ttl_seconds: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Unsigned {
        let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let scope = format!("{date}/auto/storage/goog4_request");

        // Canonical headers: lowercase, trimmed, sorted, one per line.
        let mut canonical: Vec<(String, String)> = extra
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        canonical.push(("host".to_owned(), host.to_owned()));
        canonical.sort();

        let signed_headers = canonical
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let header_block = canonical
            .iter()
            .map(|(name, value)| format!("{name}:{value}\n"))
            .collect::<String>();

        // Already in sorted order by encoded name — A, C, D, E, S — which is
        // what the canonical query string requires.
        let query = format!(
            "X-Goog-Algorithm={ALGORITHM}\
             &X-Goog-Credential={}\
             &X-Goog-Date={timestamp}\
             &X-Goog-Expires={}\
             &X-Goog-SignedHeaders={}",
            encode_query(&format!("{client_email}/{scope}")),
            ttl_seconds.min(MAX_EXPIRES),
            encode_query(&signed_headers),
        );

        let canonical_request = format!(
            "{method}\n{path}\n{query}\n{header_block}\n{signed_headers}\nUNSIGNED-PAYLOAD",
        );

        Unsigned {
            query,
            canonical_request,
            timestamp,
            scope,
        }
    }
}

// ---------------------------------------------------------------------------
// Azure
// ---------------------------------------------------------------------------

/// Azure Blob Storage.
///
/// ```no_run
/// # #[cfg(feature = "azure")] {
/// # use moso_core::config::SecretString;
/// # use moso_storage::backend::AzureStorage;
/// # fn f(key: SecretString) {
/// let _ = AzureStorage::new("account", "uploads", key);
/// # }
/// # }
/// ```
#[cfg(feature = "azure")]
#[cfg_attr(docsrs, doc(cfg(feature = "azure")))]
#[derive(Debug)]
pub struct AzureStorage {
    /// The storage account.
    account: String,
    /// The container.
    container: String,
    /// The account key, base64, redacted in every `Debug` and log.
    access_key: SecretString,
    /// A key prefix applied to every operation.
    prefix: Option<String>,
}

/// The Azure REST API version this backend speaks.
#[cfg(feature = "azure")]
const AZURE_VERSION: &str = "2021-12-02";

#[cfg(feature = "azure")]
impl AzureStorage {
    /// A backend for `container` in `account`.
    ///
    /// ```no_run
    /// # use moso_core::config::SecretString;
    /// # use moso_storage::backend::AzureStorage;
    /// # fn f(key: SecretString) { let _ = AzureStorage::new("a", "c", key); }
    /// ```
    #[must_use]
    pub fn new(
        account: impl Into<String>,
        container: impl Into<String>,
        access_key: SecretString,
    ) -> Self {
        Self {
            account: account.into(),
            container: container.into(),
            access_key,
            prefix: None,
        }
    }

    /// Prefix every key.
    ///
    /// ```no_run
    /// # use moso_core::config::SecretString;
    /// # use moso_storage::backend::AzureStorage;
    /// # fn f(key: SecretString) { let _ = AzureStorage::new("a", "c", key).prefix("shop"); }
    /// ```
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into().trim_matches('/').to_owned());
        self
    }

    /// The blob name, with the application prefix applied.
    fn scoped(&self, key: &str) -> String {
        match &self.prefix {
            Some(prefix) if !prefix.is_empty() => format!("{prefix}/{key}"),
            _ => key.to_owned(),
        }
    }

    /// Send one shared-key-signed request.
    async fn send(
        &self,
        method: &str,
        key: &str,
        query: &str,
        body: bytes::Bytes,
        extra: &[(&str, String)],
    ) -> Result<reqwest::Response> {
        let path = if key.is_empty() {
            format!("/{}", self.container)
        } else {
            format!("/{}/{}", self.container, encode_path(&self.scoped(key)))
        };

        let date = chrono::Utc::now().to_rfc2822().replace("+0000", "GMT");
        let mut headers: Vec<(String, String)> = extra
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect();
        headers.push(("x-ms-date".to_owned(), date));
        headers.push(("x-ms-version".to_owned(), AZURE_VERSION.to_owned()));
        if !body.is_empty() {
            headers.push(("content-length".to_owned(), body.len().to_string()));
        }

        let signature = self.sign(method, &path, query, &headers, body.len());
        let url = if query.is_empty() {
            format!("https://{}.blob.core.windows.net{path}", self.account)
        } else {
            format!(
                "https://{}.blob.core.windows.net{path}?{query}",
                self.account
            )
        };

        let mut request = client().request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET),
            &url,
        );
        for (name, value) in &headers {
            request = request.header(name, value);
        }
        request = request.header(
            http::header::AUTHORIZATION,
            format!("SharedKey {}:{signature}", self.account),
        );
        if !body.is_empty() {
            request = request.body(body);
        }

        request
            .send()
            .await
            .map_err(|error| transport("azure", error))
    }

    /// The shared-key signature: HMAC-SHA256 over a fixed canonical string.
    fn sign(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: &[(String, String)],
        content_length: usize,
    ) -> String {
        let header = |name: &str| {
            headers
                .iter()
                .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
                .unwrap_or_default()
        };

        // Every `x-ms-*` header, lowercased and sorted, one per line.
        let mut ms_headers: Vec<(String, String)> = headers
            .iter()
            .filter(|(name, _)| name.to_ascii_lowercase().starts_with("x-ms-"))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        ms_headers.sort();
        let canonical_headers = ms_headers
            .iter()
            .map(|(name, value)| format!("{name}:{value}\n"))
            .collect::<String>();

        // The canonical resource: the account, the path, then every query
        // parameter, lowercased and sorted.
        let mut resource = format!("/{}{path}", self.account);
        if !query.is_empty() {
            let mut pairs: Vec<(String, String)> = query
                .split('&')
                .map(|pair| {
                    let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                    (name.to_ascii_lowercase(), value.to_owned())
                })
                .collect();
            pairs.sort();
            for (name, value) in pairs {
                resource.push_str(&format!("\n{name}:{value}"));
            }
        }

        let length = if content_length == 0 {
            String::new()
        } else {
            content_length.to_string()
        };

        let to_sign = format!(
            "{method}\n\n\n{length}\n\n{}\n\n\n\n\n\n\n{canonical_headers}{resource}",
            header("content-type"),
        );

        let Ok(key) = self.account_key() else {
            // A key that is not base64 cannot sign anything; the request will
            // be refused with a 403 that names the account, which is the right
            // failure for a misconfigured credential.
            return String::new();
        };
        STANDARD.encode(hmac_sha256(&key, to_sign.as_bytes()))
    }

    /// The account key, decoded.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) when it is not base64. The
    /// shared-key path fails closed with an empty signature instead — a request
    /// path cannot return a configuration error — but a SAS is minted before
    /// anything is sent, so there it is worth saying plainly.
    fn account_key(&self) -> Result<Vec<u8>> {
        STANDARD.decode(self.access_key.expose()).map_err(|_| {
            crate::Error::config(
                "`storage.secret_key` is not a base64 Azure account key — copy it from the \
                 storage account's `Access keys` blade, which is where the base64 form is",
            )
        })
    }

    /// A service SAS for one blob: the query string a client appends.
    ///
    /// The signature is HMAC-SHA256 over a canonical string Microsoft fixes
    /// exactly — sixteen fields, newline-separated, several of them empty
    /// because this crate uses neither a stored access policy nor an IP
    /// restriction nor an encryption scope. Getting one field wrong is a 403
    /// with no detail, so the fields are named in the code rather than counted.
    fn service_sas(
        &self,
        key: &StorageKey,
        permissions: &str,
        ttl: std::time::Duration,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<String> {
        /// HTTPS only. A SAS that also worked over plain HTTP is a bearer
        /// token in a URL travelling in clear text.
        const PROTOCOL: &str = "https";
        /// `b` — this SAS names one blob, not a container.
        const RESOURCE: &str = "b";

        let expiry = (now + chrono::Duration::seconds(ttl.as_secs().max(1) as i64))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        // Unencoded, which is what the canonical resource wants.
        let resource = format!(
            "/blob/{}/{}/{}",
            self.account,
            self.container,
            self.scoped(key.as_str()),
        );

        // `st` is left empty on purpose: a start time means a clock a few
        // seconds fast rejects a URL that was minted a moment ago.
        let to_sign = [
            permissions,       // sp
            "",                // st
            expiry.as_str(),   // se
            resource.as_str(), // canonicalised resource
            "",                // si — no stored access policy
            "",                // sip — no IP restriction
            PROTOCOL,          // spr
            AZURE_VERSION,     // sv
            RESOURCE,          // sr
            "",                // snapshot time
            "",                // ses — no encryption scope
            "",                // rscc — none of the five response-header
            "",                // rscd    overrides are used, but every one of
            "",                // rsce    them is a field in the string and
            "",                // rscl    dropping it shifts everything after it
            "",                // rsct
        ]
        .join("\n");

        let signature = STANDARD.encode(hmac_sha256(&self.account_key()?, to_sign.as_bytes()));
        Ok(format!(
            "sv={}&sr={RESOURCE}&sp={}&se={}&spr={PROTOCOL}&sig={}",
            encode_query(AZURE_VERSION),
            encode_query(permissions),
            encode_query(&expiry),
            encode_query(&signature),
        ))
    }

    /// The URL of one blob, without a query string.
    fn blob_url(&self, key: &StorageKey) -> String {
        format!(
            "https://{}.blob.core.windows.net/{}/{}",
            self.account,
            self.container,
            encode_path(&self.scoped(key.as_str())),
        )
    }
}

/// The permissions a read-only SAS carries.
///
/// Azure requires the letters in its own documented order — `racwdxltmeop` —
/// and silently produces a signature nobody can use when they are out of it.
#[cfg(feature = "azure")]
const SAS_READ: &str = "r";

/// The permissions an upload SAS carries: create, then write.
#[cfg(feature = "azure")]
const SAS_CREATE_WRITE: &str = "cw";

#[cfg(feature = "azure")]
impl Storage for AzureStorage {
    fn name(&self) -> &'static str {
        "azure"
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            ranges: true,
            server_side_copy: true,
            metadata: true,
            delimited_listing: true,
            conditional_writes: true,
            public_objects: false,
            max_object_size: 190 * 1024 * 1024 * 1024 * 1024,
            // A service SAS, signed with the same account key the requests use.
            // Reported unconditionally rather than by test-decoding the key: a
            // key that is not base64 is a misconfiguration, and an error naming
            // it is more use than a capability quietly turning false.
            signed_urls: true,
            presigned_upload: true,
            multipart: false,
            min_part_size: 0,
        }
    }

    fn put<'a>(
        &'a self,
        key: &'a StorageKey,
        body: ByteStream,
        mut opts: PutOpts,
    ) -> BoxFuture<'a, Result<ObjectMeta>> {
        Box::pin(async move {
            let bytes = crate::collect_bounded(body, MAX_AZURE_PUT, "azure").await?;
            if opts.sniffs()
                && let Some(sniffed) = crate::sniff(&bytes)
            {
                opts.set_content_type(sniffed);
            }

            let digest = crate::backend::sha256_hex(&bytes);
            if let Some(expected) = opts.expected_checksum()
                && expected.digest() != digest
            {
                return Err(crate::Error::Checksum {
                    key: key.to_string(),
                    expected: expected.digest().to_owned(),
                    actual: digest,
                });
            }

            let mut extra = vec![
                ("x-ms-blob-type", "BlockBlob".to_owned()),
                ("content-type", opts.content_type().to_owned()),
            ];
            if opts.refuses_overwrite() {
                extra.push(("if-none-match", "*".to_owned()));
            }
            let owned: Vec<(String, String)> = opts
                .metadata_pairs()
                .iter()
                .map(|(name, value)| (format!("x-ms-meta-{name}"), value.clone()))
                .collect();
            let mut extra: Vec<(&str, String)> = extra;
            extra.extend(
                owned
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.clone())),
            );

            let size = bytes.len() as u64;
            let response = self.send("PUT", key.as_str(), "", bytes, &extra).await?;
            let status = response.status();
            let etag = response
                .headers()
                .get(http::header::ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("azure", status, key.as_str(), &body));
            }

            Ok(crate::object::meta_from(
                key,
                size,
                &opts,
                Some(crate::Checksum::sha256(digest)),
                etag,
            ))
        })
    }

    fn get<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<ByteStream>> {
        Box::pin(async move {
            let response = self
                .send("GET", key.as_str(), "", bytes::Bytes::new(), &[])
                .await?;
            let status = response.status();
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("azure", status, key.as_str(), &body));
            }
            Ok(response_stream("azure", response))
        })
    }

    fn get_range<'a>(
        &'a self,
        key: &'a StorageKey,
        range: Range<u64>,
    ) -> BoxFuture<'a, Result<ByteStream>> {
        Box::pin(async move {
            let last = range.end.saturating_sub(1).max(range.start);
            let response = self
                .send(
                    "GET",
                    key.as_str(),
                    "",
                    bytes::Bytes::new(),
                    &[("x-ms-range", format!("bytes={}-{last}", range.start))],
                )
                .await?;
            let status = response.status();
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("azure", status, key.as_str(), &body));
            }
            Ok(response_stream("azure", response))
        })
    }

    fn head<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<Option<ObjectMeta>>> {
        Box::pin(async move {
            let response = self
                .send("HEAD", key.as_str(), "", bytes::Bytes::new(), &[])
                .await?;
            let status = response.status();
            if status == http::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if !status.is_success() {
                return Err(status_error("azure", status, key.as_str(), ""));
            }
            Ok(Some(meta_from_headers(key, response.headers())))
        })
    }

    fn delete<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let response = self
                .send("DELETE", key.as_str(), "", bytes::Bytes::new(), &[])
                .await?;
            let status = response.status();
            if status == http::StatusCode::NOT_FOUND {
                return Ok(false);
            }
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("azure", status, key.as_str(), &body));
            }
            Ok(true)
        })
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        cursor: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            let mut query = format!(
                "restype=container&comp=list&prefix={}",
                encode_query(&self.scoped(prefix)),
            );
            if let Some(cursor) = cursor {
                query.push_str(&format!("&marker={}", encode_query(cursor)));
            }

            let response = self
                .send("GET", "", &query, bytes::Bytes::new(), &[])
                .await?;
            let status = response.status();
            let document = response.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(status_error("azure", status, prefix, &document));
            }

            let mut objects = Vec::new();
            for block in document.split("<Blob>").skip(1) {
                let block = block.split("</Blob>").next().unwrap_or_default();
                let Some(raw) = element(block, "Name") else {
                    continue;
                };
                let unscoped = self
                    .prefix
                    .as_deref()
                    .and_then(|prefix| raw.strip_prefix(&format!("{prefix}/")))
                    .unwrap_or(&raw);
                let Ok(key) = StorageKey::new(unscoped.to_owned()) else {
                    continue;
                };

                objects.push(ObjectMeta {
                    key,
                    size: element(block, "Content-Length")
                        .and_then(|value| value.parse().ok())
                        .unwrap_or_default(),
                    content_type: element(block, "Content-Type")
                        .unwrap_or_else(|| "application/octet-stream".to_owned()),
                    etag: element(block, "Etag"),
                    modified_at: element(block, "Last-Modified")
                        .and_then(|value| chrono::DateTime::parse_from_rfc2822(&value).ok())
                        .map(|value| value.with_timezone(&chrono::Utc)),
                    checksum: None,
                    metadata: std::collections::BTreeMap::new(),
                    cache_control: None,
                    content_disposition: None,
                    public: false,
                });
            }

            Ok(Listing {
                objects,
                prefixes: Vec::new(),
                cursor: element(&document, "NextMarker").filter(|value| !value.is_empty()),
            })
        })
    }

    fn copy<'a>(
        &'a self,
        from: &'a StorageKey,
        to: &'a StorageKey,
    ) -> BoxFuture<'a, Result<ObjectMeta>> {
        Box::pin(async move {
            let source = format!(
                "https://{}.blob.core.windows.net/{}/{}",
                self.account,
                self.container,
                encode_path(&self.scoped(from.as_str())),
            );
            let response = self
                .send(
                    "PUT",
                    to.as_str(),
                    "",
                    bytes::Bytes::new(),
                    &[("x-ms-copy-source", source)],
                )
                .await?;
            let status = response.status();
            if !status.is_success() {
                let body = error_body(response).await;
                return Err(status_error("azure", status, from.as_str(), &body));
            }
            self.head(to)
                .await?
                .ok_or_else(|| crate::Error::not_found(to.as_str()))
        })
    }

    fn signed_url<'a>(
        &'a self,
        key: &'a StorageKey,
        ttl: std::time::Duration,
    ) -> BoxFuture<'a, Result<Url>> {
        Box::pin(async move {
            let query = self.service_sas(key, SAS_READ, ttl, chrono::Utc::now())?;
            Url::parse_http(&format!("{}?{query}", self.blob_url(key)))
                .map_err(|error| crate::Error::config(error.message().to_owned()))
        })
    }

    fn presigned_upload<'a>(
        &'a self,
        key: &'a StorageKey,
        policy: crate::UploadPolicy,
    ) -> BoxFuture<'a, Result<crate::PresignedPost>> {
        Box::pin(async move {
            let query =
                self.service_sas(key, SAS_CREATE_WRITE, policy.ttl(), chrono::Utc::now())?;

            // `x-ms-blob-type` is not optional: a `Put Blob` without it is a 400,
            // and a client that has only been handed a URL has no way to know.
            let mut fields = vec![("x-ms-blob-type".to_owned(), "BlockBlob".to_owned())];
            for (name, value) in policy.metadata_pairs() {
                fields.push((format!("x-ms-meta-{name}"), value.clone()));
            }
            for (name, value) in policy.fields() {
                fields.push((name.clone(), value.clone()));
            }

            Ok(crate::PresignedPost {
                url: Url::parse_http(&format!("{}?{query}", self.blob_url(key)))
                    .map_err(|error| crate::Error::config(error.message().to_owned()))?,
                method: "PUT".to_owned(),
                fields,
                key: key.clone(),
                expires_at: chrono::Utc::now()
                    + chrono::Duration::seconds(policy.ttl().as_secs() as i64),
                max_size: policy.max_size(),
            })
        })
    }

    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let response = self
                .send(
                    "GET",
                    "",
                    "restype=container&comp=list&maxresults=1",
                    bytes::Bytes::new(),
                    &[],
                )
                .await?;
            let status = response.status();
            if status.is_success() {
                return Ok(());
            }
            let body = error_body(response).await;
            Err(status_error("azure", status, "", &body))
        })
    }
}

/// The largest object the Azure backend writes in one request.
///
/// Azure's own `Put Blob` limit for a block blob written in one go.
#[cfg(feature = "azure")]
pub const MAX_AZURE_PUT: u64 = 256 * 1024 * 1024;

/// The text of the first `<tag>` in a document.
#[cfg(feature = "azure")]
fn element(document: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = document.find(&open)? + open.len();
    let end = document[start..].find(&close)? + start;
    Some(
        document[start..end]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    /// The `sig=` parameter of a query string, for comparing two signatures
    /// without comparing the timestamps around them.
    #[cfg(feature = "azure")]
    fn signature_of(query: &str) -> &str {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix("sig="))
            .unwrap_or_default()
    }

    /// A key with a space or a `+` in it changes the meaning of a URL unless
    /// it is encoded, and the separator must survive.
    #[test]
    fn a_key_is_percent_encoded_segment_by_segment() {
        assert_eq!(encode_path("a/b c.txt"), "a/b%20c.txt");
        assert_eq!(encode_path("a/b+c"), "a/b%2Bc");
        assert_eq!(encode_path("a/résumé.pdf"), "a/r%C3%A9sum%C3%A9.pdf");
        // The unreserved set is left readable.
        assert_eq!(encode_path("a/b-c_d.e~f"), "a/b-c_d.e~f");
    }

    /// A 404 is not found, a 429 and a 5xx are retryable, and a 403 is not.
    /// Getting this wrong means either a lost object or a retry storm.
    #[test]
    fn the_status_classification_decides_retries_correctly() {
        assert!(status_error("s3", http::StatusCode::NOT_FOUND, "k", "").is_not_found());
        assert!(status_error("s3", http::StatusCode::TOO_MANY_REQUESTS, "k", "").retryable());
        assert!(status_error("s3", http::StatusCode::BAD_GATEWAY, "k", "").retryable());
        assert!(!status_error("s3", http::StatusCode::FORBIDDEN, "k", "").retryable());
        assert!(!status_error("s3", http::StatusCode::BAD_REQUEST, "k", "").retryable());
    }

    /// The provider's metadata headers are three different spellings of the
    /// same idea, and all three have to arrive as `ObjectMeta::metadata`.
    #[test]
    fn every_providers_metadata_prefix_is_understood() {
        let mut headers = http::HeaderMap::new();
        headers.insert("content-length", http::HeaderValue::from_static("42"));
        headers.insert("content-type", http::HeaderValue::from_static("image/png"));
        headers.insert("x-amz-meta-owner", http::HeaderValue::from_static("usr_1"));
        headers.insert("x-goog-meta-team", http::HeaderValue::from_static("shop"));
        headers.insert("x-ms-meta-tier", http::HeaderValue::from_static("hot"));

        let key = StorageKey::new("a/b.png").expect("valid");
        let meta = meta_from_headers(&key, &headers);
        assert_eq!(meta.size, 42);
        assert_eq!(meta.content_type, "image/png");
        assert_eq!(
            meta.metadata.get("owner").map(String::as_str),
            Some("usr_1")
        );
        assert_eq!(meta.metadata.get("team").map(String::as_str), Some("shop"));
        assert_eq!(meta.metadata.get("tier").map(String::as_str), Some("hot"));
    }

    /// A custom endpoint means a self-hosted gateway, and every one of those
    /// wants path addressing. Getting it wrong is a DNS error nobody can read.
    #[cfg(feature = "s3")]
    #[test]
    fn an_endpoint_switches_to_path_addressing_and_builds_the_right_target() {
        let aws = S3Storage::new("uploads", "eu-central-1", "AK", SecretString::new("SK"));
        assert_eq!(
            aws.target("a/b.png"),
            (
                "uploads.s3.eu-central-1.amazonaws.com".to_owned(),
                "/a/b.png".to_owned(),
            ),
        );

        let minio = S3Storage::new("uploads", "auto", "AK", SecretString::new("SK"))
            .endpoint("http://127.0.0.1:9000");
        assert_eq!(minio.addressing, AddressingStyle::Path);
        assert_eq!(minio.scheme(), "http");
        assert_eq!(
            minio.target("a/b.png"),
            ("127.0.0.1:9000".to_owned(), "/uploads/a/b.png".to_owned()),
        );
    }

    /// One bucket hosting several applications must not let one read another's
    /// objects, which is the whole point of the prefix.
    #[cfg(feature = "s3")]
    #[test]
    fn the_application_prefix_scopes_every_key() {
        let storage =
            S3Storage::new("shared", "auto", "AK", SecretString::new("SK")).prefix("/shop/");
        assert_eq!(storage.scoped("a/b"), "shop/a/b");
        assert_eq!(storage.target("a/b").1, "/shop/a/b");
    }

    /// A presigned URL that is not reproducible cannot be tested, and one that
    /// is missing a parameter is rejected on sight.
    #[cfg(feature = "s3")]
    #[test]
    fn a_presigned_url_carries_every_parameter_s3_requires() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("valid")
            .with_timezone(&chrono::Utc);
        let query = sigv4::presign(
            "GET",
            "uploads.s3.eu-central-1.amazonaws.com",
            "/a/b.png",
            "s3",
            "eu-central-1",
            "AKIDEXAMPLE",
            "secret",
            300,
            now,
        );

        for parameter in [
            "X-Amz-Algorithm=AWS4-HMAC-SHA256",
            "X-Amz-Credential=",
            "X-Amz-Date=20260101T000000Z",
            "X-Amz-Expires=300",
            "X-Amz-SignedHeaders=host",
            "X-Amz-Signature=",
        ] {
            assert!(
                query.contains(parameter),
                "{parameter} missing from {query}"
            );
        }

        // Reproducible for a fixed instant, or a retry is a new request.
        let again = sigv4::presign(
            "GET",
            "uploads.s3.eu-central-1.amazonaws.com",
            "/a/b.png",
            "s3",
            "eu-central-1",
            "AKIDEXAMPLE",
            "secret",
            300,
            now,
        );
        assert_eq!(query, again);
    }

    /// A signature that does not cover the path would let a URL for one object
    /// fetch another.
    #[cfg(feature = "s3")]
    #[test]
    fn a_presigned_signature_covers_the_path_the_method_and_the_expiry() {
        let now = chrono::Utc::now();
        let of = |method: &str, path: &str, ttl: u64| {
            sigv4::presign(method, "h", path, "s3", "r", "AK", "SK", ttl, now)
        };
        assert_ne!(of("GET", "/a", 300), of("GET", "/b", 300));
        assert_ne!(of("GET", "/a", 300), of("PUT", "/a", 300));
        assert_ne!(of("GET", "/a", 300), of("GET", "/a", 600));
    }

    /// AWS caps a presigned URL at seven days; asking for more produces one
    /// that is rejected on sight, so it is clamped here.
    #[cfg(feature = "s3")]
    #[test]
    fn a_presigned_expiry_is_clamped_to_the_documented_maximum() {
        let now = chrono::Utc::now();
        let query = sigv4::presign("GET", "h", "/a", "s3", "r", "AK", "SK", 30 * 24 * 3600, now);
        assert!(query.contains("X-Amz-Expires=604800"), "{query}");
    }

    /// The header signature has to cover the payload hash, or a proxy could
    /// swap the body.
    #[cfg(feature = "s3")]
    #[test]
    fn the_header_signature_covers_the_payload() {
        let now = chrono::Utc::now();
        let of = |payload: &str| {
            let mut extra = Vec::new();
            sigv4::sign(
                "PUT", "h", "/a", "", payload, &mut extra, "s3", "r", "AK", "SK", now,
            )
            .into_iter()
            .find(|(name, _)| name == "authorization")
            .expect("present")
            .1
        };
        assert_ne!(of("aaa"), of("bbb"));
    }

    /// A JSON key that is missing what signing needs fails at construction,
    /// next to the configuration that is wrong, rather than at the first
    /// presigned upload three hours later.
    #[cfg(feature = "gcs")]
    #[test]
    fn a_malformed_service_account_key_fails_at_construction() {
        for (json, expected) in [
            (r#"{"type":"service_account"}"#, "client_email"),
            (
                r#"{"type":"service_account","client_email":"a@b.iam.gserviceaccount.com"}"#,
                "private_key",
            ),
            (
                r#"{"client_email":"a@b.iam.gserviceaccount.com","private_key":"not a pem"}"#,
                "2048 bits",
            ),
            (r#"{"#, "valid JSON"),
        ] {
            let error = GcsStorage::new("uploads", SecretString::new(json))
                .expect_err("the key cannot sign");
            let text = error.to_string();
            assert!(text.contains(expected), "expected `{expected}` in: {text}");
        }
    }

    /// The three credential shapes, and what each one can do. A bearer token
    /// holds no private key, so claiming it could sign would hand out URLs that
    /// 403 at the browser.
    #[cfg(feature = "gcs")]
    #[test]
    fn only_a_service_account_key_claims_to_sign() {
        let token = GcsStorage::new("uploads", SecretString::new("ya29.token")).expect("accepted");
        assert!(matches!(token.credential, GcsCredential::Token(_)));
        assert!(!token.capabilities().signed_urls);
        assert!(!token.capabilities().presigned_upload);

        let metadata = GcsStorage::new("uploads", SecretString::new("metadata")).expect("accepted");
        assert!(matches!(metadata.credential, GcsCredential::Metadata));
        assert!(!metadata.capabilities().signed_urls);
        assert!(!metadata.capabilities().presigned_upload);
    }

    /// A bearer credential cannot sign, and the error has to say what to
    /// configure instead of failing at the browser.
    #[cfg(feature = "gcs")]
    #[tokio::test]
    async fn a_gcs_bearer_credential_refuses_to_sign_with_an_actionable_error() {
        let storage =
            GcsStorage::new("uploads", SecretString::new("ya29.token")).expect("accepted");
        let error = storage
            .signed_url(
                &StorageKey::new("a/b.png").expect("valid"),
                std::time::Duration::from_secs(60),
            )
            .await
            .expect_err("no private key");
        let text = error.to_string();
        assert!(text.contains("service-account"), "{text}");
        assert!(text.contains("STORAGE_SECRET_KEY"), "{text}");
    }

    /// A signed URL points at the XML API, one path segment per key segment,
    /// with the application prefix applied — the same scoping every other
    /// operation uses, or a URL would reach a different object.
    #[cfg(feature = "gcs")]
    #[test]
    fn a_gcs_signed_url_points_at_the_scoped_object() {
        let storage = GcsStorage::new("shared", SecretString::new("ya29.token"))
            .expect("accepted")
            .prefix("/shop/");
        assert_eq!(
            storage.object_path(&StorageKey::new("a/b c.png").expect("valid")),
            "/shared/shop/a/b%20c.png",
        );
    }

    /// The canonical request is what Google hashes; every field in it is a
    /// field that, if wrong, produces a 403 with no detail. The signature over
    /// it is `ring`'s, so what is checked here is the exact bytes.
    #[cfg(feature = "gcs")]
    #[test]
    fn the_gcs_canonical_request_is_the_documented_one() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("valid")
            .with_timezone(&chrono::Utc);
        let unsigned = goog4::canonical(
            "moso@example.iam.gserviceaccount.com",
            "GET",
            GCS_HOST,
            "/uploads/a/b.png",
            &[],
            300,
            now,
        );

        assert_eq!(
            unsigned.query,
            "X-Goog-Algorithm=GOOG4-RSA-SHA256\
             &X-Goog-Credential=moso%40example.iam.gserviceaccount.com%2F20260101%2Fauto%2F\
             storage%2Fgoog4_request\
             &X-Goog-Date=20260101T000000Z\
             &X-Goog-Expires=300\
             &X-Goog-SignedHeaders=host",
        );
        assert_eq!(
            unsigned.canonical_request,
            format!(
                "GET\n/uploads/a/b.png\n{}\nhost:storage.googleapis.com\n\nhost\n\
                 UNSIGNED-PAYLOAD",
                unsigned.query,
            ),
        );
        assert_eq!(
            unsigned.to_sign(),
            format!(
                "GOOG4-RSA-SHA256\n20260101T000000Z\n20260101/auto/storage/goog4_request\n{}",
                crate::backend::sha256_hex(unsigned.canonical_request.as_bytes()),
            ),
        );
    }

    /// Google refuses a request carrying an `x-goog-*` header the signature did
    /// not cover, so a presigned upload's metadata has to be *in* the canonical
    /// headers, lowercased and sorted.
    #[cfg(feature = "gcs")]
    #[test]
    fn a_gcs_presigned_upload_signs_the_headers_the_client_will_send() {
        let now = chrono::Utc::now();
        let unsigned = goog4::canonical(
            "moso@example.iam.gserviceaccount.com",
            "PUT",
            GCS_HOST,
            "/uploads/direct.png",
            &[
                ("x-goog-meta-uploaded-by".to_owned(), "usr_1".to_owned()),
                ("x-goog-acl".to_owned(), "public-read".to_owned()),
            ],
            600,
            now,
        );

        assert!(
            unsigned
                .query
                .contains("X-Goog-SignedHeaders=host%3Bx-goog-acl%3Bx-goog-meta-uploaded-by"),
            "{}",
            unsigned.query,
        );
        assert!(
            unsigned.canonical_request.contains(
                "host:storage.googleapis.com\nx-goog-acl:public-read\n\
                 x-goog-meta-uploaded-by:usr_1\n",
            ),
            "{}",
            unsigned.canonical_request,
        );
        assert!(unsigned.canonical_request.starts_with("PUT\n"));
    }

    /// A signature that does not cover the object, the method or the expiry
    /// would let one URL reach a different file.
    #[cfg(feature = "gcs")]
    #[test]
    fn a_gcs_signature_covers_the_object_the_method_and_the_expiry() {
        let now = chrono::Utc::now();
        let of = |method: &str, path: &str, ttl: u64| {
            goog4::canonical("a@b.example", method, GCS_HOST, path, &[], ttl, now).to_sign()
        };
        assert_ne!(of("GET", "/b/a.png", 300), of("GET", "/b/c.png", 300));
        assert_ne!(of("GET", "/b/a.png", 300), of("PUT", "/b/a.png", 300));
        assert_ne!(of("GET", "/b/a.png", 300), of("GET", "/b/a.png", 600));
    }

    /// Google's cap is seven days; asking for more produces a URL rejected on
    /// sight, so it is clamped here.
    #[cfg(feature = "gcs")]
    #[test]
    fn a_gcs_signed_expiry_is_clamped_to_the_documented_maximum() {
        let unsigned = goog4::canonical(
            "a@b.example",
            "GET",
            GCS_HOST,
            "/b/a.png",
            &[],
            30 * 24 * 3600,
            chrono::Utc::now(),
        );
        assert!(
            unsigned.query.contains("X-Goog-Expires=604800"),
            "{}",
            unsigned.query,
        );
    }

    /// The token endpoint verifies the assertion; a claim it does not expect is
    /// a 400 with no detail, so the two signed parts are checked exactly.
    #[cfg(feature = "gcs")]
    #[test]
    fn a_token_assertion_carries_the_claims_google_requires() {
        let now = chrono::Utc::now();
        let input = jwt_signing_input(
            "moso@example.iam.gserviceaccount.com",
            GOOGLE_TOKEN_URI,
            now,
        )
        .expect("encodes");

        let parts: Vec<&str> = input.split('.').collect();
        assert_eq!(parts.len(), 2, "the signature is appended, not built here");

        let header = URL_SAFE_NO_PAD.decode(parts[0]).expect("base64url");
        assert_eq!(header, br#"{"alg":"RS256","typ":"JWT"}"#);

        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).expect("base64url"))
                .expect("json");
        assert_eq!(claims["iss"], "moso@example.iam.gserviceaccount.com");
        assert_eq!(claims["aud"], GOOGLE_TOKEN_URI);
        assert_eq!(claims["scope"], GCS_SCOPE);
        assert_eq!(claims["iat"].as_i64(), Some(now.timestamp()));
        assert_eq!(claims["exp"].as_i64(), Some(now.timestamp() + 3600));
    }

    /// A PEM that is not a key `ring` will sign with has to be refused where the
    /// configuration is, not at the first upload.
    #[cfg(feature = "gcs")]
    #[test]
    fn a_private_key_that_ring_will_not_take_is_refused() {
        assert!(
            load_rsa_key("-----BEGIN PRIVATE KEY-----\nZm9v\n-----END PRIVATE KEY-----").is_none()
        );
        assert!(load_rsa_key("not a pem at all").is_none());
        assert!(
            load_rsa_key("-----BEGIN RSA PRIVATE KEY-----\n!!!\n-----END RSA PRIVATE KEY-----")
                .is_none(),
        );
    }

    /// Decode a lowercase-hex string, the inverse of [`super::hex`]. Used to
    /// recover a signature from a signed query string so it can be handed back
    /// to `ring` for verification.
    #[cfg(feature = "gcs")]
    fn from_hex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("valid hex"))
            .collect()
    }

    /// The one GCS test that actually *runs* an RSA signature. Every other GCS
    /// test above checks a canonical string with no key in hand; this one builds
    /// a [`GcsStorage`] from a real (throwaway) service-account key and drives
    /// `RsaKeyPair::sign` through all three paths that use it — the V4 signed
    /// URL, the presigned upload, and the RFC 7523 assertion that mints an
    /// access token — asserting each signature round-trips against the public
    /// half of the same key.
    ///
    /// It needs no network and no GCP: only the signing and URL-construction
    /// path runs, so it executes everywhere rather than skipping the way a
    /// backend test that talks to a real service must.
    #[cfg(feature = "gcs")]
    #[tokio::test]
    async fn a_real_service_account_key_signs_urls_and_tokens_that_verify() {
        // A throwaway 2048-bit RSA private key (PKCS#8 PEM), generated only for
        // this test. It is NOT a credential: it authenticates to nothing, was
        // never issued by Google, guards no account and unlocks no object. It
        // exists so the RSA signing path has a real key to run against offline,
        // and it may be regenerated with `openssl genpkey -algorithm RSA
        // -pkeyopt rsa_keygen_bits:2048` at any time.
        const THROWAWAY_TEST_KEY_PEM: &str = "\
            -----BEGIN PRIVATE KEY-----\n\
            MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCjUD5pPaiUtAR7\n\
            1ygjwyqy96TLAbVUp2Dq2EIUQ8zFIhe46BfNSyfGwCO0Za2vVdgP/wVXsFUz4vSC\n\
            4zU/feKLzrNaXnnvrAs8xgMFZZl/A2LJIIcS0744Gdoih/goBq+Ki3Ep1SIQwSJn\n\
            eANnn+jfgpPp/EwMIk7WTqN/NED8l5Vt72Dv+/+bjnGnqa/mZsOktHHGJuFxqMm+\n\
            J3S/tfSQ/pF9svQpaHbsOPi2wlN21kTWvB8j9NjoZ5xsKOLWwl9W4gDZZNmUZSKI\n\
            T4OH/rc35sYPRaXCKKLD2dXKccGmwjzajof8VF3IhqT6y65YDEz3J1dVqGwAOzlu\n\
            MNdz8NlNAgMBAAECggEAAosC1cewgtREx5rjlJ764LuLdN/Lb4yFrVJ9wOwHWcB8\n\
            pxPyHu+/KFCgnbQBntvS1/jsH9/ui7bKgOlB0IHIz82BrHQRKQLAUAtzS5e36qrm\n\
            VGRtxgTHDv+UDnqYiiMEg79FHVYkyCcBvqO3RdtPGH/jhr63fm7gVGT4Fch+BJDf\n\
            UdeF37fzv9KG1EKsuZywMuHATFp4sZwDM+9MIpVlzGfNQvNyKDS6TKoUIuVAIE57\n\
            UvyMgBKXUhr0YJCvsLUjU0MyKBZE9N+oeMLHVuZ6DfkKHLBwNhCzIMFFKqyYx3y5\n\
            Jcpj8uyW7psLMeonDHSaL/sHzse/9cg6SacOHYje8QKBgQDXlzAzYwldGXopAIOz\n\
            nO5WpYWZVGW/zPZqpGnElEWZWf4bub5EvWK8kT6gIcnLOPZvuVMRGoTWMRDWKkc+\n\
            Dao4YqpKnoGePQ85z+wV/GelW90TIkpu/UGHPmlQEWTh6YFo7gtKZqJNrWFJZkKd\n\
            ONMSNeBdKLtrI0f4QYgK9n/o2QKBgQDB7JxhSsPbon78Ufs3yy95RW+Fgn6EVeAz\n\
            NrcF3gqJrECjO5AQdzblBzjcCaXehbfJtXY6aH1VMvFOl+javxpm0njUYys3itun\n\
            pKId+oPj9xtc8x1bBGRcwWuGXSudngVXPoiS7DtRZ8n4YGU0FuhKygCqMw6u8zq7\n\
            PyMGSkULlQKBgH7NC6qNq2o4m+MVzGCOApivzf4654WB9cUPYq4eTzk89vozqzce\n\
            9L3X56+jb965aCiaJcM/h7W7Mh3ky/Yxb1auoV42ECKT4yqroj3kMMnPWB3y4ziY\n\
            eDwldyeCs4U0I8slhzqBVyC8wyW6oZ97Vpm1Wnswg9sl6ySW1n8sMFsxAoGBAJVV\n\
            Als9erNQX58n9l9RnP4zBRz3jzuS8bIeaTQgd1brCV9px5eWZfRZ6mQvHcbMi+nN\n\
            TfzOZ+1K7F2MR2jjjo4td5R9xVLhICLpeVnChvvuVujt4eYr7Kks3QM8DhEzFYPI\n\
            iN0zAr6+QN5+RJCnLzwgcACgjqcUcF6u0ObQHHk9AoGBAMBgHhJc92voSvoeLEsa\n\
            bZim63P8WAyeyi06RtTjBwFsKZfAmPSnfMqNGBgZRqeUBXmeodlb9ZSkcuM7UxE+\n\
            V4uSIHWhSLXt0DJQWDYdu0XXibKhqABPvmzBcKI2c79MGcHLlcysxbvXTAd3Y0ch\n\
            Gx12UyZxi/NFdKjkxvdSZ2W0\n\
            -----END PRIVATE KEY-----\n";

        /// The signing identity, matching the shape of a real one so the claims
        /// look exactly like production's.
        const EMAIL: &str = "moso-test@throwaway.iam.gserviceaccount.com";

        let json = serde_json::json!({
            "type": "service_account",
            "client_email": EMAIL,
            "private_key": THROWAWAY_TEST_KEY_PEM,
            "token_uri": GOOGLE_TOKEN_URI,
        })
        .to_string();

        let storage =
            GcsStorage::new("uploads", SecretString::new(json)).expect("the throwaway key loads");

        // A service-account key is the only credential that holds a private key,
        // so it is the only one that claims to sign.
        assert!(storage.capabilities().signed_urls);
        assert!(storage.capabilities().presigned_upload);

        let account = storage
            .service_account()
            .expect("a service-account key was configured");

        // The public half of the same key verifies every signature below.
        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            account.key.public().as_ref(),
        );

        // 1. `RsaKeyPair::sign` round-trips: the signature is well-formed and
        //    bound to its message.
        let message = b"moso-storage signs this exact byte string";
        let signature = account.sign(message).expect("the key signs");
        assert_eq!(
            signature.len(),
            account.key.public().modulus_len(),
            "a 2048-bit modulus produces a 256-byte signature",
        );
        public
            .verify(message, &signature)
            .expect("the signature verifies against the public key");
        public
            .verify(b"a different message", &signature)
            .expect_err("a signature is bound to the message it covers");

        // 2. The V4 signed URL carries a real RSA signature over the canonical
        //    request, and the public entry point builds a well-formed URL.
        let now = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("valid")
            .with_timezone(&chrono::Utc);
        let path = "/uploads/a/b.png";
        let query =
            goog4::presign(account, "GET", GCS_HOST, path, &[], 300, now).expect("presigns");
        let unsigned =
            goog4::canonical(&account.client_email, "GET", GCS_HOST, path, &[], 300, now);
        let url_signature = from_hex(
            query
                .rsplit("&X-Goog-Signature=")
                .next()
                .expect("the query carries a signature"),
        );
        public
            .verify(unsigned.to_sign().as_bytes(), &url_signature)
            .expect("the V4 URL signature verifies against the public key");

        let key = StorageKey::new("a/b c.png").expect("valid");
        let url = storage
            .signed_url(&key, std::time::Duration::from_secs(300))
            .await
            .expect("signs a URL");
        let url = url.as_str();
        assert!(
            url.starts_with("https://storage.googleapis.com/uploads/a/b%20c.png?"),
            "{url}",
        );
        assert!(url.contains("X-Goog-Algorithm=GOOG4-RSA-SHA256"), "{url}");
        assert!(url.contains("&X-Goog-Signature="), "{url}");

        // 3. The presigned upload signs the `x-goog-*` headers the client will
        //    echo, so its signature covers a PUT with those headers.
        let policy =
            crate::UploadPolicy::new(0..=8 * 1024 * 1024, std::time::Duration::from_secs(600))
                .metadata("uploaded-by", "usr_1");
        let post = storage
            .presigned_upload(&key, policy)
            .await
            .expect("presigns an upload");
        assert_eq!(post.method, "PUT");
        let post_url = post.url.as_str();
        assert!(post_url.contains("&X-Goog-Signature="), "{post_url}");
        assert!(
            post_url.contains("x-goog-meta-uploaded-by"),
            "the metadata header is part of the signature: {post_url}",
        );

        let put_headers = [("x-goog-meta-uploaded-by".to_owned(), "usr_1".to_owned())];
        let put_query = goog4::presign(account, "PUT", GCS_HOST, path, &put_headers, 600, now)
            .expect("presigns");
        let put_unsigned = goog4::canonical(
            &account.client_email,
            "PUT",
            GCS_HOST,
            path,
            &put_headers,
            600,
            now,
        );
        let put_signature = from_hex(
            put_query
                .rsplit("&X-Goog-Signature=")
                .next()
                .expect("signature"),
        );
        public
            .verify(put_unsigned.to_sign().as_bytes(), &put_signature)
            .expect("the presigned upload signature verifies against the public key");

        // 4. The RFC 7523 JWT-bearer assertion — what the token endpoint
        //    exchanges for an access token — is a signed, verifiable JWT.
        let assertion = account.assertion(now).expect("mints an assertion");
        let parts: Vec<&str> = assertion.split('.').collect();
        assert_eq!(parts.len(), 3, "a JWT is header.claims.signature");

        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let jwt_signature = URL_SAFE_NO_PAD
            .decode(parts[2])
            .expect("base64url signature");
        public
            .verify(signing_input.as_bytes(), &jwt_signature)
            .expect("the JWT-bearer assertion verifies against the public key");

        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).expect("base64url"))
                .expect("json header");
        assert_eq!(header["alg"], "RS256");
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).expect("base64url"))
                .expect("json claims");
        assert_eq!(claims["iss"], EMAIL);
        assert_eq!(claims["aud"], GOOGLE_TOKEN_URI);
        assert_eq!(claims["scope"], GCS_SCOPE);
    }

    /// A SAS whose signature does not cover the blob, the expiry or the
    /// permissions is a URL that opens the wrong door.
    #[cfg(feature = "azure")]
    #[test]
    fn an_azure_sas_covers_the_blob_the_expiry_and_the_permissions() {
        let storage = AzureStorage::new(
            "acct",
            "uploads",
            SecretString::new(STANDARD.encode([1_u8; 32])),
        );
        let now = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("valid")
            .with_timezone(&chrono::Utc);
        let ttl = std::time::Duration::from_secs(300);
        let sas = |key: &str, permissions: &str, ttl| {
            storage
                .service_sas(&StorageKey::new(key).expect("valid"), permissions, ttl, now)
                .expect("signs")
        };

        let read = sas("a/b.png", SAS_READ, ttl);
        assert!(read.contains("sv=2021-12-02"), "{read}");
        assert!(read.contains("sr=b"), "{read}");
        assert!(read.contains("sp=r"), "{read}");
        assert!(read.contains("se=2026-01-01T00%3A05%3A00Z"), "{read}");
        assert!(read.contains("spr=https"), "{read}");
        assert!(read.contains("sig="), "{read}");

        assert_ne!(
            signature_of(&read),
            signature_of(&sas("a/c.png", SAS_READ, ttl))
        );
        assert_ne!(
            signature_of(&read),
            signature_of(&sas("a/b.png", SAS_CREATE_WRITE, ttl)),
        );
        assert_ne!(
            signature_of(&read),
            signature_of(&sas(
                "a/b.png",
                SAS_READ,
                std::time::Duration::from_secs(600)
            )),
        );

        // Reproducible for a fixed instant, or a retry is a different URL.
        assert_eq!(read, sas("a/b.png", SAS_READ, ttl));
    }

    /// A `Put Blob` without `x-ms-blob-type` is a 400, and a client that was
    /// handed only a URL has no way to know that.
    #[cfg(feature = "azure")]
    #[tokio::test]
    async fn an_azure_presigned_upload_tells_the_client_what_to_send() {
        let storage = AzureStorage::new(
            "acct",
            "uploads",
            SecretString::new(STANDARD.encode([2_u8; 32])),
        );
        let key = StorageKey::new("uploads/direct.png").expect("valid");
        let policy = crate::UploadPolicy::new(1..=4096, std::time::Duration::from_secs(600))
            .metadata("uploaded-by", "usr_1");

        let post = storage
            .presigned_upload(&key, policy)
            .await
            .expect("presigns");

        assert_eq!(post.method, "PUT");
        assert!(
            post.url
                .as_str()
                .starts_with("https://acct.blob.core.windows.net/uploads/uploads/direct.png?"),
            "{}",
            post.url,
        );
        assert!(post.url.as_str().contains("sp=cw"), "{}", post.url);
        assert!(
            post.fields
                .iter()
                .any(|(name, value)| name == "x-ms-blob-type" && value == "BlockBlob"),
            "{:?}",
            post.fields,
        );
        assert!(
            post.fields
                .iter()
                .any(|(name, value)| name == "x-ms-meta-uploaded-by" && value == "usr_1"),
            "{:?}",
            post.fields,
        );
    }

    /// A credential that cannot sign has to say which one it is, not fail at
    /// the browser with a 403.
    #[cfg(feature = "azure")]
    #[tokio::test]
    async fn an_azure_sas_with_a_malformed_key_names_the_credential() {
        let storage = AzureStorage::new("acct", "uploads", SecretString::new("not base64 !!"));
        let error = storage
            .signed_url(
                &StorageKey::new("a/b.png").expect("valid"),
                std::time::Duration::from_secs(60),
            )
            .await
            .expect_err("the key is not base64");
        assert!(error.to_string().contains("Access keys"), "{error}");
    }

    /// Azure's canonical string is fixed and unforgiving; a signature over the
    /// wrong path or the wrong headers is a 403 with no detail.
    #[cfg(feature = "azure")]
    #[test]
    fn the_azure_signature_covers_the_path_the_method_and_the_ms_headers() {
        let storage = AzureStorage::new(
            "acct",
            "uploads",
            SecretString::new(STANDARD.encode([1_u8; 32])),
        );
        let headers = vec![
            (
                "x-ms-date".to_owned(),
                "Thu, 01 Jan 2026 00:00:00 GMT".to_owned(),
            ),
            ("x-ms-version".to_owned(), AZURE_VERSION.to_owned()),
        ];

        let a = storage.sign("GET", "/uploads/a", "", &headers, 0);
        let b = storage.sign("GET", "/uploads/b", "", &headers, 0);
        let put = storage.sign("PUT", "/uploads/a", "", &headers, 0);

        assert!(!a.is_empty());
        assert_ne!(a, b);
        assert_ne!(a, put);

        // A different `x-ms-date` is a different signature, which is what
        // stops a captured request being replayed forever.
        let later = vec![
            (
                "x-ms-date".to_owned(),
                "Fri, 02 Jan 2026 00:00:00 GMT".to_owned(),
            ),
            ("x-ms-version".to_owned(), AZURE_VERSION.to_owned()),
        ];
        assert_ne!(a, storage.sign("GET", "/uploads/a", "", &later, 0));
    }

    /// A credential that is not base64 cannot sign, and returning an empty
    /// signature is better than panicking in a request path.
    #[cfg(feature = "azure")]
    #[test]
    fn a_malformed_azure_key_fails_closed() {
        let storage = AzureStorage::new("acct", "uploads", SecretString::new("not base64 !!"));
        assert!(storage.sign("GET", "/uploads/a", "", &[], 0).is_empty());
    }

    /// S3's list response is XML; the reader has to find every object and
    /// nothing else.
    #[cfg(feature = "s3")]
    #[test]
    fn the_s3_list_reader_finds_every_key() {
        let document = "<ListBucketResult><Contents><Key>a/1.txt</Key><Size>5</Size>\
                        </Contents><Contents><Key>a/2.txt</Key><Size>7</Size></Contents>\
                        <NextContinuationToken>tok</NextContinuationToken></ListBucketResult>";
        assert_eq!(
            S3Storage::xml_values(document, "Key"),
            vec!["a/1.txt".to_owned(), "a/2.txt".to_owned()],
        );
        assert_eq!(
            S3Storage::xml_values(document, "NextContinuationToken"),
            vec!["tok".to_owned()],
        );
        assert!(S3Storage::xml_values(document, "Missing").is_empty());
    }

    /// A key containing `&` arrives XML-escaped and has to come back intact.
    #[cfg(feature = "s3")]
    #[test]
    fn the_s3_list_reader_unescapes_entities() {
        let document = "<Contents><Key>a/b&amp;c.txt</Key></Contents>";
        assert_eq!(
            S3Storage::xml_values(document, "Key"),
            vec!["a/b&c.txt".to_owned()],
        );
    }

    /// Azure's list response is XML too, with different element names.
    #[cfg(feature = "azure")]
    #[test]
    fn the_azure_list_reader_finds_the_documented_elements() {
        let document = "<Blob><Name>a/1.txt</Name><Properties>\
                        <Content-Length>5</Content-Length></Properties></Blob>";
        assert_eq!(element(document, "Name").as_deref(), Some("a/1.txt"));
        assert_eq!(element(document, "Content-Length").as_deref(), Some("5"));
        assert_eq!(element(document, "Missing"), None);
    }
}
