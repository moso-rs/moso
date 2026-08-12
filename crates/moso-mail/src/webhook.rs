//! Provider delivery webhooks, and the signature check that makes them safe.
//!
//! A bounce webhook that is not signature-verified is an open door: anybody who
//! finds the URL can suppress any address, which is a denial-of-service against
//! a specific user's account recovery. Every shipped provider backend verifies
//! before parsing, and the parsed event is only produced by
//! [`WebhookVerifier::verify`] — there is no way to get one without the check.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Address, MessageId, Result, SuppressionReason};

/// What the provider is telling us happened.
///
/// ```
/// use moso_mail::WebhookEventKind;
///
/// assert!(WebhookEventKind::HardBounce.suppresses().is_some());
/// assert!(WebhookEventKind::Opened.suppresses().is_none());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WebhookEventKind {
    /// The provider accepted the message for delivery.
    Accepted,
    /// The message reached the recipient's server.
    Delivered,
    /// A temporary failure. Does not suppress.
    SoftBounce,
    /// A permanent failure. Suppresses.
    HardBounce,
    /// The recipient marked it as spam. Suppresses.
    Complaint,
    /// The recipient unsubscribed. Suppresses marketing only.
    Unsubscribed,
    /// The provider rejected the address outright. Suppresses.
    Invalid,
    /// The recipient opened it. Only when tracking is on.
    Opened,
    /// The recipient followed a link. Only when tracking is on.
    Clicked,
    /// The provider delayed delivery and will retry.
    Deferred,
}

impl WebhookEventKind {
    /// The suppression this event implies, when it implies one.
    ///
    /// ```
    /// use moso_mail::{SuppressionReason, WebhookEventKind};
    ///
    /// assert_eq!(
    ///     WebhookEventKind::Complaint.suppresses(),
    ///     Some(SuppressionReason::Complaint),
    /// );
    /// ```
    #[must_use]
    pub const fn suppresses(self) -> Option<SuppressionReason> {
        match self {
            Self::HardBounce => Some(SuppressionReason::HardBounce),
            Self::Complaint => Some(SuppressionReason::Complaint),
            Self::Unsubscribed => Some(SuppressionReason::Unsubscribed),
            Self::Invalid => Some(SuppressionReason::Invalid),
            _ => None,
        }
    }
}

/// One verified provider event.
///
/// ```no_run
/// use moso_mail::WebhookEvent;
///
/// # fn f(e: &WebhookEvent) {
/// if let Some(reason) = e.kind.suppresses() {
///     let _ = reason;
/// }
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WebhookEvent {
    /// What happened.
    pub kind: WebhookEventKind,
    /// Whose mailbox it happened to.
    pub recipient: Address,
    /// The message it happened to, when the provider echoes an id.
    pub message_id: Option<MessageId>,
    /// When, according to the provider.
    pub occurred_at: DateTime<Utc>,
    /// The provider's own detail, already stripped of credentials.
    pub detail: Option<String>,
    /// The provider that sent it.
    pub provider: String,
    /// The raw payload, for an operator debugging a mapping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

/// Verifies a provider's webhook signature and parses its payload.
///
/// One implementation per provider. Each one is constructed with the signing
/// secret, so a verifier that exists is a verifier that can check — there is no
/// "verification disabled" mode.
///
/// ```no_run
/// use moso_mail::{WebhookEvent, WebhookVerifier};
/// use bytes::Bytes;
///
/// fn handle(v: &dyn WebhookVerifier, h: &http::HeaderMap, b: Bytes)
///     -> moso_mail::Result<Vec<WebhookEvent>>
/// {
///     v.verify(h, &b)
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot verify mail webhooks",
    label = "not a webhook verifier",
    note = "a verifier implements `provider` and `verify`, and must check the signature before \
            parsing anything",
    note = "help: use the verifier that ships with your provider backend rather than writing \
            one — the header names and canonicalisation rules differ per provider and getting \
            them wrong silently accepts forged events"
)]
pub trait WebhookVerifier: Send + Sync + 'static {
    /// The provider this verifies for, e.g. `"ses"`.
    fn provider(&self) -> &'static str;

    /// Check the signature, then parse. One payload may carry several events.
    ///
    /// # Errors
    ///
    /// [`Error::Signature`](crate::Error::Signature) when the signature is
    /// absent, malformed or wrong — and nothing is parsed in that case.
    fn verify(&self, headers: &http::HeaderMap, body: &Bytes) -> Result<Vec<WebhookEvent>>;
}

/// Apply verified events to a suppression list.
///
/// The whole of what an application has to do with a webhook: verify, then hand
/// the events here. Returns how many suppressions were recorded.
///
/// # Errors
///
/// Whatever the suppression list reports.
///
/// ```no_run
/// use moso_mail::{apply_events, SuppressionList, WebhookEvent};
///
/// async fn go(list: &dyn SuppressionList, events: Vec<WebhookEvent>)
///     -> moso_mail::Result<usize>
/// {
///     apply_events(list, &events).await
/// }
/// ```
pub async fn apply_events(
    list: &dyn crate::SuppressionList,
    events: &[WebhookEvent],
) -> Result<usize> {
    let mut recorded = 0_usize;
    for event in events {
        let Some(reason) = event.kind.suppresses() else {
            continue;
        };
        let mut entry = crate::Suppression::at(event.recipient.clone(), reason, event.occurred_at);
        if let Some(detail) = &event.detail {
            entry = entry.with_detail(event.provider.clone(), detail.clone());
        } else {
            entry = entry.with_detail(event.provider.clone(), crate::describe_reason(reason));
        }
        list.record(entry).await?;
        recorded += 1;
    }
    Ok(recorded)
}

