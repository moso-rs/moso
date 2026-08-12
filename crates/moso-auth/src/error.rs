//! Authentication failures, and the rule that shapes all of them.
//!
//! **A failure must not say which part failed.** "No such account" and "wrong
//! password" are the same response with the same timing, because the difference
//! between them is a user-enumeration oracle — and enumeration is the first step
//! of every credential-stuffing campaign. The variants below exist so the
//! *server* can log precisely; [`Error::client_facing`] is what a client sees,
//! and it collapses them.
//!
//! # The collapse rule
//!
//! Exactly four variants are enumeration-sensitive, and
//! [`Error::client_facing`] folds all four into [`Error::InvalidCredentials`]:
//!
//! | Logged as | Seen as | Why it cannot be distinguished |
//! | --- | --- | --- |
//! | [`Error::InvalidCredentials`] | `InvalidCredentials` | the base case |
//! | [`Error::Expired`] | `InvalidCredentials` | "expired" proves the account exists |
//! | [`Error::Revoked`] | `InvalidCredentials` | so does "revoked" |
//! | [`Error::Ceremony`] | `InvalidCredentials` | a `state` mismatch is a probe |
//!
//! Everything else survives the collapse unchanged, because none of it answers
//! "does this account exist?": a rate limit, a password-policy rejection, a
//! second-factor challenge, an unreachable store and a configuration mistake are
//! all facts about the *request* or the *server*, not about an identity.
//!
//! `From<Error> for moso_core::Error` runs the collapse first and then maps onto
//! the HTTP problem, so no route can accidentally answer with the uncollapsed
//! form — the conversion is the only way an authentication failure reaches a
//! client.

use std::borrow::Cow;
use std::time::Duration;

use http::HeaderValue;
use moso_schema::{ValidationErrors, codes};

/// The result of every fallible operation in this crate.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// A boxed error from a backend or a store.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The `WWW-Authenticate` challenge every 401 this crate produces carries.
///
/// RFC 7235 requires a 401 to name at least one scheme. `Bearer` is the only
/// honest choice of the three schemes this crate documents: a session cookie is
/// not an HTTP authentication scheme, and `Basic` would make a browser open its
/// own credential dialog over an API response.
const WWW_AUTHENTICATE_CHALLENGE: &str = "Bearer";

/// The RFC 9457 extension member that carries a partial-authentication token.
///
/// `challenge` collides with none of the members the problem document reserves
/// (`type`, `title`, `status`, `detail`, `instance`, `errors`, `request_id`,
/// `trace_id`, `chain`), so it merges at the top level without shadowing one.
const CHALLENGE_MEMBER: &str = "challenge";

