//! Rate limiting: GCRA, as a [`Guard`], with the headers a client can act on.
//!
//! # Why GCRA and not a fixed window
//!
//! A fixed window admits twice the quota across a window boundary: ten
//! requests at 11:59:59 and ten more at 12:00:00 is twenty in one second under
//! a "ten a minute" limit. A sliding-window *log* fixes that and costs one
//! entry per request. GCRA — the generic cell rate algorithm, a leaky bucket
//! expressed as one timestamp — fixes it for **one number per bucket** and one
//! atomic operation per request.
//!
//! The state is a single "theoretical arrival time". Every admitted request
//! pushes it forward by the emission interval `T = period / limit`; a request
//! is admitted when that pushed-forward time is no more than `τ = burst · T`
//! ahead of now. There is no window to fall across.
//!
//! # One operation per request
//!
//! With [`Capabilities::scripting`](crate::Capabilities::scripting) — Redis —
//! the whole decision is one `EVAL`. Without it, it is a compare-and-swap loop,
//! which is one `GET` plus one conditional `SET` in the uncontended case. Both
//! are correct; only the round-trip count differs, which is exactly what
//! `capabilities()` is for.
//!
//! # What the client is told
//!
//! | Header | On success | On 429 |
//! | --- | --- | --- |
//! | `X-RateLimit-Limit` | yes | yes |
//! | `X-RateLimit-Remaining` | yes | yes (`0`) |
//! | `X-RateLimit-Reset` | yes | yes |
//! | `Retry-After` | no | yes |
//!
//! A [`Guard`] can only decorate the **rejection**, because
//! [`Guard::check`] returns `Result<()>` and never
//! sees a successful response. That is a real limitation and it is stated
//! rather than worked around: the guard is what documents the 429 in the
//! OpenAPI operation, which is the half that matters most.
//!
//! An application that wants the headers on every response calls
//! [`RateLimit::decide`] from its own middleware and copies
//! [`RateDecision::headers`] onto the response — six lines, and the decision
//! logic is shared with the guard rather than duplicated.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::request::Parts;
use http::{HeaderName, HeaderValue};
use moso_core::middleware::Guard;
use moso_core::{BoxFuture, RequestCtx};
use moso_openapi::{OperationBuilder, ResponseSpec};

use crate::error::{Error, Result};
use crate::key::{Key, KeyBuf};
use crate::kv::Kv;
use crate::store::SetOpts;

/// The namespace segment rate-limit buckets live under.
///
/// ```
/// use moso_kv::rate::RATE_PREFIX;
///
/// assert_eq!(RATE_PREFIX, "rate");
/// ```
pub const RATE_PREFIX: &str = "rate";

/// The layout version of the rate-limit keys.
const RATE_VERSION: u16 = 1;

/// How many times the compare-and-swap loop retries before giving up.
///
/// Generous, because every retry is a lost race with another request for the
/// *same* bucket, and losing a race must not turn into a spurious 429 — the
/// acceptance criterion is that a quota of ten admits exactly ten, not
/// "roughly ten under contention".
const MAX_CAS_ATTEMPTS: u32 = 32;

/// `X-RateLimit-Limit`.
pub const HEADER_LIMIT: HeaderName = HeaderName::from_static("x-ratelimit-limit");
/// `X-RateLimit-Remaining`.
pub const HEADER_REMAINING: HeaderName = HeaderName::from_static("x-ratelimit-remaining");
/// `X-RateLimit-Reset`, in whole seconds.
pub const HEADER_RESET: HeaderName = HeaderName::from_static("x-ratelimit-reset");

// ---------------------------------------------------------------------------
// RateQuota
// ---------------------------------------------------------------------------

/// How much traffic a bucket may pass.
///
/// ```
/// use moso_kv::RateQuota;
/// use std::time::Duration;
///
/// // Ten a minute, ten of which may arrive at once.
/// let quota = RateQuota::new(10, Duration::from_secs(60));
/// assert_eq!(quota.burst, 10);
/// assert_eq!(quota.emission_interval(), Duration::from_secs(6));
///
/// // ... or ten a minute, at most three at once.
/// let smoothed = quota.burst(3);
/// assert_eq!(smoothed.tolerance(), Duration::from_secs(18));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RateQuota {
    /// How many requests per [`period`](Self::period).
    pub limit: u32,
    /// The window the limit is expressed over.
    pub period: Duration,
    /// How many may arrive at once.
    ///
    /// Defaults to [`limit`](Self::limit), which is what "ten a minute" means
    /// to everybody who is not thinking about queueing theory. A smaller burst
    /// smooths traffic; a larger one is not allowed, because it would admit
    /// more than the limit in one period.
    pub burst: u32,
}

impl RateQuota {
    /// `limit` requests per `period`, burstable up to `limit`.
    ///
    /// # Panics
    ///
    /// Never. A zero limit is clamped to one, because a quota of zero would
    /// divide by zero and "reject everything" is spelled by not routing there.
    ///
    /// ```
    /// use moso_kv::RateQuota;
    /// use std::time::Duration;
    ///
    /// let quota = RateQuota::new(10, Duration::from_secs(60));
    /// assert_eq!(quota.limit, 10);
    /// assert_eq!(RateQuota::new(0, Duration::from_secs(1)).limit, 1);
    /// ```
    #[must_use]
    pub const fn new(limit: u32, period: Duration) -> Self {
        let limit = if limit == 0 { 1 } else { limit };
        Self {
            limit,
            period,
            burst: limit,
        }
    }