// ---------------------------------------------------------------------------
// the shipped verifiers
// ---------------------------------------------------------------------------

/// Compare two byte strings without leaking their contents through timing.
///
/// A signature compared with `==` returns as soon as the first byte differs,
/// which is enough to recover the expected value one byte at a time. Comparing
/// the SHA-256 *digests* instead removes the leak without needing a
/// constant-time primitive: the only thing an attacker learns from the timing
/// is how far two digests agree, and a digest cannot be walked backwards into
/// the secret. It also compares equal-length inputs whatever the inputs were,
/// so the length is not a side channel either.
fn digest_eq(a: &[u8], b: &[u8]) -> bool {
    let left = ring::digest::digest(&ring::digest::SHA256, a);
    let right = ring::digest::digest(&ring::digest::SHA256, b);
    left.as_ref() == right.as_ref()
}

/// Read one header as UTF-8, or `None`.
fn header<'a>(headers: &'a http::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Decode lowercase hex into bytes.
fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).ok())
        .collect()
}

/// Whether a Unix timestamp is within `tolerance` seconds of now.
///
/// A signature with no timestamp check is replayable forever: an attacker who
/// captures one valid bounce webhook can suppress that address again whenever
/// they like.
fn timestamp_is_fresh(timestamp: i64, tolerance: i64) -> bool {
    let now = Utc::now().timestamp();
    (now - timestamp).abs() <= tolerance
}

/// How far out of date a signed timestamp may be, in seconds.
///
/// Five minutes, the value every provider that documents one uses.
const TIMESTAMP_TOLERANCE: i64 = 300;

/// Parse a provider timestamp that may be RFC 3339 or Unix seconds.
fn parse_timestamp(value: &serde_json::Value) -> DateTime<Utc> {
    match value {
        serde_json::Value::Number(number) => number
            .as_i64()
            .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
            .unwrap_or_else(Utc::now),
        serde_json::Value::String(text) => text
            .parse::<DateTime<Utc>>()
            .or_else(|_| {
                text.parse::<i64>()
                    .map_err(|_| ())
                    .and_then(|seconds| DateTime::from_timestamp(seconds, 0).ok_or(()))
            })
            .unwrap_or_else(|()| Utc::now()),
        _ => Utc::now(),
    }
}

/// Pull the first present string out of a JSON object, by any of several keys.
///
/// The five providers spell every field differently and change the spelling
/// between API versions; a list of aliases is smaller than five parsers.
fn first_str<'a>(object: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
}

/// Build one event, validating the recipient address.
///
/// An event whose recipient is not a valid mailbox is dropped rather than
/// recorded: it would suppress a key nothing can ever match, and it is a
/// signal the payload was not what we think it is.
fn event(
    provider: &str,
    kind: WebhookEventKind,
    recipient: &str,
    message_id: Option<&str>,
    detail: Option<String>,
    occurred_at: DateTime<Utc>,
    raw: serde_json::Value,
) -> Option<WebhookEvent> {
    let recipient = Address::new(recipient).ok()?;
    Some(WebhookEvent {
        kind,
        recipient,
        message_id: message_id.map(MessageId::new),
        occurred_at,
        detail,
        provider: provider.to_owned(),
        raw: Some(raw),
    })
}

/// A verifier over a shared secret, with the canonicalisation of one provider.
///
/// One type for the four HMAC-and-secret providers rather than four: they
/// differ in which headers carry the signature and what bytes go under it, and
/// in nothing else. The scheme is chosen at construction and is not a runtime
/// option, so there is no "verification disabled" state to reach.
///
/// ```no_run
/// # use moso_core::config::SecretString;
/// # use moso_mail::webhook::{SharedSecretVerifier, WebhookScheme};
/// # fn f(secret: SecretString) {
/// let verifier = SharedSecretVerifier::new(WebhookScheme::Mailgun, secret);
/// # let _ = verifier;
/// # }
/// ```
pub struct SharedSecretVerifier {
    /// Which provider's canonicalisation to apply.
    scheme: WebhookScheme,
    /// The signing secret, redacted in `Debug`.
    secret: moso_core::config::SecretString,
}

/// The signature scheme one provider uses.
///
/// ```
/// use moso_mail::webhook::WebhookScheme;
///
/// assert_eq!(WebhookScheme::Mailgun.provider(), "mailgun");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WebhookScheme {
    /// `HMAC-SHA256(timestamp || token)`, hex, in the payload's
    /// `signature` object. Mailgun signs the *body*'s own timestamp and token
    /// rather than a header, which is why the signature travels in the JSON.
    Mailgun,
    /// Svix: `HMAC-SHA256("{id}.{timestamp}.{body}")`, base64, in
    /// `svix-signature`, with the secret itself base64 after a `whsec_`
    /// prefix. What Resend uses.
    Resend,
    /// A shared token compared in constant time. Postmark authenticates its
    /// webhook with HTTP basic credentials on the URL you registered rather
    /// than with a body signature.
    Postmark,
    /// ECDSA over P-256 of `timestamp || body`, with the public key in place
    /// of a shared secret. What SendGrid's Event Webhook uses.
    SendGrid,
}

impl WebhookScheme {
    /// The provider's short name.
    ///
    /// ```
    /// use moso_mail::webhook::WebhookScheme;
    ///
    /// assert_eq!(WebhookScheme::Resend.provider(), "resend");
    /// ```
    #[must_use]
    pub const fn provider(self) -> &'static str {
        match self {
            Self::Mailgun => "mailgun",
            Self::Resend => "resend",
            Self::Postmark => "postmark",
            Self::SendGrid => "sendgrid",
        }
    }
}

