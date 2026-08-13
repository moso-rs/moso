//! Enrolling, confirming and removing a time-based second factor.
//!
//! # Where an enrolment lives
//!
//! In [`moso_kv`], under two namespaces, because a TOTP secret belongs to
//! neither the session (it outlives one) nor the account store (whose trait,
//! [`AccountStore`](crate::AccountStore), deliberately has no column for it).
//! The key is the SHA-256 of the account's subject, so the store never holds a
//! list of which accounts have a second factor in readable form.
//!
//! | Namespace | Holds | Lives |
//! | --- | --- | --- |
//! | `PendingTotp` | a started, unconfirmed enrolment | [`PENDING_TTL`] |
//! | `ConfirmedTotp` | the secret and the last period a code came from | until it is disabled |
//!
//! Both declare `on_failure = fail`. A store that blinks must not read as "this
//! account has no second factor", which is the one degradation that would turn
//! an outage into an authentication bypass.
//!
//! # Why the last period is stored
//!
//! [`TotpEnrollment`] refuses a code from a period it has already accepted, so
//! a code observed on the wire cannot be replayed inside its own thirty seconds
//! — but only if the period survives the request that used it. That is what
//! [`save_confirmed`] writes back, and it is why the login path calls it.

use moso_core::extract::ClientIp;
use moso_core::extract::{Headers, Json};
use moso_core::response::NoContent;
use moso_core::{Depends, Inject, Router};
use serde::{Deserialize, Serialize};

use super::support::{self, ClientHeaders};
use super::{AuthState, TotpCodeRequest, TotpSetupResponse};
use crate::{AuthSession, Error, Result, TotpEnrollment, TotpSecret};

/// How long a started but unconfirmed enrolment survives.
///
/// Fifteen minutes: long enough to scan a QR code and read the next code out of
/// an authenticator, short enough that an abandoned enrolment is not a secret
/// sitting in a store forever.
pub(crate) const PENDING_TTL: std::time::Duration = moso_kv::minutes(15);

/// The component name a store failure in this module is reported under.
const COMPONENT: &str = "TOTP enrolment store";

moso_kv::namespace! {
    /// An enrolment that has been started and not yet proved.
    pub(crate) PendingTotp: str => StoredTotp, ttl = PENDING_TTL, on_failure = fail;

    /// A confirmed enrolment: the secret, and the last period it accepted.
    pub(crate) ConfirmedTotp: str => StoredTotp, on_failure = fail;
}

/// The three things a [`TotpEnrollment`] has to be rebuilt from.
///
/// `state` is not among them: a value under `PendingTotp` is pending and a value
/// under `ConfirmedTotp` is confirmed, so the namespace *is* the state and
/// storing it twice would let the two disagree.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct StoredTotp {
    /// The base32 secret.
    pub(crate) secret: String,
    /// The period the last accepted code came from, which is what makes a code
    /// single-use.
    pub(crate) last_period: Option<u64>,
}

/// Mount the TOTP routes.
pub(crate) fn mount() -> Router {
    Router::new()
        .post("/auth/totp/setup", setup)
        .post("/auth/totp/confirm", confirm)
        .post("/auth/totp/disable", disable)
        .tag(super::AUTH_TAG)
        .responds(401, super::unauthenticated_response())
        .responds(429, super::throttled_response())
        .responds(503, super::unavailable_response())
}

/// `POST /auth/totp/setup` — a fresh secret and the URI to render as a QR code.
async fn setup(
    Inject(state): Inject<AuthState>,
    address: Option<ClientIp>,
    Headers(headers): Headers<ClientHeaders>,
    Depends(AuthSession(session)): Depends<AuthSession>,
) -> moso_core::Result<Json<TotpSetupResponse>> {
    let subject = support::subject_of(&session)?;
    let ctx = support::auth_ctx(address.as_ref(), &headers, Some(&subject));
    support::gate(&state, &ctx, headers.x_captcha_response.as_deref()).await?;

    let account = session
        .get::<String>(super::password::IDENTITY_KEY)?
        .unwrap_or_else(|| subject.clone());
    let enrolment = TotpEnrollment::start(state.issuer_name(), &account)?;

    write(&state, &subject, &enrolment, false).await?;

    Ok(Json(TotpSetupResponse {
        secret: enrolment.secret().as_secret().expose().to_owned(),
        provisioning_uri: enrolment.provisioning_uri().to_owned(),
    }))
}