    /// Allow at most `burst` at once, clamped to [`limit`](Self::limit).
    ///
    /// ```
    /// use moso_kv::RateQuota;
    /// use std::time::Duration;
    ///
    /// let quota = RateQuota::new(10, Duration::from_secs(60)).burst(3);
    /// assert_eq!(quota.burst, 3);
    /// // A burst over the limit would admit more than the limit in one period.
    /// assert_eq!(quota.burst(99).burst, 10);
    /// ```
    #[must_use]
    pub const fn burst(mut self, burst: u32) -> Self {
        let burst = if burst == 0 { 1 } else { burst };
        self.burst = if burst > self.limit {
            self.limit
        } else {
            burst
        };
        self
    }

    /// `period / limit` — how long one request "costs".
    ///
    /// ```
    /// use moso_kv::RateQuota;
    /// use std::time::Duration;
    ///
    /// assert_eq!(
    ///     RateQuota::new(4, Duration::from_secs(60)).emission_interval(),
    ///     Duration::from_secs(15),
    /// );
    /// ```
    #[must_use]
    pub fn emission_interval(&self) -> Duration {
        self.period / self.limit
    }

    /// `burst · emission_interval` — how far ahead of now the bucket may run.
    ///
    /// ```
    /// use moso_kv::RateQuota;
    /// use std::time::Duration;
    ///
    /// assert_eq!(
    ///     RateQuota::new(4, Duration::from_secs(60)).tolerance(),
    ///     Duration::from_secs(60),
    /// );
    /// ```
    #[must_use]
    pub fn tolerance(&self) -> Duration {
        self.emission_interval() * self.burst
    }

    /// The emission interval in microseconds, which is what the state is in.
    fn emission_us(&self) -> u64 {
        u64::try_from(self.emission_interval().as_micros())
            .unwrap_or(u64::MAX)
            .max(1)
    }

    /// The tolerance in microseconds.
    fn tolerance_us(&self) -> u64 {
        u64::try_from(self.tolerance().as_micros()).unwrap_or(u64::MAX)
    }

    /// How long a bucket's state is worth keeping: one full drain, plus a
    /// second so a clock skew of under a second cannot resurrect an empty
    /// bucket as a full one.
    fn state_ttl(&self) -> Duration {
        self.tolerance() + self.emission_interval() + Duration::from_secs(1)
    }
}

// ---------------------------------------------------------------------------
// RateDecision
// ---------------------------------------------------------------------------

/// What the limiter decided, and what to tell the client.
///
/// ```
/// use moso_kv::RateDecision;
/// use std::time::Duration;
///
/// let allowed = RateDecision {
///     allowed: true,
///     limit: 10,
///     remaining: 7,
///     retry_after: Duration::ZERO,
///     reset: Duration::from_secs(18),
/// };
///
/// let headers = allowed.headers();
/// assert_eq!(headers.len(), 3, "no Retry-After on a request that was allowed");
/// assert_eq!(headers[1].1, "7");
/// ```
// Constructible on purpose, like `KvStats`: a middleware that renders the
// headers itself, and every test that asserts what a client is told, builds
// one of these by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// The quota's limit, echoed for the client.
    pub limit: u32,
    /// How many more requests this bucket would admit right now.
    pub remaining: u32,
    /// How long until the request would be admitted. Zero when it was.
    pub retry_after: Duration,
    /// How long until the bucket is empty again.
    pub reset: Duration,
}

impl RateDecision {
    /// The headers to attach: three when allowed, four when not.
    ///
    /// ```
    /// use moso_kv::RateDecision;
    /// use std::time::Duration;
    ///
    /// let denied = RateDecision {
    ///     allowed: false,
    ///     limit: 10,
    ///     remaining: 0,
    ///     retry_after: Duration::from_millis(1_500),
    ///     reset: Duration::from_secs(30),
    /// };
    ///
    /// let headers = denied.headers();
    /// assert_eq!(headers.len(), 4);
    /// // Rounded up: `Retry-After: 1` would invite the client back too early.
    /// assert_eq!(headers[3].1, "2");
    /// ```
    #[must_use]
    pub fn headers(&self) -> Vec<(HeaderName, HeaderValue)> {
        let mut out = Vec::with_capacity(4);
        out.push((HEADER_LIMIT, number_header(u64::from(self.limit))));
        out.push((HEADER_REMAINING, number_header(u64::from(self.remaining))));
        out.push((HEADER_RESET, number_header(ceil_secs(self.reset))));
        if !self.allowed {
            out.push((
                http::header::RETRY_AFTER,
                number_header(ceil_secs(self.retry_after).max(1)),
            ));
        }
        out
    }

    /// The 429 this decision becomes, headers and all.
    ///
    /// # Panics
    ///
    /// Never: every header value is a decimal number.
    ///
    /// ```
    /// use moso_kv::RateDecision;
    /// use std::time::Duration;
    ///
    /// let denied = RateDecision {
    ///     allowed: false,
    ///     limit: 10,
    ///     remaining: 0,
    ///     retry_after: Duration::from_secs(6),
    ///     reset: Duration::from_secs(60),
    /// };
    ///
    /// let error = denied.into_error();
    /// assert_eq!(error.status(), moso_core::deps::http::StatusCode::TOO_MANY_REQUESTS);
    /// assert_eq!(error.headers().expect("headers")["x-ratelimit-limit"], "10");
    /// ```
    #[must_use]
    pub fn into_error(self) -> moso_core::Error {
        let mut error = moso_core::Error::too_many(self.retry_after);
        for (name, value) in self.headers() {
            error = error.with_header(name, value);
        }
        error
    }
}