impl SharedSecretVerifier {
    /// A verifier for `scheme`, over `secret`.
    ///
    /// For [`WebhookScheme::SendGrid`] the "secret" is the **public** key
    /// SendGrid shows in its dashboard, base64-encoded — the signature is
    /// asymmetric, so there is no shared secret to leak.
    ///
    /// ```no_run
    /// # use moso_core::config::SecretString;
    /// # use moso_mail::webhook::{SharedSecretVerifier, WebhookScheme};
    /// # fn f(secret: SecretString) {
    /// let _ = SharedSecretVerifier::new(WebhookScheme::Postmark, secret);
    /// # }
    /// ```
    #[must_use]
    pub fn new(scheme: WebhookScheme, secret: moso_core::config::SecretString) -> Self {
        Self { scheme, secret }
    }

    /// Check the signature, without parsing.
    ///
    /// Split out from [`WebhookVerifier::verify`] so the "did it verify"
    /// question can be tested on its own, and so nothing in the parse path can
    /// run before the answer is known.
    ///
    /// # Errors
    ///
    /// [`Error::Signature`](crate::Error::Signature) when the signature is
    /// absent, malformed, stale or wrong.
    ///
    /// ```no_run
    /// # use bytes::Bytes;
    /// # use moso_mail::webhook::SharedSecretVerifier;
    /// # fn f(v: &SharedSecretVerifier, h: &http::HeaderMap, b: &Bytes) -> moso_mail::Result<()> {
    /// v.check(h, b)
    /// # }
    /// ```
    pub fn check(&self, headers: &http::HeaderMap, body: &Bytes) -> Result<()> {
        let refused = || crate::Error::Signature {
            backend: self.scheme.provider(),
        };
        match self.scheme {
            WebhookScheme::Mailgun => self.check_mailgun(body).ok_or_else(refused),
            WebhookScheme::Resend => self.check_svix(headers, body).ok_or_else(refused),
            WebhookScheme::Postmark => self.check_postmark(headers).ok_or_else(refused),
            WebhookScheme::SendGrid => self.check_sendgrid(headers, body).ok_or_else(refused),
        }
    }