/// `POST /auth/totp/confirm` — prove the authenticator holds the secret.
async fn confirm(
    Inject(state): Inject<AuthState>,
    address: Option<ClientIp>,
    Headers(headers): Headers<ClientHeaders>,
    Depends(AuthSession(session)): Depends<AuthSession>,
    Json(body): Json<TotpCodeRequest>,
) -> moso_core::Result<NoContent> {
    let subject = support::subject_of(&session)?;
    let ctx = support::auth_ctx(address.as_ref(), &headers, Some(&subject));
    support::gate(&state, &ctx, headers.x_captcha_response.as_deref()).await?;

    let mut enrolment = read::<PendingTotp>(&state, &subject, TotpEnrollment::resume)
        .await?
        .ok_or(Error::InvalidCredentials)?;

    // A pending enrolment is resumed as confirmed, so `confirm` cannot be used
    // on it — `check` is the same code comparison and the same replay refusal.
    let accepted = enrolment.check(&body.code)?;
    support::record(&state, &ctx, accepted).await;
    if !accepted {
        return Err(Error::InvalidCredentials.into());
    }

    write(&state, &subject, &enrolment, true).await?;
    delete::<PendingTotp>(&state, &subject).await?;
    Ok(NoContent)
}

/// `POST /auth/totp/disable` — remove the second factor, given a live code.
///
/// A code is required. Without one, an unattended browser removes the second
/// factor, which is exactly what the second factor exists to prevent.
async fn disable(
    Inject(state): Inject<AuthState>,
    address: Option<ClientIp>,
    Headers(headers): Headers<ClientHeaders>,
    Depends(AuthSession(session)): Depends<AuthSession>,
    Json(body): Json<TotpCodeRequest>,
) -> moso_core::Result<NoContent> {
    let subject = support::subject_of(&session)?;
    let ctx = support::auth_ctx(address.as_ref(), &headers, Some(&subject));
    support::gate(&state, &ctx, headers.x_captcha_response.as_deref()).await?;

    let mut enrolment = confirmed(&state, &subject)
        .await?
        .ok_or(Error::InvalidCredentials)?;

    let accepted = enrolment.check(&body.code)?;
    support::record(&state, &ctx, accepted).await;
    if !accepted {
        // Nothing is written back: `TotpEnrollment` consumes a period only when
        // a code matches, so a refusal leaves the stored state untouched and a
        // wrong guess costs one read rather than a read and a write.
        return Err(Error::InvalidCredentials.into());
    }

    delete::<ConfirmedTotp>(&state, &subject).await?;
    Ok(NoContent)
}

// ---------------------------------------------------------------------------
// What the login path uses
// ---------------------------------------------------------------------------

/// The confirmed enrolment for `subject`, if there is one.
///
/// `Ok(None)` when no key-value store is configured at all: a deployment
/// without one cannot have enrolled anybody either, so there is nothing to
/// bypass. A store that *is* configured and cannot be reached is an `Err`.
///
/// # Errors
///
/// [`Error::Unavailable`] when the store is configured and unreachable.
pub(crate) async fn confirmed(state: &AuthState, subject: &str) -> Result<Option<TotpEnrollment>> {
    if state.kv_store().is_none() {
        return Ok(None);
    }
    read::<ConfirmedTotp>(state, subject, TotpEnrollment::resume).await
}