/// A whole number as a header value.
fn number_header(value: u64) -> HeaderValue {
    HeaderValue::try_from(value.to_string()).unwrap_or_else(|_| HeaderValue::from_static("0"))
}

/// `duration` in whole seconds, rounded up.
fn ceil_secs(duration: Duration) -> u64 {
    duration.as_secs() + u64::from(duration.subsec_nanos() > 0)
}

// ---------------------------------------------------------------------------
// The algorithm
// ---------------------------------------------------------------------------

/// The GCRA decision, server-side, in one round trip.
///
/// `KEYS[1]` is the bucket. `ARGV` is `now`, the emission interval and the
/// tolerance — all in microseconds — and the state TTL in milliseconds. The
/// reply is `{allowed, remaining, retry_after_us, reset_us}`.
///
/// `string.format('%.0f', …)` and not `tostring`: Lua would render a
/// microsecond timestamp in scientific notation, and the next request would
/// read back a number that had lost its last four digits.
pub const GCRA_SCRIPT: &str = r"
local tat = tonumber(redis.call('GET', KEYS[1]))
local now = tonumber(ARGV[1])
local emission = tonumber(ARGV[2])
local tolerance = tonumber(ARGV[3])
local ttl = tonumber(ARGV[4])

if tat == nil or tat < now then tat = now end
local new_tat = tat + emission
local allow_at = new_tat - tolerance

if allow_at > now then
  local reset = tat - now
  if reset < 0 then reset = 0 end
  return {0, 0, math.ceil(allow_at - now), math.ceil(reset)}
end

redis.call('SET', KEYS[1], string.format('%.0f', new_tat), 'PX', ttl)
local remaining = math.floor((now + tolerance - new_tat) / emission)
if remaining < 0 then remaining = 0 end
return {1, remaining, 0, math.ceil(new_tat - now)}
";

/// Now, in microseconds since the Unix epoch.
///
/// Wall clock and not [`Instant`](std::time::Instant), because the state is
/// shared between processes and a monotonic clock is not comparable across
/// them. A clock that jumps backwards makes the limiter briefly more
/// permissive, which is the safe direction.
fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_micros()).unwrap_or(u64::MAX)
        })
}

impl Kv {
    /// The key a rate-limit bucket occupies.
    ///
    /// # Errors
    ///
    /// [`Error::Key`] when the resulting key is too long.
    ///
    /// ```
    /// use moso_kv::Kv;
    ///
    /// let kv = Kv::in_memory("shop").expect("built");
    /// assert_eq!(
    ///     kv.rate_key("ip:127.0.0.1").expect("short").as_str(),
    ///     "moso:v1:shop:rate:1:ip\\c127.0.0.1",
    /// );
    /// ```
    pub fn rate_key(&self, bucket: &str) -> Result<Key> {
        let mut buf = KeyBuf::new(self.app(), RATE_PREFIX, RATE_VERSION)?;
        buf.segment_str(bucket);
        Ok(buf.finish()?)
    }

    /// Charge one request against `bucket`.
    ///
    /// Uses [`GCRA_SCRIPT`] when the backend has scripting, and a
    /// compare-and-swap loop otherwise. Both give the same answer.
    ///
    /// # Errors
    ///
    /// A backend failure. Rate limiting never degrades: a limiter that quietly
    /// stops limiting when the store blinks is worse than one that says so, and
    /// the caller decides what a 503 from here means.
    ///
    /// ```
    /// use moso_kv::{Kv, RateQuota, Result};
    /// use std::time::Duration;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let quota = RateQuota::new(2, Duration::from_secs(60));
    ///
    /// assert!(kv.rate_limit("ip:1.2.3.4", quota).await?.allowed);
    /// assert!(kv.rate_limit("ip:1.2.3.4", quota).await?.allowed);
    ///
    /// let third = kv.rate_limit("ip:1.2.3.4", quota).await?;
    /// assert!(!third.allowed);
    /// assert_eq!(third.remaining, 0);
    /// assert!(third.retry_after > Duration::ZERO);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn rate_limit(&self, bucket: &str, quota: RateQuota) -> Result<RateDecision> {
        use tracing::Instrument as _;