    /// `HMAC-SHA256(timestamp || token)`, hex, from the body's own `signature`
    /// object.
    fn check_mailgun(&self, body: &Bytes) -> Option<()> {
        let payload: serde_json::Value = serde_json::from_slice(body).ok()?;
        let signature = payload.get("signature")?;
        let timestamp = signature.get("timestamp")?;
        let timestamp = timestamp
            .as_str()
            .map(str::to_owned)
            .or_else(|| timestamp.as_i64().map(|value| value.to_string()))?;
        let token = signature.get("token")?.as_str()?;
        let expected = from_hex(signature.get("signature")?.as_str()?)?;

        if !timestamp_is_fresh(timestamp.parse().ok()?, TIMESTAMP_TOLERANCE) {
            return None;
        }

        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, self.secret.expose_bytes());
        let mut signed = timestamp;
        signed.push_str(token);
        ring::hmac::verify(&key, signed.as_bytes(), &expected).ok()
    }

    /// Svix: `HMAC-SHA256("{id}.{timestamp}.{body}")`, base64, one or more
    /// `v1,<sig>` values in `svix-signature`.
    fn check_svix(&self, headers: &http::HeaderMap, body: &Bytes) -> Option<()> {
        let id = header(headers, "svix-id").or_else(|| header(headers, "webhook-id"))?;
        let timestamp =
            header(headers, "svix-timestamp").or_else(|| header(headers, "webhook-timestamp"))?;
        let signatures =
            header(headers, "svix-signature").or_else(|| header(headers, "webhook-signature"))?;

        if !timestamp_is_fresh(timestamp.parse().ok()?, TIMESTAMP_TOLERANCE) {
            return None;
        }

        // The secret is `whsec_` followed by the base64 of the raw key.
        let secret = self.secret.expose();
        let secret = secret.strip_prefix("whsec_").unwrap_or(secret);
        let key_bytes = STANDARD.decode(secret).ok()?;
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &key_bytes);

        let mut signed = String::with_capacity(body.len() + 64);
        signed.push_str(id);
        signed.push('.');
        signed.push_str(timestamp);
        signed.push('.');
        signed.push_str(core::str::from_utf8(body).ok()?);
        // Any one of the space-separated versioned signatures matching is
        // enough; that is how Svix rotates a secret without downtime.
        signatures
            .split(' ')
            .filter_map(|entry| entry.strip_prefix("v1,"))
            .filter_map(|encoded| STANDARD.decode(encoded).ok())
            .any(|candidate| ring::hmac::verify(&key, signed.as_bytes(), &candidate).is_ok())
            .then_some(())
    }

    /// A shared token, compared in constant time.
    fn check_postmark(&self, headers: &http::HeaderMap) -> Option<()> {
        // Either the basic-auth password on the URL Postmark was given, or a
        // bare token header for a proxy that strips credentials.
        let presented = header(headers, http::header::AUTHORIZATION.as_str())
            .and_then(|value| value.strip_prefix("Basic "))
            .and_then(|encoded| STANDARD.decode(encoded).ok())
            .and_then(|decoded| String::from_utf8(decoded).ok())
            .map(|pair| {
                pair.split_once(':')
                    .map_or(pair.clone(), |(_, password)| password.to_owned())
            })
            .or_else(|| header(headers, "x-postmark-token").map(str::to_owned))?;

        digest_eq(presented.as_bytes(), self.secret.expose_bytes()).then_some(())
    }

    /// ECDSA over P-256 of `timestamp || body`, with a base64 DER public key.
    fn check_sendgrid(&self, headers: &http::HeaderMap, body: &Bytes) -> Option<()> {
        let signature = header(headers, "x-twilio-email-event-webhook-signature")?;
        let timestamp = header(headers, "x-twilio-email-event-webhook-timestamp")?;
        let signature = STANDARD.decode(signature).ok()?;
        let key_der = STANDARD.decode(self.secret.expose()).ok()?;

        // SendGrid publishes the key as a DER SubjectPublicKeyInfo; `ring`
        // wants the raw uncompressed point, which is its last 65 bytes.
        let point = key_der.get(key_der.len().checked_sub(65)?..)?;
        if point.first() != Some(&0x04) {
            return None;
        }

        let mut signed = Vec::with_capacity(body.len() + timestamp.len());
        signed.extend_from_slice(timestamp.as_bytes());
        signed.extend_from_slice(body);

        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, point)
            .verify(&signed, &signature)
            .ok()
    }

    /// Map a provider's own event name onto the taxonomy.
    fn kind_of(&self, name: &str, payload: &serde_json::Value) -> Option<WebhookEventKind> {
        let lower = name.to_ascii_lowercase();
        Some(match lower.as_str() {
            "accepted" | "processed" | "queued" => WebhookEventKind::Accepted,
            "delivered" | "delivery" => WebhookEventKind::Delivered,
            "opened" | "open" => WebhookEventKind::Opened,
            "clicked" | "click" => WebhookEventKind::Clicked,
            "deferred" | "delayed" => WebhookEventKind::Deferred,
            "complained" | "complaint" | "spamcomplaint" | "spamreport" => {
                WebhookEventKind::Complaint
            }
            "unsubscribed" | "unsubscribe" | "subscriptionchange" | "group_unsubscribe"
            | "groupunsubscribe" => WebhookEventKind::Unsubscribed,
            "dropped" | "invalid" | "invalidemail" | "blocked" => WebhookEventKind::Invalid,
            "bounce" | "bounced" | "failed" | "permanent_fail" => {
                // The severity decides whether this suppresses. A soft bounce
                // recorded as permanent locks a mailbox out over a full inbox.
                let severity = first_str(payload, &["severity", "Type", "type", "bounce_class"])
                    .unwrap_or("permanent")
                    .to_ascii_lowercase();
                if severity.contains("temporary")
                    || severity.contains("soft")
                    || severity.contains("transient")
                {
                    WebhookEventKind::SoftBounce
                } else {
                    WebhookEventKind::HardBounce
                }
            }
            "softbounce" | "temporary_fail" | "transient" => WebhookEventKind::SoftBounce,
            "hardbounce" => WebhookEventKind::HardBounce,
            _ => return None,
        })
    }

    /// Parse an already-verified payload into events.
    fn parse(&self, body: &Bytes) -> Vec<WebhookEvent> {
        let Ok(payload) = serde_json::from_slice::<serde_json::Value>(body) else {
            return Vec::new();
        };

        // SendGrid posts an array; the others post one object. Mailgun wraps
        // the interesting half in `event-data`.
        let items: Vec<serde_json::Value> = match payload {
            serde_json::Value::Array(items) => items,
            serde_json::Value::Object(_) => {
                let unwrapped = payload
                    .get("event-data")
                    .cloned()
                    .unwrap_or_else(|| payload.clone());
                vec![unwrapped]
            }
            _ => return Vec::new(),
        };

        items
            .into_iter()
            .filter_map(|item| {
                let name = first_str(
                    &item,
                    &[
                        "event",
                        "RecordType",
                        "Type",
                        "type",
                        "eventType",
                        "notificationType",
                    ],
                )?;
                let kind = self.kind_of(name, &item)?;
                let recipient =
                    first_str(&item, &["recipient", "email", "Email", "Recipient", "to"]).or_else(
                        || {
                            item.get("message")
                                .and_then(|message| message.get("headers"))
                                .and_then(|headers| headers.get("to"))
                                .and_then(serde_json::Value::as_str)
                        },
                    )?;
                let message_id = first_str(
                    &item,
                    &[
                        "MessageID",
                        "message_id",
                        "sg_message_id",
                        "id",
                        "Message-Id",
                    ],
                );
                let detail = first_str(
                    &item,
                    &[
                        "reason",
                        "Description",
                        "description",
                        "Details",
                        "delivery-status",
                    ],
                )
                .map(str::to_owned);
                let occurred_at = ["timestamp", "ReceivedAt", "created_at", "DeliveredAt"]
                    .iter()
                    .find_map(|key| item.get(*key))
                    .map_or_else(Utc::now, parse_timestamp);

                let recipient = recipient.to_owned();
                let message_id = message_id.map(str::to_owned);
                event(
                    self.scheme.provider(),
                    kind,
                    &recipient,
                    message_id.as_deref(),
                    detail,
                    occurred_at,
                    item,
                )
            })
            .collect()
    }
}

impl core::fmt::Debug for SharedSecretVerifier {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SharedSecretVerifier")
            .field("scheme", &self.scheme)
            .finish_non_exhaustive()
    }
}

impl WebhookVerifier for SharedSecretVerifier {
    fn provider(&self) -> &'static str {
        self.scheme.provider()
    }

    fn verify(&self, headers: &http::HeaderMap, body: &Bytes) -> Result<Vec<WebhookEvent>> {
        // Nothing is parsed until this returns `Ok`. That ordering is the
        // whole security property of this module.
        self.check(headers, body)?;
        Ok(self.parse(body))
    }
}

