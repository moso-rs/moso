//! The pieces every mounted flow shares: the request facts a throttle keys on,
//! the throttle gate itself, and the opaque handle a session listing hands out.
//!
//! Nothing here is a policy of its own. Each function is the single place one
//! decision is written down, so two handlers cannot disagree about it:
//!
//! | Function | The decision it owns |
//! | --- | --- |
//! | [`auth_ctx`] | what a throttle sees about a request |
//! | [`gate`] | what a [`ThrottleDecision`] means before any hashing happens |
//! | [`record`] | that an outcome is always recorded, and never fails a request |
//! | [`handle_of`] | that a session is named to a client by a digest, never by its id |
//! | [`subject_of`] | that an unauthenticated request is one 401, whatever the reason |

use std::borrow::Cow;

use chrono::{DateTime, Utc};
use moso_core::extract::ClientIp;
use serde::{Deserialize, Serialize};

use super::AuthState;
use crate::{AuthCtx, Error, Result, Session, SessionId, ThrottleDecision};

/// The headers the credential routes read.
///
/// A struct rather than three ad-hoc lookups, because
/// [`Headers`](moso_core::extract::Headers) documents every field it declares as
/// a parameter — so the CAPTCHA header appears in the OpenAPI document instead
/// of being folklore.
#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct ClientHeaders {
    /// What the request says it is, recorded on the attempt so a security page
    /// can show "Firefox on macOS" rather than nothing.
    pub(crate) user_agent: Option<String>,
    /// The CAPTCHA provider's response token, when the throttle asked for one.
    pub(crate) x_captcha_response: Option<String>,
}

dto! {
    ClientHeaders, "The headers a credential route reads.",
    user_agent: Option<String> = "The client's `User-Agent`.", false;
    x_captcha_response: Option<String> = "A CAPTCHA response token, when one was demanded.", false;
}

/// What the throttle and the attempt log are told about a request.
///
/// The address comes from [`ClientIp`], which walks `X-Forwarded-For` only as
/// far as the configured trusted proxies allow. Reading the header directly
/// would let an attacker pick their own throttle bucket, which is the same as
/// having no per-address throttle at all.
pub(crate) fn auth_ctx(
    address: Option<&ClientIp>,
    headers: &ClientHeaders,
    identity: Option<&str>,
) -> AuthCtx {
    let mut ctx = AuthCtx::new();
    if let Some(ClientIp(address)) = address {
        ctx = ctx.with_ip(address.to_string());
    }
    if let Some(agent) = headers.user_agent.as_deref() {
        ctx = ctx.with_user_agent(agent);
    }
    if let Some(identity) = identity {
        ctx = ctx.with_identity(identity);
    }
    ctx
}

/// Ask the throttle whether this attempt may proceed.
///
/// Called **before** any password hashing, which is the point: hashing is the
/// expensive operation an attacker is trying to make the server do, and a
/// refused attempt must not pay for one.
///
/// A [`ThrottleDecision::Challenge`] is resolved here rather than in each
/// handler, and the resolution is deliberately strict: with no
/// [`CaptchaVerifier`](crate::CaptchaVerifier) configured, or with one that says
/// the token did not check out, a challenge becomes a refusal. Treating
/// "we cannot check" as "let them through" would make the challenge tier a way
/// to *skip* the throttle rather than a way to slow it down.
///
/// # Errors
///
/// [`Error::RateLimited`] when the attempt is refused, or
/// [`Error::Unavailable`] when the throttle store could not be reached — never
/// an `Ok` on a store failure, which is what fail-closed means.
pub(crate) async fn gate(
    state: &AuthState,
    ctx: &AuthCtx,
    captcha_response: Option<&str>,
) -> Result<()> {
    let Some(throttle) = state.login_throttle() else {
        return Ok(());
    };

    match throttle.check(ctx).await? {
        ThrottleDecision::Allow => Ok(()),
        ThrottleDecision::Deny { retry_after } => Err(Error::RateLimited { retry_after }),
        ThrottleDecision::Challenge => {
            let cleared = match (state.captcha_verifier(), captcha_response) {
                (Some(verifier), Some(token)) => verifier.verify(token, ctx.ip()).await?,
                _ => false,
            };

            if cleared {
                return Ok(());
            }
            Err(Error::RateLimited {
                retry_after: throttle.config().per_identity_base,
            })
        }
    }
}