        // One span over the charge, whichever path it takes — the scripted
        // single round trip or the compare-and-swap loop — so rate-limit work
        // is one `op="rate"` unit in a request trace.
        self.rate_limit_inner(bucket, quota)
            .instrument(self.op_span("rate"))
            .await
    }

    async fn rate_limit_inner(&self, bucket: &str, quota: RateQuota) -> Result<RateDecision> {
        let key = self.rate_key(bucket)?;
        if self.capabilities().scripting {
            return self.rate_limit_scripted(&key, quota).await;
        }
        self.rate_limit_cas(&key, quota).await
    }

    /// The one-round-trip path.
    async fn rate_limit_scripted(&self, key: &Key, quota: RateQuota) -> Result<RateDecision> {
        let args = [
            Bytes::from(now_us().to_string().into_bytes()),
            Bytes::from(quota.emission_us().to_string().into_bytes()),
            Bytes::from(quota.tolerance_us().to_string().into_bytes()),
            Bytes::from(
                u64::try_from(quota.state_ttl().as_millis())
                    .unwrap_or(u64::MAX)
                    .to_string()
                    .into_bytes(),
            ),
        ];
        let reply = self
            .store()
            .eval(GCRA_SCRIPT, std::slice::from_ref(key), &args)
            .await?;

        let [allowed, remaining, retry_after_us, reset_us] = reply[..] else {
            return Err(Error::backend(
                self.store().name(),
                "eval",
                format!(
                    "the rate-limit script returned {} values, not 4",
                    reply.len()
                ),
            ));
        };

        Ok(RateDecision {
            allowed: allowed != 0,
            limit: quota.limit,
            remaining: u32::try_from(remaining.max(0)).unwrap_or(u32::MAX),
            retry_after: Duration::from_micros(retry_after_us.max(0).unsigned_abs()),
            reset: Duration::from_micros(reset_us.max(0).unsigned_abs()),
        })
    }

    /// The portable path: read, decide, conditionally write.
    async fn rate_limit_cas(&self, key: &Key, quota: RateQuota) -> Result<RateDecision> {
        let emission = quota.emission_us();
        let tolerance = quota.tolerance_us();
        let ttl = quota.state_ttl();

        for _ in 0..MAX_CAS_ATTEMPTS {
            let current = self.store().get(key).await?;
            let now = now_us();
            let stored = current
                .as_deref()
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .and_then(|text| text.trim().parse::<u64>().ok());

            let tat = stored.unwrap_or(now).max(now);
            let new_tat = tat.saturating_add(emission);
            let allow_at = new_tat.saturating_sub(tolerance);

            if allow_at > now {
                return Ok(RateDecision {
                    allowed: false,
                    limit: quota.limit,
                    remaining: 0,
                    retry_after: Duration::from_micros(allow_at - now),
                    reset: Duration::from_micros(tat.saturating_sub(now)),
                });
            }

            let swapped = self
                .store()
                .compare_and_swap(
                    key,
                    current.as_deref(),
                    Bytes::from(new_tat.to_string().into_bytes()),
                    SetOpts::new().ttl(ttl),
                )
                .await?;

            if swapped {
                let head_room = (now + tolerance).saturating_sub(new_tat);
                return Ok(RateDecision {
                    allowed: true,
                    limit: quota.limit,
                    remaining: u32::try_from(head_room / emission).unwrap_or(u32::MAX),
                    retry_after: Duration::ZERO,
                    reset: Duration::from_micros(new_tat.saturating_sub(now)),
                });
            }
        }

        // Thirty-two lost races on one bucket means the bucket is hotter than
        // its own quota, so refusing is both the safe answer and the true one.
        tracing::warn!(
            bucket = key.parts(),
            attempts = MAX_CAS_ATTEMPTS,
            "the rate limiter could not settle its compare-and-swap; refusing"
        );
        Ok(RateDecision {
            allowed: false,
            limit: quota.limit,
            remaining: 0,
            retry_after: quota.emission_interval(),
            reset: quota.tolerance(),
        })
    }
}

// ---------------------------------------------------------------------------
// RateKey
// ---------------------------------------------------------------------------

/// A request extension naming who the request is, for [`RateKey::User`].
///
/// Inserted by whatever knows: an authentication middleware, an API-key guard,
/// a tenant resolver. `moso-kv` deliberately does not know how identity works.
///
/// ```
/// use moso_kv::RateSubject;
///
/// let subject = RateSubject::new("user:42");
/// assert_eq!(subject.as_str(), "user:42");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateSubject(String);

impl RateSubject {
    /// Name the subject.
    ///
    /// ```
    /// use moso_kv::RateSubject;
    ///
    /// assert_eq!(RateSubject::new("tenant:acme").as_str(), "tenant:acme");
    /// ```
    #[must_use]
    pub fn new(subject: impl Into<String>) -> Self {
        Self(subject.into())
    }

    /// The subject.
    ///
    /// ```
    /// use moso_kv::RateSubject;
    ///
    /// assert_eq!(RateSubject::new("a").as_str(), "a");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The closure a [`RateKey::Custom`] holds.
///
/// A named alias rather than the type written out: it appears in the variant,
/// in [`RateKey::custom`]'s bound and in every error message about either.
pub type BucketFn = Arc<dyn Fn(&Parts) -> Option<String> + Send + Sync>;

/// What a request is bucketed by.
///
/// Every variant can come up empty — no peer address, no subject, no header —
/// and the rule for what happens then is one sentence, stated once: **a request
/// with no key falls back to its peer address, and one with no peer address
/// falls into a single shared `unknown` bucket.** Sharing a bucket is the safe
/// direction; the alternative is an unbounded number of buckets, or an
/// unlimited request.
///
/// ```
/// use moso_kv::RateKey;
///
/// assert_eq!(RateKey::Ip.as_str(), "ip");
/// assert_eq!(RateKey::header("x-api-key").as_str(), "header");
/// ```
#[derive(Clone)]
pub enum RateKey {
    /// The client's IP address.
    Ip,
    /// The [`RateSubject`] extension.
    User,
    /// The value of a request header.
    Header(HeaderName),
    /// One bucket for every request through this route.
    Global,
    /// Anything else.
    Custom(BucketFn),
}

impl std::fmt::Debug for RateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RateKey::Header(name) => f.debug_tuple("Header").field(&name.as_str()).finish(),
            RateKey::Custom(_) => f.write_str("Custom(..)"),
            other => f.write_str(other.as_str()),
        }
    }
}

impl RateKey {
    /// Bucket by a header, by name.
    ///
    /// # Panics
    ///
    /// If `name` is not a valid header name. It is a `&'static str` written in
    /// the composition root, so an invalid one is a typo the author sees on the
    /// first run.
    ///
    /// ```
    /// use moso_kv::RateKey;
    ///
    /// assert_eq!(RateKey::header("x-api-key").as_str(), "header");
    /// ```
    #[must_use]
    pub fn header(name: &'static str) -> Self {
        RateKey::Header(HeaderName::from_static(name))
    }