/// Amazon SNS, which is how SES delivers events, over a pinned public key.
///
/// # Why a pinned key and not the certificate URL
///
/// An SNS message names the URL of the certificate that signed it. Fetching
/// that URL at verification time would mean a network round trip inside
/// [`WebhookVerifier::verify`], which is synchronous — and, worse, would make
/// the check depend on an attacker-influenced URL. Pinning the key is both
/// simpler and stronger.
///
/// Extract it once from Amazon's signing certificate:
///
/// ```text
/// curl -s "$SIGNING_CERT_URL" | openssl x509 -pubkey -noout
/// ```
///
/// and configure the PEM as the signing secret. `SubjectPublicKeyInfo` DER is
/// accepted too, base64-encoded.
///
/// ```no_run
/// # use moso_core::config::SecretString;
/// # use moso_mail::webhook::SnsVerifier;
/// # fn f(public_key_pem: SecretString) {
/// let _ = SnsVerifier::new(public_key_pem);
/// # }
/// ```
pub struct SnsVerifier {
    /// The signing key's DER `SubjectPublicKeyInfo`.
    key_der: Vec<u8>,
}

impl SnsVerifier {
    /// A verifier over a pinned public key, PEM or base64 DER.
    ///
    /// ```no_run
    /// # use moso_core::config::SecretString;
    /// # use moso_mail::webhook::SnsVerifier;
    /// # fn f(pem: SecretString) { let _ = SnsVerifier::new(pem); }
    /// ```
    #[must_use]
    pub fn new(public_key: moso_core::config::SecretString) -> Self {
        Self {
            key_der: decode_pem(public_key.expose()).unwrap_or_default(),
        }
    }

    /// The canonical string SNS signs: every signed field, name then value,
    /// each newline-terminated, in the order the specification fixes.
    fn canonical(payload: &serde_json::Value) -> Option<String> {
        let kind = payload.get("Type")?.as_str()?;
        let fields: &[&str] = match kind {
            "Notification" => &[
                "Message",
                "MessageId",
                "Subject",
                "Timestamp",
                "TopicArn",
                "Type",
            ],
            "SubscriptionConfirmation" | "UnsubscribeConfirmation" => &[
                "Message",
                "MessageId",
                "SubscribeURL",
                "Timestamp",
                "Token",
                "TopicArn",
                "Type",
            ],
            _ => return None,
        };

        let mut canonical = String::new();
        for field in fields {
            // `Subject` is optional and is simply absent from the canonical
            // string when the message did not carry one.
            let Some(value) = payload.get(*field).and_then(serde_json::Value::as_str) else {
                continue;
            };
            canonical.push_str(field);
            canonical.push('\n');
            canonical.push_str(value);
            canonical.push('\n');
        }
        Some(canonical)
    }
}

impl core::fmt::Debug for SnsVerifier {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SnsVerifier")
            .field("key_bytes", &self.key_der.len())
            .finish_non_exhaustive()
    }
}

impl WebhookVerifier for SnsVerifier {
    fn provider(&self) -> &'static str {
        "ses"
    }

    fn verify(&self, headers: &http::HeaderMap, body: &Bytes) -> Result<Vec<WebhookEvent>> {
        let _ = headers;
        let refused = || crate::Error::Signature { backend: "ses" };

        let outer: serde_json::Value = serde_json::from_slice(body).map_err(|_| refused())?;
        let canonical = Self::canonical(&outer).ok_or_else(refused)?;
        let signature = outer
            .get("Signature")
            .and_then(serde_json::Value::as_str)
            .and_then(|encoded| STANDARD.decode(encoded).ok())
            .ok_or_else(refused)?;

        // SNS signature version 1 is SHA-1 and version 2 is SHA-256. Only
        // version 2 is accepted: SHA-1 signatures are no longer a defence, and
        // every SNS topic can be switched to version 2 in one API call.
        let version = outer
            .get("SignatureVersion")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("1");
        if version != "2" {
            return Err(refused());
        }

        ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            &self.key_der,
        )
        .verify(canonical.as_bytes(), &signature)
        .map_err(|_| refused())?;

        // The SES event itself is a JSON document inside SNS's `Message`.
        let Some(message) = outer.get("Message").and_then(serde_json::Value::as_str) else {
            return Ok(Vec::new());
        };
        let Ok(inner) = serde_json::from_str::<serde_json::Value>(message) else {
            return Ok(Vec::new());
        };

        Ok(parse_ses(&inner))
    }
}

/// Parse one verified SES notification into events, one per recipient.
fn parse_ses(inner: &serde_json::Value) -> Vec<WebhookEvent> {
    let notification = first_str(inner, &["notificationType", "eventType"]).unwrap_or_default();
    let occurred_at = inner
        .get("mail")
        .and_then(|mail| mail.get("timestamp"))
        .map_or_else(Utc::now, parse_timestamp);
    let message_id = inner
        .get("mail")
        .and_then(|mail| mail.get("messageId"))
        .and_then(serde_json::Value::as_str);

    // SES reports the affected recipients in a per-notification array, and one
    // notification can carry several. One event each.
    let (kind, recipients_key, container) = match notification {
        "Bounce" => {
            let permanent = inner
                .get("bounce")
                .and_then(|bounce| bounce.get("bounceType"))
                .and_then(serde_json::Value::as_str)
                == Some("Permanent");
            (
                if permanent {
                    WebhookEventKind::HardBounce
                } else {
                    WebhookEventKind::SoftBounce
                },
                "bouncedRecipients",
                "bounce",
            )
        }
        "Complaint" => (
            WebhookEventKind::Complaint,
            "complainedRecipients",
            "complaint",
        ),
        "Delivery" => (WebhookEventKind::Delivered, "recipients", "delivery"),
        _ => return Vec::new(),
    };

    let Some(container) = inner.get(container) else {
        return Vec::new();
    };
    let Some(recipients) = container
        .get(recipients_key)
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };

    recipients
        .iter()
        .filter_map(|entry| {
            // `recipients` on a delivery is an array of strings; the bounce and
            // complaint arrays are objects with an `emailAddress`.
            let address = entry.as_str().or_else(|| {
                entry
                    .get("emailAddress")
                    .and_then(serde_json::Value::as_str)
            })?;
            let detail = entry
                .get("diagnosticCode")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            event(
                "ses",
                kind,
                address,
                message_id,
                detail,
                occurred_at,
                entry.clone(),
            )
        })
        .collect()
}