/// Record the outcome of an attempt, and never fail the request for it.
///
/// The attempt has already happened by the time this runs. Turning a throttle
/// store blip into a 503 *after* a successful login would tell the user their
/// login failed when it did not, and would leave them signed in anyway.
pub(crate) async fn record(state: &AuthState, ctx: &AuthCtx, succeeded: bool) {
    let Some(throttle) = state.login_throttle() else {
        return;
    };

    if let Err(error) = throttle.record(ctx, succeeded).await {
        tracing::warn!(
            target: "moso.auth",
            %error,
            "an attempt outcome could not be recorded; the throttle will under-count"
        );
    }
}

/// The opaque handle a session listing names a session by.
///
/// The SHA-256 of the identifier, hex: stable, so a client can revoke what it
/// listed; one-way, so the listing is not a list of live credentials; and
/// derived, so nothing extra has to be stored for it.
pub(crate) fn handle_of(id: &SessionId) -> String {
    crate::jwks::sha256_hex(id.as_str().as_bytes())
}

/// Who this session belongs to, or the one 401 every unauthenticated request
/// gets.
///
/// # Errors
///
/// [`Error::Unauthenticated`] when the session names nobody.
pub(crate) fn subject_of(session: &Session) -> Result<String> {
    session.user_id().ok_or(Error::Unauthenticated)
}

/// A timestamp, as every response in this module spells one.
pub(crate) fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339()
}

/// How many bytes of entropy the opaque tokens this module mints carry.
///
/// Thirty-two, the same as a session identifier: these tokens stand in for one
/// for the length of a ceremony, so anything less would make the ceremony the
/// weakest step.
pub(crate) const OPAQUE_TOKEN_BYTES: usize = 32;

/// An opaque, unguessable token, from the operating system's CSPRNG.
///
/// # Errors
///
/// [`Error::Unavailable`] when the system random generator refuses, which is a
/// 503 rather than a panic: a container out of file descriptors is diagnosable,
/// an unwrap inside a token mint is not.
pub(crate) fn opaque_token() -> Result<String> {
    Ok(crate::jwks::b64u(&crate::jwks::random_bytes(
        OPAQUE_TOKEN_BYTES,
    )?))
}

/// Read a `next` from a request and check it against the allowlist.
///
/// `None` stays `None`; anything present must survive
/// [`validate_next`](super::validate_next) before it is stored, returned or
/// redirected to — including on the path where it is only *stored*, so a
/// tampered session cannot produce an open redirect later.
///
/// # Errors
///
/// [`Error::Ceremony`] when the target is not allowed.
pub(crate) fn checked_next(next: Option<&str>, allowlist: &[String]) -> Result<Option<String>> {
    match next {
        None => Ok(None),
        Some(next) => {
            super::validate_next(next, allowlist)?;
            Ok(Some(next.to_owned()))
        }
    }
}

/// The failure a route gives when a request carries no body it can act on.
///
/// # Errors
///
/// Always. This is a constructor.
pub(crate) fn ceremony(reason: impl Into<Cow<'static, str>>) -> Error {
    Error::ceremony("route", reason)
}

/// A key-value failure, named for the thing that was being stored.
///
/// Always [`Error::Unavailable`], which is a 503. Degrading it into "no
/// enrolment found" would turn a store outage into a second factor that
/// silently switched itself off.
pub(crate) fn kv_failed(
    component: &'static str,
    operation: &'static str,
    error: moso_kv::Error,
) -> Error {
    Error::Unavailable {
        component,
        detail: format!("{operation}: {error}"),
        source: Some(Box::new(error)),
    }
}