    /// Bucket by a closure over the request head.
    ///
    /// ```
    /// use moso_kv::RateKey;
    ///
    /// // One bucket per HTTP method, which nobody wants but which shows the
    /// // shape.
    /// let key = RateKey::custom(|parts| Some(parts.method.to_string()));
    /// assert_eq!(key.as_str(), "custom");
    /// ```
    #[must_use]
    pub fn custom(f: impl Fn(&Parts) -> Option<String> + Send + Sync + 'static) -> Self {
        RateKey::Custom(Arc::new(f))
    }

    /// The variant's name, for a log field and for the bucket prefix.
    ///
    /// ```
    /// use moso_kv::RateKey;
    ///
    /// assert_eq!(RateKey::Global.as_str(), "global");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            RateKey::Ip => "ip",
            RateKey::User => "user",
            RateKey::Header(_) => "header",
            RateKey::Global => "global",
            RateKey::Custom(_) => "custom",
        }
    }

    /// This request's bucket, or `None` when the variant has nothing to say.
    ///
    /// ```
    /// use moso_kv::RateKey;
    ///
    /// let (mut parts, _) = http::Request::new(()).into_parts();
    /// parts.headers.insert("x-api-key", "abc".parse().expect("valid"));
    ///
    /// assert_eq!(RateKey::header("x-api-key").of(&parts).as_deref(), Some("abc"));
    /// assert_eq!(RateKey::Global.of(&parts).as_deref(), Some(""));
    /// assert_eq!(RateKey::User.of(&parts), None);
    /// ```
    #[must_use]
    pub fn of(&self, parts: &Parts) -> Option<String> {
        match self {
            RateKey::Ip => None,
            RateKey::Global => Some(String::new()),
            RateKey::User => parts
                .extensions
                .get::<RateSubject>()
                .map(|subject| subject.0.clone()),
            RateKey::Header(name) => parts
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
            RateKey::Custom(f) => f(parts),
        }
    }
}

// ---------------------------------------------------------------------------
// RateLimit
// ---------------------------------------------------------------------------

/// A rate limit, as a [`Guard`].
///
/// Attached with `Router::guard`, so it both enforces the limit and documents
/// the 429 it can return.
///
/// ```
/// use moso_kv::{RateKey, RateLimit, RateQuota};
/// use std::time::Duration;
///
/// let guard = RateLimit::new(RateQuota::new(10, Duration::from_secs(60)).burst(3))
///     .key(RateKey::Ip);
///
/// assert_eq!(guard.quota().limit, 10);
/// assert_eq!(guard.quota().burst, 3);
/// ```
#[derive(Clone)]
pub struct RateLimit {
    quota: RateQuota,
    key: RateKey,
    scope: Arc<str>,
    kv: Option<Kv>,
}