/// Decode a PEM public key, or base64 DER, into DER bytes.
fn decode_pem(text: &str) -> Option<Vec<u8>> {
    let body: String = if text.contains("-----BEGIN") {
        text.lines()
            .filter(|line| !line.starts_with("-----"))
            .collect()
    } else {
        text.split_whitespace().collect()
    };
    let der = STANDARD.decode(body.trim()).ok()?;
    // `ring` wants the RSA public key itself, not the SPKI wrapper. The key is
    // the BIT STRING at the end of the SPKI; find it by looking for the
    // `rsaEncryption` OID and taking the payload that follows.
    Some(strip_spki(&der).unwrap_or(der))
}

/// The `rsaEncryption` algorithm identifier, DER-encoded.
const RSA_ENCRYPTION_OID: &[u8] = &[
    0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
];

/// Pull the RSA public key out of a `SubjectPublicKeyInfo`.
///
/// Hand-rolled rather than pulling an X.509 parser for one shape: the only
/// input is Amazon's own signing key, the structure is fixed, and every step
/// below fails closed.
fn strip_spki(der: &[u8]) -> Option<Vec<u8>> {
    let at = der
        .windows(RSA_ENCRYPTION_OID.len())
        .position(|window| window == RSA_ENCRYPTION_OID)?;
    let rest = der.get(at + RSA_ENCRYPTION_OID.len()..)?;
    // A BIT STRING: tag, length, then one "unused bits" byte that is always 0
    // for a key.
    if rest.first() != Some(&0x03) {
        return None;
    }
    let (length, header) = der_length(rest.get(1..)?)?;
    let payload = rest.get(1 + header..1 + header + length)?;
    payload
        .split_first()
        .and_then(|(unused, key)| (*unused == 0).then(|| key.to_vec()))
}