/// Something went wrong authenticating.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The credentials did not match.
    ///
    /// Covers "no such account", "wrong password" and "the account is
    /// inactive" — deliberately. Which one it was is in the log, never in the
    /// response.
    #[error("invalid credentials")]
    InvalidCredentials,

    /// Nothing was presented. A 401 with a `WWW-Authenticate` challenge.
    #[error("no credentials presented")]
    Unauthenticated,

    /// The credential is real but expired: a session past its absolute timeout,
    /// a token past `exp`, an API key past its expiry.
    #[error("{kind} has expired")]
    Expired {
        /// What expired, e.g. `"session"`.
        kind: &'static str,
    },

    /// The credential was valid and has been revoked: a logged-out session, a
    /// deleted key, a refresh token whose family was burned.
    #[error("{kind} has been revoked")]
    Revoked {
        /// What was revoked.
        kind: &'static str,
    },

    /// A second factor is needed before the session is fully authenticated.
    ///
    /// Not a failure so much as a step: the client shows the TOTP prompt and
    /// posts again. Carries the partial-authentication token that binds the two
    /// requests together, so the second one cannot be replayed for a different
    /// account.
    #[error("a second factor is required")]
    SecondFactorRequired {
        /// The opaque token the second request must carry.
        challenge: String,
    },

    /// Too many attempts. Carries how long to wait, for `Retry-After`.
    #[error("too many attempts; retry in {}s", .retry_after.as_secs())]
    RateLimited {
        /// How long until the next attempt is allowed.
        retry_after: Duration,
    },

    /// The password does not meet policy: too short, or found in a breach list.
    ///
    /// The one authentication failure that *should* be specific, because the
    /// user has to fix it and cannot guess how.
    #[error("password rejected: {code}")]
    PasswordPolicy {
        /// A stable code the client can branch on: `"len"`, `"breached"`,
        /// `"weak"`, `"reused"`.
        code: &'static str,
        /// A human explanation, safe to show.
        detail: Cow<'static, str>,
    },

    /// An OAuth or passkey ceremony did not check out.
    ///
    /// A mismatched `state`, a missing PKCE verifier, a `nonce` that does not
    /// match, an unverified email on an auto-link, a WebAuthn signature that
    /// does not verify. All the same to a client; all very different in a log.
    #[error("{ceremony} ceremony failed: {reason}")]
    Ceremony {
        /// Which ceremony, e.g. `"oauth"` or `"webauthn"`.
        ceremony: &'static str,
        /// What went wrong, for the log.
        reason: Cow<'static, str>,
    },

    /// The session store, the user store or an identity provider was
    /// unreachable.
    ///
    /// Never degraded into "not logged in": a session store outage must be a
    /// 503, because silently logging everybody out is worse than being down and
    /// is much harder to diagnose.
    #[error("{component} is unavailable: {detail}")]
    Unavailable {
        /// What could not be reached, e.g. `"session store"`.
        component: &'static str,
        /// What the transport reported.
        detail: String,
        /// The source, when there was one.
        #[source]
        source: Option<BoxError>,
    },

    /// Configuration is missing or contradictory.
    #[error("auth configuration is invalid: {0}")]
    Config(Cow<'static, str>),
}

impl Error {
    // ── constructors ──────────────────────────────────────────────────────

    /// A [`Error::Ceremony`] naming what failed.
    ///
    /// ```
    /// use moso_auth::Error;
    ///
    /// let error = Error::ceremony("oauth", "state did not match the session");
    ///
    /// // The log gets the specific reason …
    /// assert_eq!(error.to_string(), "oauth ceremony failed: state did not match the session");
    /// // … and the client gets the same answer a wrong password gets.
    /// assert!(matches!(error.client_facing(), Error::InvalidCredentials));
    /// ```
    #[must_use]
    pub fn ceremony(ceremony: &'static str, reason: impl Into<Cow<'static, str>>) -> Self {
        Error::Ceremony {
            ceremony,
            reason: reason.into(),
        }
    }

    /// A [`Error::Unavailable`] for a component that could not be reached.
    ///
    /// ```
    /// use moso_auth::Error;
    ///
    /// let error = Error::unavailable("session store", "connection refused", None);
    ///
    /// assert!(error.retryable());
    /// assert!(!error.counts_as_attempt());
    /// ```
    #[must_use]
    pub fn unavailable(
        component: &'static str,
        detail: impl Into<String>,
        source: Option<BoxError>,
    ) -> Self {
        Error::Unavailable {
            component,
            detail: detail.into(),
            source,
        }
    }

    /// A [`Error::PasswordPolicy`] with a stable code.
    ///
    /// `code` is what a client branches on and is one of `"len"`, `"banned"`,
    /// `"breached"`, `"weak"` or `"reused"`; `detail` is the sentence a person
    /// reads and is deliberately *not* part of the contract.
    ///
    /// ```
    /// use moso_auth::Error;
    ///
    /// let error = Error::password_policy("breached", "this password appears in a known breach");
    ///
    /// // A policy rejection is not enumeration-sensitive: the user is choosing
    /// // a password, not guessing somebody else's.
    /// assert!(matches!(error.client_facing(), Error::PasswordPolicy { code: "breached", .. }));
    /// ```
    #[must_use]
    pub fn password_policy(code: &'static str, detail: impl Into<Cow<'static, str>>) -> Self {
        Error::PasswordPolicy {
            code,
            detail: detail.into(),
        }
    }

    // ── the collapse, and the two policy questions ────────────────────────