impl std::fmt::Debug for RateLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimit")
            .field("quota", &self.quota)
            .field("key", &self.key)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl RateLimit {
    /// A limit of `quota`, bucketed by IP, in the default scope.
    ///
    /// ```
    /// use moso_kv::{RateLimit, RateQuota};
    /// use std::time::Duration;
    ///
    /// let guard = RateLimit::new(RateQuota::new(100, Duration::from_secs(60)));
    /// assert_eq!(guard.scope_name(), "default");
    /// ```
    #[must_use]
    pub fn new(quota: RateQuota) -> Self {
        Self {
            quota,
            key: RateKey::Ip,
            scope: Arc::from("default"),
            kv: None,
        }
    }

    /// Bucket by something other than the IP.
    ///
    /// ```
    /// use moso_kv::{RateKey, RateLimit, RateQuota};
    /// use std::time::Duration;
    ///
    /// let guard = RateLimit::new(RateQuota::new(10, Duration::from_secs(60)))
    ///     .key(RateKey::header("x-api-key"));
    /// assert_eq!(guard.key_kind().as_str(), "header");
    /// ```
    #[must_use]
    pub fn key(mut self, key: RateKey) -> Self {
        self.key = key;
        self
    }

    /// Name this limit, so two limits on one client do not share a bucket.
    ///
    /// A login endpoint limited to 10/minute and a search endpoint limited to
    /// 100/minute must not consume each other's quota; the scope is what keeps
    /// them apart.
    ///
    /// ```
    /// use moso_kv::{RateLimit, RateQuota};
    /// use std::time::Duration;
    ///
    /// let guard = RateLimit::new(RateQuota::new(10, Duration::from_secs(60))).scope("login");
    /// assert_eq!(guard.scope_name(), "login");
    /// ```
    #[must_use]
    pub fn scope(mut self, scope: impl AsRef<str>) -> Self {
        self.scope = Arc::from(scope.as_ref());
        self
    }

    /// Use this [`Kv`] rather than resolving one from the provider map.
    ///
    /// ```
    /// use moso_kv::{Kv, RateLimit, RateQuota};
    /// use std::time::Duration;
    ///
    /// let kv = Kv::in_memory("shop").expect("built");
    /// let guard = RateLimit::new(RateQuota::new(10, Duration::from_secs(60))).with_kv(kv);
    /// assert!(guard.kv().is_some());
    /// ```
    #[must_use]
    pub fn with_kv(mut self, kv: Kv) -> Self {
        self.kv = Some(kv);
        self
    }

    /// The quota.
    ///
    /// ```
    /// use moso_kv::{RateLimit, RateQuota};
    /// use std::time::Duration;
    ///
    /// assert_eq!(
    ///     RateLimit::new(RateQuota::new(5, Duration::from_secs(1))).quota().limit,
    ///     5,
    /// );
    /// ```
    #[must_use]
    pub fn quota(&self) -> RateQuota {
        self.quota
    }

    /// What requests are bucketed by.
    ///
    /// Named `key_kind` and not `key` because [`key`](Self::key) is the
    /// builder: one identifier cannot mean both "set this" and "read this".
    ///
    /// ```
    /// use moso_kv::{RateLimit, RateQuota};
    /// use std::time::Duration;
    ///
    /// assert_eq!(
    ///     RateLimit::new(RateQuota::new(5, Duration::from_secs(1))).key_kind().as_str(),
    ///     "ip",
    /// );
    /// ```
    #[must_use]
    pub fn key_kind(&self) -> &RateKey {
        &self.key
    }

    /// This limit's scope.
    ///
    /// ```
    /// use moso_kv::{RateLimit, RateQuota};
    /// use std::time::Duration;
    ///
    /// assert_eq!(
    ///     RateLimit::new(RateQuota::new(5, Duration::from_secs(1))).scope_name(),
    ///     "default",
    /// );
    /// ```
    #[must_use]
    pub fn scope_name(&self) -> &str {
        &self.scope
    }

    /// The handle this limit was built with, if it was built with one.
    ///
    /// ```
    /// use moso_kv::{RateLimit, RateQuota};
    /// use std::time::Duration;
    ///
    /// assert!(RateLimit::new(RateQuota::new(5, Duration::from_secs(1))).kv().is_none());
    /// ```
    #[must_use]
    pub fn kv(&self) -> Option<&Kv> {
        self.kv.as_ref()
    }

    /// The bucket name this request falls into.
    ///
    /// `<scope>:<kind>:<value>`, so two scopes never share a bucket and two
    /// *kinds* within a scope never do either.
    ///
    /// ```
    /// use moso_kv::{RateKey, RateLimit, RateQuota};
    /// use std::time::Duration;
    ///
    /// let guard = RateLimit::new(RateQuota::new(10, Duration::from_secs(60)))
    ///     .key(RateKey::header("x-api-key"))
    ///     .scope("login");
    ///
    /// let (mut parts, _) = http::Request::new(()).into_parts();
    /// parts.headers.insert("x-api-key", "abc".parse().expect("valid"));
    /// assert_eq!(guard.bucket(&parts), "login:header:abc");
    ///
    /// // With no header and no peer address, everything shares one bucket.
    /// let (bare, _) = http::Request::new(()).into_parts();
    /// assert_eq!(guard.bucket(&bare), "login:ip:unknown");
    /// ```
    #[must_use]
    pub fn bucket(&self, parts: &Parts) -> String {
        match self.key.of(parts) {
            Some(value) => format!("{}:{}:{}", self.scope, self.key.as_str(), value),
            None => format!("{}:ip:{}", self.scope, peer_ip(parts)),
        }
    }

    /// Decide, given an already-resolved [`Kv`].
    ///
    /// Split out from [`Guard::check`] so a test can exercise every branch
    /// without an `AppState`.
    ///
    /// # Errors
    ///
    /// A backend failure.
    ///
    /// ```
    /// use moso_kv::{Kv, RateLimit, RateQuota, Result};
    /// use std::time::Duration;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let guard = RateLimit::new(RateQuota::new(1, Duration::from_secs(60)));
    /// let (parts, _) = http::Request::new(()).into_parts();
    ///
    /// assert!(guard.decide(&kv, &parts).await?.allowed);
    /// assert!(!guard.decide(&kv, &parts).await?.allowed);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn decide(&self, kv: &Kv, parts: &Parts) -> Result<RateDecision> {
        kv.rate_limit(&self.bucket(parts), self.quota).await
    }
}

/// The peer address as a bucket component, or `unknown`.
///
/// Deliberately **not** `X-Forwarded-For`: an IP taken from a header a client
/// controls turns a rate limiter into a rate-limit bypass. `moso-core`'s
/// [`ClientIp`](moso_core::extract::ClientIp) does honour the header, but only
/// for a peer that `http.trusted_proxies` names, and it needs a `&mut Parts`
/// that a `Guard` does not have. Use
/// [`RateKey::custom`] with `ClientIp` in a handler-side limiter when running
/// behind a trusted proxy.
fn peer_ip(parts: &Parts) -> String {
    parts
        .extensions
        .get::<moso_core::deps::axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map_or_else(|| String::from("unknown"), |info| info.0.ip().to_string())
}

impl Guard for RateLimit {
    fn describe(&self, op: &mut OperationBuilder) {
        let integer = moso_schema::json_schema::NumberBuilder::integer().build();
        op.response(
            429,
            ResponseSpec::problem(format!(
                "Rate limit exceeded: at most {} request(s) per {}, bucketed by {}.",
                self.quota.limit,
                humantime::format_duration(self.quota.period),
                self.key.as_str(),
            ))
            .header(HEADER_LIMIT.as_str(), integer.clone())
            .header(HEADER_REMAINING.as_str(), integer.clone())
            .header(HEADER_RESET.as_str(), integer.clone())
            .header(http::header::RETRY_AFTER.as_str(), integer),
        );
    }