/// Read a DER length, returning it and how many bytes it occupied.
fn der_length(bytes: &[u8]) -> Option<(usize, usize)> {
    let first = *bytes.first()?;
    if first < 0x80 {
        return Some((usize::from(first), 1));
    }
    let count = usize::from(first & 0x7f);
    if count == 0 || count > core::mem::size_of::<usize>() {
        return None;
    }
    let mut length = 0_usize;
    for byte in bytes.get(1..=count)? {
        length = length.checked_mul(256)?.checked_add(usize::from(*byte))?;
    }
    Some((length, 1 + count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SuppressionList as _;
    use moso_core::config::SecretString;

    /// Hex-encode, for building a Mailgun signature in a test.
    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// A Mailgun payload signed with `secret`, at `timestamp`.
    fn mailgun_body(secret: &str, timestamp: i64, event: &str, recipient: &str) -> Bytes {
        let token = "0123456789abcdef";
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
        let signature = ring::hmac::sign(&key, format!("{timestamp}{token}").as_bytes());
        let payload = serde_json::json!({
            "signature": {
                "timestamp": timestamp.to_string(),
                "token": token,
                "signature": to_hex(signature.as_ref()),
            },
            "event-data": {
                "event": event,
                "severity": "permanent",
                "recipient": recipient,
                "timestamp": timestamp,
                "id": "abc123",
            },
        });
        Bytes::from(serde_json::to_vec(&payload).expect("serialises"))
    }

    /// The acceptance criterion: a real signature verifies and yields events.
    #[test]
    fn a_correctly_signed_mailgun_payload_verifies() {
        let verifier =
            SharedSecretVerifier::new(WebhookScheme::Mailgun, SecretString::new("s3cret"));
        let body = mailgun_body("s3cret", Utc::now().timestamp(), "failed", "a@example.com");

        let events = verifier
            .verify(&http::HeaderMap::new(), &body)
            .expect("verifies");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, WebhookEventKind::HardBounce);
        assert_eq!(events[0].recipient.address(), "a@example.com");
        assert_eq!(events[0].provider, "mailgun");
    }

    /// The half that matters: a forged payload is refused, and nothing is
    /// parsed out of it.
    #[test]
    fn a_mailgun_payload_signed_with_the_wrong_secret_is_refused() {
        let verifier =
            SharedSecretVerifier::new(WebhookScheme::Mailgun, SecretString::new("s3cret"));
        let body = mailgun_body("guessed", Utc::now().timestamp(), "failed", "a@example.com");

        let error = verifier
            .verify(&http::HeaderMap::new(), &body)
            .expect_err("forged");
        assert!(matches!(
            error,
            crate::Error::Signature { backend: "mailgun" }
        ));
    }

    /// A captured-and-replayed webhook must stop working: without the freshness
    /// check, one valid bounce event suppresses that address forever.
    #[test]
    fn a_stale_mailgun_signature_is_refused() {
        let verifier =
            SharedSecretVerifier::new(WebhookScheme::Mailgun, SecretString::new("s3cret"));
        let hour_ago = Utc::now().timestamp() - 3_600;
        let body = mailgun_body("s3cret", hour_ago, "failed", "a@example.com");

        assert!(verifier.check(&http::HeaderMap::new(), &body).is_err());
    }

    /// A payload with no signature at all must not be treated as unsigned-and-
    /// therefore-fine.
    #[test]
    fn an_unsigned_payload_is_refused() {
        let verifier =
            SharedSecretVerifier::new(WebhookScheme::Mailgun, SecretString::new("s3cret"));
        let body = Bytes::from_static(br#"{"event-data":{"event":"failed"}}"#);
        assert!(verifier.check(&http::HeaderMap::new(), &body).is_err());
    }

    /// Svix's canonicalisation is `{id}.{timestamp}.{body}` under a base64
    /// secret; getting any part of it wrong silently accepts forged events.
    #[test]
    fn a_correctly_signed_resend_payload_verifies() {
        use base64::Engine as _;

        let raw_key = b"0123456789abcdef0123456789abcdef";
        let secret = format!("whsec_{}", STANDARD.encode(raw_key));
        let verifier = SharedSecretVerifier::new(WebhookScheme::Resend, SecretString::new(secret));

        let id = "msg_2b";
        let timestamp = Utc::now().timestamp().to_string();
        let body = Bytes::from_static(
            br#"{"type":"email.bounced","data":{"to":["a@example.com"]},"created_at":1}"#,
        );
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, raw_key);
        let signed = format!(
            "{id}.{timestamp}.{}",
            core::str::from_utf8(&body).expect("utf8")
        );
        let tag = ring::hmac::sign(&key, signed.as_bytes());

        let mut headers = http::HeaderMap::new();
        headers.insert("svix-id", http::HeaderValue::from_static("msg_2b"));
        headers.insert(
            "svix-timestamp",
            http::HeaderValue::from_str(&timestamp).expect("ascii"),
        );
        headers.insert(
            "svix-signature",
            http::HeaderValue::from_str(&format!("v1,{}", STANDARD.encode(tag.as_ref())))
                .expect("ascii"),
        );

        verifier.check(&headers, &body).expect("verifies");

        // One wrong byte in the body and the same headers no longer verify.
        let tampered = Bytes::from_static(
            br#"{"type":"email.bounced","data":{"to":["b@example.com"]},"created_at":1}"#,
        );
        assert!(verifier.check(&headers, &tampered).is_err());
    }

    /// Svix rotates secrets by sending two signatures; either matching is a
    /// pass, or a rotation is an outage.
    #[test]
    fn resend_accepts_any_one_of_several_signatures() {
        use base64::Engine as _;

        let raw_key = b"0123456789abcdef0123456789abcdef";
        let secret = format!("whsec_{}", STANDARD.encode(raw_key));
        let verifier = SharedSecretVerifier::new(WebhookScheme::Resend, SecretString::new(secret));

        let timestamp = Utc::now().timestamp().to_string();
        let body = Bytes::from_static(b"{}");
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, raw_key);
        let signed = format!("id1.{timestamp}.{{}}");
        let tag = ring::hmac::sign(&key, signed.as_bytes());

        let mut headers = http::HeaderMap::new();
        headers.insert("svix-id", http::HeaderValue::from_static("id1"));
        headers.insert(
            "svix-timestamp",
            http::HeaderValue::from_str(&timestamp).expect("ascii"),
        );
        headers.insert(
            "svix-signature",
            http::HeaderValue::from_str(&format!(
                "v1,{} v1,{}",
                STANDARD.encode(b"not-the-signature"),
                STANDARD.encode(tag.as_ref()),
            ))
            .expect("ascii"),
        );

        verifier
            .check(&headers, &body)
            .expect("the second one matches");
    }

    /// Postmark authenticates with a shared token rather than a body
    /// signature, and the compare must not leak it.
    #[test]
    fn postmark_accepts_the_configured_token_and_refuses_another() {
        use base64::Engine as _;

        let verifier =
            SharedSecretVerifier::new(WebhookScheme::Postmark, SecretString::new("hook-token"));
        let body = Bytes::from_static(
            br#"{"RecordType":"Bounce","Type":"HardBounce","Email":"a@example.com"}"#,
        );

        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Basic {}", STANDARD.encode("moso:hook-token")))
                .expect("ascii"),
        );
        let events = verifier.verify(&headers, &body).expect("verifies");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, WebhookEventKind::HardBounce);

        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Basic {}", STANDARD.encode("moso:wrong")))
                .expect("ascii"),
        );
        assert!(verifier.verify(&headers, &body).is_err());
    }

    /// A soft bounce is a full inbox, not a dead address. Suppressing on one
    /// locks a real user out of their own account recovery.
    #[test]
    fn a_soft_bounce_does_not_suppress() {
        let verifier = SharedSecretVerifier::new(WebhookScheme::Postmark, SecretString::new("t"));
        let body = Bytes::from_static(
            br#"{"RecordType":"Bounce","Type":"Transient","Email":"a@example.com"}"#,
        );
        let mut headers = http::HeaderMap::new();
        headers.insert("x-postmark-token", http::HeaderValue::from_static("t"));

        let events = verifier.verify(&headers, &body).expect("verifies");
        assert_eq!(events[0].kind, WebhookEventKind::SoftBounce);
        assert!(events[0].kind.suppresses().is_none());
    }

    /// SendGrid posts an array of events in one body.
    #[test]
    fn a_batched_payload_becomes_one_event_each() {
        let verifier = SharedSecretVerifier::new(WebhookScheme::Postmark, SecretString::new("t"));
        let body = Bytes::from_static(
            br#"[{"event":"bounce","email":"a@example.com","timestamp":1700000000},
                 {"event":"spamreport","email":"b@example.com","timestamp":1700000001},
                 {"event":"open","email":"c@example.com","timestamp":1700000002}]"#,
        );
        let mut headers = http::HeaderMap::new();
        headers.insert("x-postmark-token", http::HeaderValue::from_static("t"));

        let events = verifier.verify(&headers, &body).expect("verifies");
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind, WebhookEventKind::HardBounce);
        assert_eq!(events[1].kind, WebhookEventKind::Complaint);
        assert_eq!(events[2].kind, WebhookEventKind::Opened);
    }

    /// An event naming an address that is not a mailbox is dropped: recording
    /// it would suppress a key nothing can match.
    #[test]
    fn an_event_with_an_unparseable_recipient_is_dropped() {
        let verifier = SharedSecretVerifier::new(WebhookScheme::Postmark, SecretString::new("t"));
        let body = Bytes::from_static(br#"[{"event":"bounce","email":"not an address"}]"#);
        let mut headers = http::HeaderMap::new();
        headers.insert("x-postmark-token", http::HeaderValue::from_static("t"));

        assert!(
            verifier
                .verify(&headers, &body)
                .expect("verifies")
                .is_empty()
        );
    }

    /// SNS signature version 1 is SHA-1 and is no longer a defence.
    #[test]
    fn an_sns_message_signed_with_sha1_is_refused() {
        let verifier = SnsVerifier::new(SecretString::new(String::new()));
        let body = Bytes::from_static(
            br#"{"Type":"Notification","MessageId":"1","Message":"{}","Timestamp":"t",
                 "TopicArn":"arn","SignatureVersion":"1","Signature":"AA=="}"#,
        );
        assert!(verifier.verify(&http::HeaderMap::new(), &body).is_err());
    }

    /// SNS's canonical string is name-then-value, newline-terminated, in a
    /// fixed order, with an absent `Subject` simply skipped. Getting the order
    /// wrong means every real signature fails.
    #[test]
    fn the_sns_canonical_string_has_the_documented_shape() {
        let payload = serde_json::json!({
            "Type": "Notification",
            "MessageId": "id-1",
            "Message": "body",
            "Timestamp": "2026-01-01T00:00:00.000Z",
            "TopicArn": "arn:aws:sns:eu-central-1:1:mail",
        });
        let canonical = SnsVerifier::canonical(&payload).expect("known type");
        assert_eq!(
            canonical,
            "Message\nbody\nMessageId\nid-1\nTimestamp\n2026-01-01T00:00:00.000Z\n\
             TopicArn\narn:aws:sns:eu-central-1:1:mail\nType\nNotification\n",
        );
    }

    /// SES reports several recipients in one notification; each is its own
    /// suppression.
    #[test]
    fn one_ses_notification_becomes_one_event_per_recipient() {
        let inner = serde_json::json!({
            "notificationType": "Bounce",
            "mail": { "timestamp": "2026-01-01T00:00:00.000Z", "messageId": "m-1" },
            "bounce": {
                "bounceType": "Permanent",
                "bouncedRecipients": [
                    { "emailAddress": "a@example.com", "diagnosticCode": "550 unknown" },
                    { "emailAddress": "b@example.com" },
                ],
            },
        });
        let events = parse_ses(&inner);
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .all(|e| e.kind == WebhookEventKind::HardBounce)
        );
        assert_eq!(events[0].detail.as_deref(), Some("550 unknown"));
        assert_eq!(
            events[0].message_id.as_ref().map(MessageId::as_str),
            Some("m-1")
        );
    }

    /// A transient SES bounce is a soft one and must not suppress.
    #[test]
    fn a_transient_ses_bounce_is_soft() {
        let inner = serde_json::json!({
            "notificationType": "Bounce",
            "mail": { "timestamp": "2026-01-01T00:00:00.000Z" },
            "bounce": {
                "bounceType": "Transient",
                "bouncedRecipients": [{ "emailAddress": "a@example.com" }],
            },
        });
        assert_eq!(parse_ses(&inner)[0].kind, WebhookEventKind::SoftBounce);
    }

    /// The whole of what an application does with a webhook: verify, then
    /// apply. Only the suppressing kinds are recorded.
    #[tokio::test]
    async fn applying_events_records_only_what_suppresses() {
        let list = crate::MemorySuppressionList::new();
        let events = vec![
            WebhookEvent {
                kind: WebhookEventKind::HardBounce,
                recipient: Address::new("bounced@example.com").expect("valid"),
                message_id: None,
                occurred_at: Utc::now(),
                detail: Some("550".to_owned()),
                provider: "ses".to_owned(),
                raw: None,
            },
            WebhookEvent {
                kind: WebhookEventKind::Opened,
                recipient: Address::new("reader@example.com").expect("valid"),
                message_id: None,
                occurred_at: Utc::now(),
                detail: None,
                provider: "ses".to_owned(),
                raw: None,
            },
        ];

        assert_eq!(apply_events(&list, &events).await.expect("applies"), 1);
        assert_eq!(list.len(), 1);
        let entry = list
            .lookup("bounced@example.com")
            .await
            .expect("looks up")
            .expect("present");
        assert_eq!(entry.source(), Some("ses"));
        assert_eq!(entry.detail(), Some("550"));
    }

    /// The digest compare is a real equality test, not a stub that always
    /// passes — the kind of mistake that turns a token check into nothing.
    #[test]
    fn the_timing_safe_compare_actually_compares() {
        assert!(digest_eq(b"same", b"same"));
        assert!(!digest_eq(b"same", b"different"));
        assert!(!digest_eq(b"", b"x"));
        assert!(digest_eq(b"", b""));
    }

    /// The DER helpers fail closed rather than panicking on rubbish, because
    /// their input is whatever an operator pasted into configuration.
    #[test]
    fn a_malformed_public_key_yields_no_key_rather_than_a_panic() {
        assert!(decode_pem("not base64 at all !!!").is_none());
        assert!(strip_spki(&[0x30, 0x00]).is_none());
        assert!(der_length(&[]).is_none());
        assert_eq!(der_length(&[0x05]), Some((5, 1)));
        assert_eq!(der_length(&[0x82, 0x01, 0x00]), Some((256, 3)));
    }
}