/// Write back the period a code was accepted from.
///
/// # Errors
///
/// [`Error::Unavailable`].
pub(crate) async fn save_confirmed(
    state: &AuthState,
    subject: &str,
    enrolment: &TotpEnrollment,
) -> Result<()> {
    write(state, subject, enrolment, true).await
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// Read one namespace and rebuild the enrolment it holds.
async fn read<N>(
    state: &AuthState,
    subject: &str,
    rebuild: fn(TotpSecret, Option<u64>) -> TotpEnrollment,
) -> Result<Option<TotpEnrollment>>
where
    N: moso_kv::Namespace<Key = str, Value = StoredTotp>,
{
    let kv = state.require_kv()?;
    let stored = kv
        .get::<N>(&support::keyed(subject))
        .await
        .map_err(|error| support::kv_failed(COMPONENT, "get", error))?;

    match stored {
        None => Ok(None),
        Some(stored) => Ok(Some(rebuild(
            TotpSecret::from_base32(&stored.secret)?,
            stored.last_period,
        ))),
    }
}

/// Store an enrolment under the namespace its state names.
async fn write(
    state: &AuthState,
    subject: &str,
    enrolment: &TotpEnrollment,
    confirmed: bool,
) -> Result<()> {
    let kv = state.require_kv()?;
    let value = StoredTotp {
        secret: enrolment.secret().as_secret().expose().to_owned(),
        last_period: enrolment.last_period(),
    };
    let key = support::keyed(subject);

    if confirmed {
        return kv
            .set::<ConfirmedTotp>(&key, &value)
            .await
            .map_err(|error| support::kv_failed(COMPONENT, "set confirmed", error));
    }
    kv.set::<PendingTotp>(&key, &value)
        .await
        .map_err(|error| support::kv_failed(COMPONENT, "set pending", error))
}

/// Forget an enrolment.
async fn delete<N>(state: &AuthState, subject: &str) -> Result<()>
where
    N: moso_kv::Namespace<Key = str, Value = StoredTotp>,
{
    let kv = state.require_kv()?;
    kv.delete::<N>(&support::keyed(subject))
        .await
        .map(|_| ())
        .map_err(|error| support::kv_failed(COMPONENT, "delete", error))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state with an in-memory key-value store, which is a real store.
    fn state() -> AuthState {
        AuthState::new(crate::MemorySessionStore::shared())
            .kv(moso_kv::Kv::in_memory("totp-test").expect("an in-memory kv"))
            .issuer("Example")
    }

    #[tokio::test]
    async fn an_enrolment_survives_a_round_trip_through_the_store() {
        let state = state();
        let started = TotpEnrollment::start("Example", "ada@example.com").expect("started");

        write(&state, "usr_1", &started, true)
            .await
            .expect("stored");
        let back = confirmed(&state, "usr_1")
            .await
            .expect("read")
            .expect("found");

        assert_eq!(
            back.secret().as_secret().expose(),
            started.secret().as_secret().expose()
        );
        assert!(back.is_confirmed());
    }

    #[tokio::test]
    async fn the_period_a_code_came_from_survives_so_the_code_cannot_be_replayed() {
        let state = state();
        let enrolment = TotpEnrollment::start("Example", "ada@example.com").expect("started");
        let code = enrolment
            .totp()
            .current(enrolment.secret())
            .expect("a code");

        write(&state, "usr_1", &enrolment, true)
            .await
            .expect("stored");
        let mut first = confirmed(&state, "usr_1")
            .await
            .expect("read")
            .expect("found");

        assert!(first.check(&code).expect("checked"), "the first use works");
        save_confirmed(&state, "usr_1", &first)
            .await
            .expect("saved");

        let mut second = confirmed(&state, "usr_1")
            .await
            .expect("read")
            .expect("found");
        assert!(
            !second.check(&code).expect("checked"),
            "the same code inside the same period is refused"
        );
    }

    #[tokio::test]
    async fn an_account_with_no_enrolment_has_no_second_factor() {
        assert!(
            confirmed(&state(), "usr_nobody")
                .await
                .expect("read")
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_deployment_with_no_key_value_store_has_no_enrolments_to_bypass() {
        let state = AuthState::new(crate::MemorySessionStore::shared());

        assert!(confirmed(&state, "usr_1").await.expect("read").is_none());
    }

    #[tokio::test]
    async fn a_pending_enrolment_is_not_a_confirmed_one() {
        let state = state();
        let started = TotpEnrollment::start("Example", "ada@example.com").expect("started");

        write(&state, "usr_1", &started, false)
            .await
            .expect("stored");

        assert!(confirmed(&state, "usr_1").await.expect("read").is_none());
        assert!(
            read::<PendingTotp>(&state, "usr_1", TotpEnrollment::resume)
                .await
                .expect("read")
                .is_some()
        );
    }

    #[test]
    fn a_subject_never_appears_in_a_key_in_the_clear() {
        let keyed = support::keyed("usr_1");

        assert_eq!(keyed.len(), 64);
        assert!(!keyed.contains("usr_1"));
    }
}