    /// The error a *client* is allowed to see.
    ///
    /// Collapses [`Error::InvalidCredentials`], [`Error::Expired`],
    /// [`Error::Revoked`] and [`Error::Ceremony`] into
    /// [`Error::InvalidCredentials`], so no response distinguishes "no such
    /// account" from "wrong password" from "your session was revoked". The
    /// original is what gets logged.
    ///
    /// Every other variant is reproduced unchanged, with one deliberate loss:
    /// the [`source`](Error::Unavailable) of an [`Error::Unavailable`] is a
    /// `Box<dyn Error>` and cannot be cloned, so the client-facing copy carries
    /// `None`. That is the right direction to fail — a transport's own message
    /// is a server-side detail and must never reach a client — but it is why
    /// `From<Error> for moso_core::Error` consumes an `Unavailable` by value
    /// instead, so the operator's log keeps the chain.
    ///
    /// ```
    /// use moso_auth::Error;
    ///
    /// let logged = Error::Expired { kind: "session" };
    /// assert!(matches!(logged.client_facing(), Error::InvalidCredentials));
    ///
    /// // Not enumeration-sensitive, therefore untouched.
    /// let limited = Error::RateLimited { retry_after: std::time::Duration::from_secs(30) };
    /// assert!(matches!(limited.client_facing(), Error::RateLimited { .. }));
    /// ```
    #[must_use]
    pub fn client_facing(&self) -> Self {
        match self {
            Error::InvalidCredentials
            | Error::Expired { .. }
            | Error::Revoked { .. }
            | Error::Ceremony { .. } => Error::InvalidCredentials,
            Error::Unauthenticated => Error::Unauthenticated,
            Error::SecondFactorRequired { challenge } => Error::SecondFactorRequired {
                challenge: challenge.clone(),
            },
            Error::RateLimited { retry_after } => Error::RateLimited {
                retry_after: *retry_after,
            },
            Error::PasswordPolicy { code, detail } => Error::PasswordPolicy {
                code,
                detail: detail.clone(),
            },
            Error::Unavailable {
                component, detail, ..
            } => Error::Unavailable {
                component,
                detail: detail.clone(),
                source: None,
            },
            Error::Config(detail) => Error::Config(detail.clone()),
        }
    }

    /// Whether this failure should count against a rate limit.
    ///
    /// A failed credential does; an unreachable store does not, because
    /// counting an outage as an attack locks every user out of a recovering
    /// system.
    ///
    /// | Variant | Counts | Reason |
    /// | --- | --- | --- |
    /// | [`Error::InvalidCredentials`] | yes | the guess this exists to slow |
    /// | [`Error::Unauthenticated`] | yes | an empty credential is still a probe |
    /// | [`Error::Expired`] | yes | a replayed dead credential is the replay pattern |
    /// | [`Error::Revoked`] | yes | likewise, and see the note below |
    /// | [`Error::Ceremony`] | yes | a forged `state` or signature is an attack |
    /// | [`Error::SecondFactorRequired`] | no | the first factor *succeeded* |
    /// | [`Error::RateLimited`] | no | the attempt was refused, not made |
    /// | [`Error::PasswordPolicy`] | no | choosing a bad password is not guessing |
    /// | [`Error::Unavailable`] | no | an outage is not an attack |
    /// | [`Error::Config`] | no | our mistake, not theirs |
    ///
    /// The four collapsed variants must agree with each other, or the throttle
    /// becomes the oracle the collapse just closed: if an expired session were
    /// free and a wrong password were not, an attacker could tell them apart by
    /// watching when the backoff starts. [`Error::SecondFactorRequired`] is not
    /// a failure at all — it is the protocol working — and charging it would
    /// add backoff to every ordinary login of every user who has 2FA enabled,
    /// while leaking nothing, since the response already tells the client the
    /// password was right.
    ///
    /// ```
    /// use moso_auth::Error;
    ///
    /// assert!(Error::InvalidCredentials.counts_as_attempt());
    /// assert!(Error::Expired { kind: "session" }.counts_as_attempt());
    /// assert!(!Error::unavailable("user store", "connection reset", None).counts_as_attempt());
    /// ```
    #[must_use]
    pub fn counts_as_attempt(&self) -> bool {
        matches!(
            self,
            Error::InvalidCredentials
                | Error::Unauthenticated
                | Error::Expired { .. }
                | Error::Revoked { .. }
                | Error::Ceremony { .. }
        )
    }