    fn check<'a>(
        &'a self,
        parts: &'a Parts,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, moso_core::Result<()>> {
        Box::pin(async move {
            let kv = match &self.kv {
                Some(kv) => kv.clone(),
                None => {
                    let provided = ctx.provider::<Kv>()?;
                    Kv::clone(&provided)
                }
            };

            let decision = self.decide(&kv, parts).await?;
            if decision.allowed {
                Ok(())
            } else {
                Err(decision.into_error())
            }
        })
    }
}

#[cfg(all(test, feature = "memory"))]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn kv() -> Kv {
        Kv::in_memory("shop").expect("built")
    }

    fn parts() -> Parts {
        http::Request::new(()).into_parts().0
    }

    #[test]
    fn a_quota_computes_its_intervals() {
        let quota = RateQuota::new(10, Duration::from_secs(60));
        assert_eq!(quota.emission_interval(), Duration::from_secs(6));
        assert_eq!(quota.tolerance(), Duration::from_secs(60));
        assert_eq!(quota.emission_us(), 6_000_000);
        assert_eq!(quota.tolerance_us(), 60_000_000);
    }

    #[test]
    fn a_burst_is_clamped_to_the_limit_and_never_zero() {
        let quota = RateQuota::new(10, Duration::from_secs(60));
        assert_eq!(quota.burst(99).burst, 10);
        assert_eq!(quota.burst(0).burst, 1);
        assert_eq!(RateQuota::new(0, Duration::from_secs(1)).limit, 1);
    }

    #[tokio::test]
    async fn ten_a_minute_admits_exactly_ten() {
        let kv = kv();
        let quota = RateQuota::new(10, Duration::from_secs(60));

        let mut admitted = 0;
        for _ in 0..25 {
            if kv.rate_limit("b", quota).await.expect("decided").allowed {
                admitted += 1;
            }
        }
        assert_eq!(admitted, 10);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ten_a_minute_admits_exactly_ten_across_four_workers() {
        let kv = kv();
        let quota = RateQuota::new(10, Duration::from_secs(60));
        let admitted = StdArc::new(AtomicU32::new(0));

        let mut workers = Vec::new();
        for _ in 0..4 {
            let kv = kv.clone();
            let admitted = StdArc::clone(&admitted);
            workers.push(tokio::spawn(async move {
                for _ in 0..25 {
                    if kv
                        .rate_limit("shared", quota)
                        .await
                        .expect("decided")
                        .allowed
                    {
                        admitted.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }));
        }
        for worker in workers {
            worker.await.expect("joined");
        }

        assert_eq!(admitted.load(Ordering::SeqCst), 10);
    }

    #[tokio::test]
    async fn remaining_counts_down_and_the_headers_say_so() {
        let kv = kv();
        let quota = RateQuota::new(3, Duration::from_secs(60));

        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(kv.rate_limit("count", quota).await.expect("decided"));
        }

        assert_eq!(
            seen.iter().map(|d| d.remaining).collect::<Vec<_>>(),
            vec![2, 1, 0, 0]
        );
        assert!(seen[..3].iter().all(|d| d.allowed));
        assert!(!seen[3].allowed);

        let headers = seen[0].headers();
        assert_eq!(headers[0].0, HEADER_LIMIT);
        assert_eq!(headers[0].1, "3");
        assert_eq!(headers[1].1, "2");
        assert_eq!(headers.len(), 3, "no Retry-After when allowed");

        let denied = seen[3].headers();
        assert_eq!(denied.len(), 4);
        assert_eq!(denied[1].1, "0");
        assert_eq!(denied[3].0, http::header::RETRY_AFTER);
    }

    #[tokio::test]
    async fn a_burst_smaller_than_the_limit_smooths_traffic() {
        let kv = kv();
        let quota = RateQuota::new(60, Duration::from_secs(60)).burst(3);

        let mut admitted = 0;
        for _ in 0..10 {
            if kv
                .rate_limit("smooth", quota)
                .await
                .expect("decided")
                .allowed
            {
                admitted += 1;
            }
        }
        assert_eq!(admitted, 3, "only the burst arrives at once");
    }

    #[tokio::test]
    async fn the_bucket_refills_over_time() {
        let kv = kv();
        // One every 20 ms, burst of one.
        let quota = RateQuota::new(50, Duration::from_secs(1)).burst(1);

        assert!(
            kv.rate_limit("refill", quota)
                .await
                .expect("decided")
                .allowed
        );
        assert!(
            !kv.rate_limit("refill", quota)
                .await
                .expect("decided")
                .allowed
        );

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            kv.rate_limit("refill", quota)
                .await
                .expect("decided")
                .allowed,
            "the bucket did not refill"
        );
    }

    #[tokio::test]
    async fn buckets_are_independent() {
        let kv = kv();
        let quota = RateQuota::new(1, Duration::from_secs(60));
        assert!(kv.rate_limit("a", quota).await.expect("decided").allowed);
        assert!(kv.rate_limit("b", quota).await.expect("decided").allowed);
        assert!(!kv.rate_limit("a", quota).await.expect("decided").allowed);
    }

    #[test]
    fn a_bucket_name_says_scope_kind_and_value() {
        let guard = RateLimit::new(RateQuota::new(10, Duration::from_secs(60)))
            .key(RateKey::header("x-api-key"))
            .scope("login");

        let mut parts = parts();
        parts
            .headers
            .insert("x-api-key", "abc".parse().expect("valid"));
        assert_eq!(guard.bucket(&parts), "login:header:abc");
        assert_eq!(guard.bucket(&self::parts()), "login:ip:unknown");
    }

    #[test]
    fn every_key_kind_resolves_or_falls_back() {
        let mut parts = parts();
        parts.extensions.insert(RateSubject::new("user:7"));
        parts
            .headers
            .insert("x-api-key", "k".parse().expect("valid"));

        assert_eq!(RateKey::User.of(&parts).as_deref(), Some("user:7"));
        assert_eq!(
            RateKey::header("x-api-key").of(&parts).as_deref(),
            Some("k")
        );
        assert_eq!(RateKey::Global.of(&parts).as_deref(), Some(""));
        assert_eq!(RateKey::Ip.of(&parts), None);
        assert_eq!(
            RateKey::custom(|p| Some(p.method.to_string()))
                .of(&parts)
                .as_deref(),
            Some("GET")
        );

        // An absent subject falls through to the peer address.
        let guard = RateLimit::new(RateQuota::new(1, Duration::from_secs(1))).key(RateKey::User);
        assert_eq!(guard.bucket(&self::parts()), "default:ip:unknown");
    }

    #[test]
    fn the_peer_address_is_used_when_the_connection_reported_one() {
        let mut parts = parts();
        parts
            .extensions
            .insert(moso_core::deps::axum::extract::ConnectInfo(
                std::net::SocketAddr::from(([203, 0, 113, 7], 4242)),
            ));
        assert_eq!(peer_ip(&parts), "203.0.113.7");

        let guard = RateLimit::new(RateQuota::new(1, Duration::from_secs(1)));
        assert_eq!(guard.bucket(&parts), "default:ip:203.0.113.7");
    }

    #[tokio::test]
    async fn two_scopes_do_not_share_a_bucket() {
        let kv = kv();
        let quota = RateQuota::new(1, Duration::from_secs(60));
        let login = RateLimit::new(quota).scope("login");
        let search = RateLimit::new(quota).scope("search");
        let parts = parts();

        assert!(login.decide(&kv, &parts).await.expect("decided").allowed);
        assert!(search.decide(&kv, &parts).await.expect("decided").allowed);
        assert!(!login.decide(&kv, &parts).await.expect("decided").allowed);
    }

    #[test]
    fn a_decision_renders_a_429_with_every_header() {
        let denied = RateDecision {
            allowed: false,
            limit: 10,
            remaining: 0,
            retry_after: Duration::from_millis(1_500),
            reset: Duration::from_secs(42),
        };
        let error = denied.into_error();
        assert_eq!(error.status(), http::StatusCode::TOO_MANY_REQUESTS);
        let headers = error.headers().expect("headers");
        assert_eq!(headers[HEADER_LIMIT], "10");
        assert_eq!(headers[HEADER_REMAINING], "0");
        assert_eq!(headers[HEADER_RESET], "42");
        assert_eq!(headers[http::header::RETRY_AFTER], "2");
    }

    #[test]
    fn the_guard_documents_the_429_and_its_headers() {
        use moso_openapi::SchemaGenerator;

        let guard = RateLimit::new(RateQuota::new(10, Duration::from_secs(60)));
        let mut op =
            OperationBuilder::new(SchemaGenerator::new(moso_core::COMPONENTS_SCHEMAS_PREFIX));
        guard.describe(&mut op);
        let spec = op.into_spec();

        let response = spec.responses.get("429").expect("documented");
        assert!(
            response
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("10 request"),
            "{:?}",
            response.description
        );
        assert!(response.headers.contains_key(HEADER_LIMIT.as_str()));
        assert!(response.headers.contains_key(HEADER_RESET.as_str()));
        assert!(response.headers.contains_key("retry-after"));
    }

    #[test]
    fn it_is_usable_as_the_trait_object_the_route_table_holds() {
        let guard: StdArc<dyn moso_core::router::DynGuard> =
            StdArc::new(RateLimit::new(RateQuota::new(1, Duration::from_secs(1))));
        let mut op = OperationBuilder::new(moso_openapi::SchemaGenerator::new(
            moso_core::COMPONENTS_SCHEMAS_PREFIX,
        ));
        guard.describe_dyn(&mut op);
        assert!(op.into_spec().responses.contains_key("429"));
    }

    #[test]
    fn it_describes_itself_without_leaking_a_closure() {
        let guard = RateLimit::new(RateQuota::new(1, Duration::from_secs(1)))
            .key(RateKey::custom(|_| None));
        let rendered = format!("{guard:?}");
        assert!(rendered.contains("Custom(..)"), "{rendered}");
        assert!(format!("{:?}", RateKey::Ip).contains("ip"));
        assert!(format!("{:?}", RateKey::header("x-a")).contains("x-a"));
    }

    #[test]
    fn ceil_secs_rounds_up_and_never_invites_an_early_retry() {
        assert_eq!(ceil_secs(Duration::ZERO), 0);
        assert_eq!(ceil_secs(Duration::from_millis(1)), 1);
        assert_eq!(ceil_secs(Duration::from_secs(2)), 2);
        assert_eq!(ceil_secs(Duration::from_millis(2_001)), 3);
    }

    #[test]
    fn the_script_is_the_documented_shape() {
        // Not executed here — no Redis in a unit test — but its contract is
        // asserted so a careless edit is caught: four `ARGV`, one `KEYS`, and a
        // formatted write so a microsecond timestamp survives the round trip.
        assert!(GCRA_SCRIPT.contains("KEYS[1]"));
        for index in 1..=4 {
            assert!(
                GCRA_SCRIPT.contains(&format!("ARGV[{index}]")),
                "ARGV[{index}]"
            );
        }
        assert!(GCRA_SCRIPT.contains("string.format('%.0f'"));
        assert!(!GCRA_SCRIPT.contains("tostring("));
    }
}