/// The key an identity or a subject occupies in a key-value store.
///
/// The digest, never the value: a key can leak through a `SCAN`, a slow-log
/// entry or a backend error message quoting the key it failed on, and none of
/// those should hand over the list of accounts that have a second factor.
pub(crate) fn keyed(subject: &str) -> String {
    crate::jwks::sha256_hex(subject.as_bytes())
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::{LoginThrottle, ThrottleConfig};

    fn state() -> AuthState {
        AuthState::new(crate::MemorySessionStore::shared())
    }

    fn headers() -> ClientHeaders {
        ClientHeaders {
            user_agent: Some("Firefox".to_owned()),
            x_captcha_response: None,
        }
    }

    // ── the request facts ─────────────────────────────────────────────────

    #[test]
    fn the_throttle_sees_the_resolved_address_and_the_normalised_identity() {
        let address = ClientIp(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));
        let ctx = auth_ctx(Some(&address), &headers(), Some("  ADA@Example.COM "));

        assert_eq!(ctx.ip(), Some("203.0.113.7"));
        assert_eq!(ctx.user_agent(), Some("Firefox"));
        assert_eq!(ctx.identity(), Some("ada@example.com"));
    }

    #[test]
    fn a_request_with_no_address_still_produces_a_context() {
        let ctx = auth_ctx(None, &ClientHeaders::default(), None);

        assert!(ctx.ip().is_none());
        assert!(ctx.identity().is_none());
    }

    // ── the gate ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_state_with_no_throttle_lets_every_attempt_through() {
        let ctx = auth_ctx(None, &ClientHeaders::default(), Some("ada@example.com"));

        gate(&state(), &ctx, None)
            .await
            .expect("no throttle, no gate");
    }

    #[tokio::test]
    async fn a_spent_address_quota_is_a_rate_limit_and_not_a_credential_failure() {
        let config = ThrottleConfig {
            per_ip_burst: 1,
            ..ThrottleConfig::default()
        };
        let throttle = LoginThrottle::new(
            moso_kv::Kv::in_memory("gate-test").expect("an in-memory kv"),
            config,
        );
        let state = state().throttle(throttle);
        let address = ClientIp(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));
        let ctx = auth_ctx(Some(&address), &ClientHeaders::default(), None);

        gate(&state, &ctx, None).await.expect("the first attempt");

        match gate(&state, &ctx, None).await {
            Err(Error::RateLimited { retry_after }) => {
                assert!(retry_after > std::time::Duration::ZERO);
            }
            other => panic!("the second attempt was {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_challenge_with_no_verifier_configured_is_a_refusal() {
        let config = ThrottleConfig {
            per_ip_burst: 10_000,
            per_identity_free: 10,
            challenge_after: 1,
            ..ThrottleConfig::default()
        };
        let throttle = LoginThrottle::new(
            moso_kv::Kv::in_memory("gate-test").expect("an in-memory kv"),
            config,
        );
        let state = state().throttle(throttle);
        let ctx = auth_ctx(None, &ClientHeaders::default(), Some("ada@example.com"));

        record(&state, &ctx, false).await;

        assert!(
            matches!(
                gate(&state, &ctx, None).await,
                Err(Error::RateLimited { .. })
            ),
            "no verifier means a challenge cannot be cleared"
        );
    }

    // ── the handle ────────────────────────────────────────────────────────

    #[test]
    fn a_session_is_named_to_a_client_by_a_digest_and_never_by_its_id() {
        let id = SessionId::generate();
        let handle = handle_of(&id);

        assert_eq!(handle.len(), 64);
        assert!(handle.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(!handle.contains(id.as_str()));
        assert_eq!(
            handle,
            handle_of(&id),
            "stable, so a listing can be acted on"
        );
        assert_ne!(handle, handle_of(&SessionId::generate()));
    }

    // ── next ──────────────────────────────────────────────────────────────

    #[test]
    fn an_absent_next_stays_absent_and_a_bad_one_is_refused_before_it_is_stored() {
        assert_eq!(checked_next(None, &[]).expect("absent"), None);
        assert_eq!(
            checked_next(Some("/dashboard"), &[]).expect("a path"),
            Some("/dashboard".to_owned())
        );
        assert!(checked_next(Some("https://evil.example"), &[]).is_err());
    }
}