    /// Whether retrying could succeed.
    ///
    /// [`Error::Unavailable`] is retryable because the store may come back, and
    /// [`Error::RateLimited`] is too — but only once its `retry_after` has
    /// elapsed, which is why the wait is carried in the variant rather than
    /// implied by this flag; answering `false` for it would also contradict the
    /// 429 this error becomes, whose own `moso_core::ErrorKind::retryable` is
    /// `true`.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use moso_auth::Error;
    ///
    /// assert!(!Error::InvalidCredentials.retryable());
    /// assert!(Error::unavailable("session store", "timed out", None).retryable());
    /// assert!(Error::RateLimited { retry_after: Duration::from_secs(30) }.retryable());
    /// ```
    #[must_use]
    pub fn retryable(&self) -> bool {
        matches!(self, Error::Unavailable { .. } | Error::RateLimited { .. })
    }
}

// ---------------------------------------------------------------------------
// Store failures
// ---------------------------------------------------------------------------

impl From<moso_kv::Error> for Error {
    /// A session-store failure. Always [`Error::Unavailable`], never a
    /// silent logout — which is why the session namespace declares
    /// `on_failure = fail`.
    ///
    /// Every `moso_kv` failure maps the same way on purpose: a circuit that has
    /// opened, a key that would not encode and a backend that refused the
    /// connection are all "the session store did not answer" from a caller's
    /// point of view, and the distinction is preserved in the `source` rather
    /// than in the variant.
    fn from(error: moso_kv::Error) -> Self {
        Error::Unavailable {
            component: "session store",
            detail: error.to_string(),
            source: Some(Box::new(error)),
        }
    }
}

impl From<moso_orm::Error> for Error {
    /// A user-store failure. A missing row is
    /// [`Error::InvalidCredentials`] and everything else is
    /// [`Error::Unavailable`].
    ///
    /// "No such row" is the miss path of a credential lookup, and it has to be
    /// indistinguishable from a wrong password — so it becomes the same
    /// [`Error::InvalidCredentials`] rather than a 404 that would confirm the
    /// account does not exist.
    fn from(error: moso_orm::Error) -> Self {
        if matches!(error, moso_orm::Error::NotFound { .. }) {
            return Error::InvalidCredentials;
        }
        Error::Unavailable {
            component: "user store",
            detail: error.to_string(),
            source: Some(Box::new(error)),
        }
    }
}

// ---------------------------------------------------------------------------
// The HTTP boundary
// ---------------------------------------------------------------------------

/// The 401 every unauthenticated answer from this crate produces.
///
/// One function, so the challenge header cannot drift between the four ways a
/// request fails to authenticate.
fn unauthenticated_401() -> moso_core::Error {
    moso_core::Error::unauthenticated().with_header(
        http::header::WWW_AUTHENTICATE,
        HeaderValue::from_static(WWW_AUTHENTICATE_CHALLENGE),
    )
}

/// The 503 an unreachable component produces.
///
/// `detail` is kept for the operator: `moso_core` suppresses a 5xx `detail`
/// at render time unless `http.expose_internal_errors` is set, so writing it
/// here costs a client nothing and saves an outage from being undiagnosable.
fn unavailable_503(component: &'static str, detail: &str) -> moso_core::Error {
    let message = if detail.is_empty() {
        format!("{component} is unavailable")
    } else {
        format!("{component} is unavailable: {detail}")
    };
    moso_core::Error::unavailable(message)
}

/// The field-error code a password-policy `code` is reported under.
///
/// `moso_schema`'s documented set is used where it has a match — only `"len"`
/// does — and everything this crate invents is namespaced with
/// [`codes::CUSTOM_PREFIX`], which is exactly what that prefix exists for: a
/// bare `"breached"` would collide with a code a future `moso-schema` minor
/// release might add.
fn field_code(code: &'static str) -> Cow<'static, str> {
    if code != codes::CUSTOM && codes::ALL.contains(&code) {
        return Cow::Borrowed(code);
    }
    Cow::Owned(format!("{}{code}", codes::CUSTOM_PREFIX))
}

impl From<Error> for moso_core::Error {
    /// An authentication failure becomes the HTTP problem it means.
    ///
    /// [`Error::InvalidCredentials`] and [`Error::Unauthenticated`] are 401
    /// with `WWW-Authenticate`; [`Error::RateLimited`] is 429 with
    /// `Retry-After`; [`Error::PasswordPolicy`] is 422 with a field pointer at
    /// `/password` and the code as the error code;
    /// [`Error::SecondFactorRequired`] is 401 with the challenge in an
    /// extension member; [`Error::Unavailable`] is 503 marked retryable.
    ///
    /// Everything goes through [`client_facing`](Error::client_facing) first.
    ///
    /// [`Error::Expired`], [`Error::Revoked`] and [`Error::Ceremony`] cannot
    /// survive that collapse, but they are still mapped — onto the identical
    /// 401 the collapse would have produced — so the conversion stays total and
    /// stays correct if the collapse rule is ever narrowed.
    fn from(error: Error) -> Self {
        // The one variant taken by value rather than through the collapse:
        // `client_facing` cannot clone a `BoxError`, and the source is the only
        // record of *why* a store was unreachable. Nothing is disclosed by
        // keeping it — an `Unavailable` is not enumeration-sensitive, and a 5xx
        // renders neither its detail nor its chain unless an operator has
        // deliberately set `http.expose_internal_errors`.
        if let Error::Unavailable {
            component,
            detail,
            source,
        } = error
        {
            let unavailable = unavailable_503(component, &detail);
            return match source {
                Some(source) => unavailable.with_source(source),
                None => unavailable,
            };
        }

        match error.client_facing() {
            Error::InvalidCredentials
            | Error::Unauthenticated
            | Error::Expired { .. }
            | Error::Revoked { .. }
            | Error::Ceremony { .. } => unauthenticated_401(),
            Error::SecondFactorRequired { challenge } => {
                unauthenticated_401().with_extension(CHALLENGE_MEMBER, challenge)
            }
            Error::RateLimited { retry_after } => moso_core::Error::too_many(retry_after),
            Error::PasswordPolicy { code, detail } => {
                let errors = ValidationErrors::one("/password", field_code(code), detail.clone());
                moso_core::Error::validation(errors).with_detail(detail)
            }
            Error::Unavailable {
                component, detail, ..
            } => unavailable_503(component, &detail),
            // A 500 whose detail names the misconfiguration. `moso_core`
            // suppresses a 5xx detail at render time, so the sentence reaches
            // the log and the dev error page and never the client.
            Error::Config(detail) => moso_core::Error::internal_msg(detail),
        }
    }
}

#[cfg(test)]
mod tests {
    use moso_core::Profile;
    use moso_core::error::problem::{Problem, ProblemOptions};
    use serde_json::Value;

    use super::*;

    /// The bytes a production deployment would actually put on the wire.
    ///
    /// Every "the client cannot see this" assertion goes through here rather
    /// than through `moso_core::Error::detail`, because `detail` is what the
    /// server holds and the rendered document is what the client receives.
    fn rendered(error: &moso_core::Error) -> String {
        let options = ProblemOptions::for_profile(Profile::Production);
        String::from_utf8(Problem::from_error(error, &options).to_bytes()).expect("utf-8 bytes")
    }

    /// The rendered document, parsed.
    fn document(error: &moso_core::Error) -> Value {
        serde_json::from_str(&rendered(error)).expect("a problem document")
    }

    // ── the collapse ─────────────────────────────────────────────────────

    #[test]
    fn an_expired_session_is_indistinguishable_from_a_wrong_password() {
        let expired = Error::Expired { kind: "session" }.client_facing();
        let wrong = Error::InvalidCredentials.client_facing();

        assert!(matches!(expired, Error::InvalidCredentials));
        assert!(matches!(wrong, Error::InvalidCredentials));
        assert_eq!(expired.to_string(), wrong.to_string());
    }

    #[test]
    fn a_revoked_credential_and_a_failed_ceremony_collapse_the_same_way() {
        let revoked = Error::Revoked { kind: "api key" };
        let ceremony = Error::ceremony("webauthn", "the signature did not verify");

        assert!(matches!(revoked.client_facing(), Error::InvalidCredentials));
        assert!(matches!(
            ceremony.client_facing(),
            Error::InvalidCredentials
        ));
    }

    #[test]
    fn the_reason_a_ceremony_failed_survives_for_the_log_and_not_the_client() {
        let ceremony = Error::ceremony("oauth", "state did not match the session");

        assert!(ceremony.to_string().contains("state did not match"));

        let rendered = rendered(&moso_core::Error::from(ceremony));
        assert!(!rendered.contains("state did not match"));
        assert!(!rendered.contains("oauth"));
    }

    #[test]
    fn a_variant_that_is_not_enumeration_sensitive_survives_the_collapse() {
        let policy = Error::password_policy("breached", "found in a known breach");
        let limited = Error::RateLimited {
            retry_after: Duration::from_secs(30),
        };
        let second = Error::SecondFactorRequired {
            challenge: "opaque".to_owned(),
        };
        let config = Error::Config(Cow::Borrowed("no signing key"));

        assert!(matches!(
            policy.client_facing(),
            Error::PasswordPolicy {
                code: "breached",
                ..
            }
        ));
        assert!(matches!(limited.client_facing(), Error::RateLimited { .. }));
        assert!(matches!(
            second.client_facing(),
            Error::SecondFactorRequired { .. }
        ));
        assert!(matches!(config.client_facing(), Error::Config(_)));
    }

    #[test]
    fn the_client_facing_copy_of_an_outage_drops_the_transport_source() {
        let outage = Error::unavailable(
            "session store",
            "connection refused",
            Some(Box::new(std::io::Error::other("ECONNREFUSED"))),
        );

        match outage.client_facing() {
            Error::Unavailable {
                component, source, ..
            } => {
                assert_eq!(component, "session store");
                assert!(source.is_none());
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    // ── the two policy questions ─────────────────────────────────────────

    #[test]
    fn an_unreachable_store_is_not_an_attempt_and_is_worth_retrying() {
        let outage = Error::unavailable("session store", "connection refused", None);

        assert!(!outage.counts_as_attempt());
        assert!(outage.retryable());
    }

    #[test]
    fn every_collapsed_variant_agrees_on_whether_it_counts() {
        let collapsed = [
            Error::InvalidCredentials,
            Error::Expired { kind: "session" },
            Error::Revoked {
                kind: "refresh token",
            },
            Error::ceremony("oauth", "nonce mismatch"),
        ];

        for error in &collapsed {
            assert!(error.counts_as_attempt(), "{error} should count");
            assert!(!error.retryable(), "{error} should not be retryable");
        }
    }

    #[test]
    fn a_refused_attempt_is_not_counted_twice() {
        let limited = Error::RateLimited {
            retry_after: Duration::from_secs(30),
        };

        assert!(!limited.counts_as_attempt());
        assert!(limited.retryable());
    }

    #[test]
    fn issuing_a_second_factor_challenge_is_not_a_failed_attempt() {
        let second = Error::SecondFactorRequired {
            challenge: "opaque".to_owned(),
        };

        assert!(!second.counts_as_attempt());
    }

    #[test]
    fn a_rejected_password_and_a_broken_configuration_are_never_attempts() {
        assert!(!Error::password_policy("len", "at least 12 characters").counts_as_attempt());
        assert!(!Error::Config(Cow::Borrowed("no signing key")).counts_as_attempt());
    }

    // ── store failures ───────────────────────────────────────────────────

    #[test]
    fn a_session_store_outage_is_never_a_silent_logout() {
        let error = Error::from(moso_kv::Error::codec("session", "unexpected end of input"));

        match error {
            Error::Unavailable {
                component, source, ..
            } => {
                assert_eq!(component, "session store");
                assert!(source.is_some());
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_row_in_the_user_store_is_a_credential_failure() {
        let error = Error::from(moso_orm::Error::not_found("Account"));

        assert!(matches!(error, Error::InvalidCredentials));
    }

    #[test]
    fn a_broken_user_store_is_never_a_credential_failure() {
        let error = Error::from(moso_orm::Error::Configuration {
            detail: "no pool was configured".to_owned(),
        });

        match error {
            Error::Unavailable {
                component, source, ..
            } => {
                assert_eq!(component, "user store");
                assert!(source.is_some());
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    // ── the HTTP boundary ────────────────────────────────────────────────

    #[test]
    fn a_wrong_password_and_a_missing_account_render_the_same_401() {
        let wrong = moso_core::Error::from(Error::InvalidCredentials);
        let missing = moso_core::Error::from(Error::from(moso_orm::Error::not_found("Account")));

        assert_eq!(wrong.status(), 401);
        assert_eq!(missing.status(), 401);
        assert_eq!(rendered(&wrong), rendered(&missing));
    }

    #[test]
    fn every_401_carries_a_challenge_header_that_does_not_open_a_browser_dialog() {
        let error = moso_core::Error::from(Error::Unauthenticated);
        let headers = error.headers().expect("a WWW-Authenticate header");

        assert_eq!(headers[http::header::WWW_AUTHENTICATE], "Bearer");
    }

    #[test]
    fn a_password_policy_failure_is_a_422_pointing_at_the_password_field() {
        let error = moso_core::Error::from(Error::password_policy("len", "at least 12 characters"));

        assert_eq!(error.status(), 422);

        let document = document(&error);
        let field = &document["errors"][0];
        assert_eq!(field["pointer"], "/password");
        assert_eq!(field["code"], "len");
        assert_eq!(field["message"], "at least 12 characters");
    }

    #[test]
    fn a_policy_code_outside_the_documented_set_is_namespaced_as_custom() {
        for code in ["breached", "weak", "reused", "banned"] {
            let error = moso_core::Error::from(Error::password_policy(code, "pick another"));
            let document = document(&error);

            assert_eq!(document["errors"][0]["code"], format!("custom:{code}"));
        }
    }

    #[test]
    fn a_rate_limited_attempt_is_a_429_carrying_retry_after() {
        let error = moso_core::Error::from(Error::RateLimited {
            retry_after: Duration::from_millis(1_500),
        });

        assert_eq!(error.status(), 429);
        assert!(error.retryable());

        let headers = error.headers().expect("a Retry-After header");
        // Rounded up: inviting the client back at 1 s would be inside the window.
        assert_eq!(headers[http::header::RETRY_AFTER], "2");
        assert_eq!(document(&error)["retry_after"], 2);
    }

    #[test]
    fn a_second_factor_requirement_is_a_401_carrying_the_challenge() {
        let error = moso_core::Error::from(Error::SecondFactorRequired {
            challenge: "pat_9f3c".to_owned(),
        });

        assert_eq!(error.status(), 401);

        let document = document(&error);
        assert_eq!(document["challenge"], "pat_9f3c");
        // The member must merge alongside the reserved ones, not over them.
        assert_eq!(document["status"], 401);
        assert_eq!(document["type"], "https://moso.rs/errors/unauthenticated");
    }

    #[test]
    fn an_unreachable_store_is_a_503_whose_transport_detail_stays_server_side() {
        let error = moso_core::Error::from(Error::unavailable(
            "session store",
            "connection refused by 10.0.0.7:6379",
            Some(Box::new(std::io::Error::other("ECONNREFUSED"))),
        ));

        assert_eq!(error.status(), 503);
        assert!(error.retryable());
        // The chain is what an operator greps for, and it survives the boundary.
        assert!(error.chain().contains("ECONNREFUSED"));

        let rendered = rendered(&error);
        assert!(!rendered.contains("10.0.0.7"));
        assert!(!rendered.contains("ECONNREFUSED"));
    }

    #[test]
    fn a_configuration_mistake_never_reaches_the_client_bytes() {
        let error = moso_core::Error::from(Error::Config(Cow::Borrowed(
            "auth.session.keys is empty; set MOSO_AUTH__SESSION__KEYS",
        )));

        assert_eq!(error.status(), 500);

        let rendered = rendered(&error);
        assert!(!rendered.contains("MOSO_AUTH__SESSION__KEYS"));
        assert!(!rendered.contains("auth.session.keys"));
        assert_eq!(document(&error)["title"], "Internal Server Error");
    }
}
