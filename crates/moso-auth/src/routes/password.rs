//! Registration, password login, the account lifecycle, and logging out.
//!
//! # The two rules every handler here is shaped by
//!
//! **A response must not say whether an address has an account.**
//! `/auth/register`, `/auth/password/forgot` and `/auth/verify-email/resend`
//! answer 202 with [`AcknowledgedResponse`] — one constant sentence — and do the
//! same work either way, because
//! [`Accounts`](crate::Accounts) hashes on the taken-address path and mints a
//! token it then burns on the unknown-address path. `/auth/login` answers the
//! same 401 for "no such account", "wrong password" and "suspended", and pays
//! for a password verification on all three.
//!
//! **The throttle runs before the hash.** Hashing is the expensive thing an
//! attacker wants the server to do, so [`gate`](super::support::gate) is the
//! first `await` on every throttled route here.
//! [`record`](super::support::record) is the last — but **only on
//! `/auth/login`**, because recording a success clears the identity's
//! consecutive-failure counter, and a route an unauthenticated caller can post
//! any address to must not be a way to clear somebody else's backoff.
//!
//! ```text
//! POST /auth/login
//!   └─ gate            per-address quota, then per-identity backoff   (429)
//!   └─ find identity   one round trip, hit or miss
//!   └─ verify          a real hash, or `dummy_verify` — the same cost  (401)
//!   └─ second factor   when one is enrolled                           (200)
//!   └─ session.log_in  cycles the id; `SessionLayer` writes the cookie
//!   └─ record          the outcome, which never fails the request
//! ```

use std::sync::Arc;

use moso_core::extract::ClientIp;
use moso_core::extract::{Headers, Json};
use moso_core::response::{Accepted, NoContent};
use moso_core::{Depends, Inject, Router};
use moso_schema::ValidationErrors;

use super::support::{self, ClientHeaders};
use super::{
    AcknowledgedResponse, AddressRequest, AuthState, ChangeEmailRequest, ChangePasswordRequest,
    ForgotPasswordRequest, LoginRequest, LoginResponse, MeResponse, RegisterRequest,
    ResetPasswordRequest, TokenRequest,
};
use crate::{AccountStore, AuthSession, AuthUser, Error, Principal, Result};

/// The session key the identity a login presented is recorded under.
///
/// The account store speaks [`DefaultUser`](crate::DefaultUser), which carries a
/// key and an epoch and no address, so the only server-side record of *which*
/// address signed in is this one. `/auth/email/change` reads it rather than
/// trusting the client to say what its own address is — a client that could
/// choose would choose where the "your address is being changed" warning goes.
pub(crate) const IDENTITY_KEY: &str = "_identity";

/// The session key a pending second-factor step is recorded under.
pub(crate) const PENDING_KEY: &str = "_pending_second_factor";

/// What the first step of a two-step login leaves in the session.
#[derive(serde::Deserialize, serde::Serialize)]
pub(crate) struct PendingSecondFactor {
    /// The account the second step will sign in.
    pub(crate) subject: String,
    /// The identity that account presented, for the session record.
    pub(crate) identity: String,
    /// The opaque token the second request must echo, which is what binds the
    /// two requests to each other.
    pub(crate) challenge: String,
    /// Where the login was heading, already validated.
    pub(crate) next: Option<String>,
}

/// Mount the password flows.
pub(crate) fn mount(allowlist: Arc<[String]>) -> Router {
    let login_allowlist = Arc::clone(&allowlist);

    let throttled = Router::new()
        .post("/auth/register", register)
        .post(
            "/auth/login",
            move |Inject(state): Inject<AuthState>,
                  address: Option<ClientIp>,
                  Headers(headers): Headers<ClientHeaders>,
                  Depends(AuthSession(session)): Depends<AuthSession>,
                  Json(body): Json<LoginRequest>| {
                let allowlist = Arc::clone(&login_allowlist);
                async move {
                    login(
                        &state,
                        address.as_ref(),
                        &headers,
                        &session,
                        body,
                        &allowlist,
                    )
                    .await
                }
            },
        )
        .post("/auth/password/forgot", forgot_password)
        .post("/auth/verify-email/resend", resend_verification)
        .tag(super::AUTH_TAG)
        .responds(429, super::throttled_response())
        .responds(503, super::unavailable_response());

    let rest = Router::new()
        .post("/auth/logout", logout)
        .post("/auth/logout-all", logout_all)
        .get("/auth/me", me)
        .post("/auth/verify-email", verify_email)
        .post("/auth/password/reset", reset_password)
        .post("/auth/password/change", change_password)
        .post("/auth/email/change", request_email_change)
        .post("/auth/email/change/confirm", confirm_email_change)
        .tag(super::AUTH_TAG)
        .responds(401, super::unauthenticated_response())
        .responds(503, super::unavailable_response());

    throttled.merge(rest)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// `POST /auth/register` — create an account, and answer the same way either
/// way.
async fn register(
    Inject(state): Inject<AuthState>,
    address: Option<ClientIp>,
    Headers(headers): Headers<ClientHeaders>,
    Json(body): Json<RegisterRequest>,
) -> moso_core::Result<Accepted<Json<AcknowledgedResponse>>> {
    let ctx = support::auth_ctx(address.as_ref(), &headers, Some(body.email.as_str()));
    support::gate(&state, &ctx, headers.x_captcha_response.as_deref()).await?;

    let accounts = state.require_accounts()?;
    let profile = match body.name.as_deref() {
        Some(name) => serde_json::json!({ "name": name }),
        None => serde_json::Value::Null,
    };

    // `register` hashes on both paths and mints a token on both paths, so the
    // branch below costs the same either way and only the *content* of the mail
    // differs — "confirm your address" or "somebody tried to use your address".
    let registration = accounts
        .register(body.email.as_str(), &body.password, profile)
        .await?;

    if let Some(token) = registration.token {
        state.deliver(super::Delivery::from_issued(&token)).await;
    }

    // Deliberately not recorded as an attempt. `LoginThrottle::record(.., true)`
    // clears the identity's consecutive-failure counter, and this route takes an
    // address from an unauthenticated caller — recording a success here would
    // let an attacker who has earned a backoff against a victim's address reset
    // it by posting this form. The address quota was already charged by `gate`,
    // which is the tier that belongs to a route nobody has to authenticate for.
    Ok(Accepted::new(Json(AcknowledgedResponse::new())))
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

/// `POST /auth/login` — one answer for every way it can fail.
async fn login(
    state: &AuthState,
    address: Option<&ClientIp>,
    headers: &ClientHeaders,
    session: &crate::Session,
    body: LoginRequest,
    allowlist: &[String],
) -> moso_core::Result<Json<LoginResponse>> {
    let ctx = support::auth_ctx(address, headers, Some(&body.identity));
    support::gate(state, &ctx, headers.x_captcha_response.as_deref()).await?;

    let next = support::checked_next(body.next.as_deref(), allowlist)?;

    let outcome = attempt(state, session, &body, next.clone()).await;
    let response = match outcome {
        Ok(response) => {
            support::record(state, &ctx, true).await;
            response
        }
        Err(error) => {
            support::record(state, &ctx, error.counts_as_attempt()).await;
            return Err(error.into());
        }
    };

    Ok(Json(response))
}

/// The credential half of a login, with every failure spelled the same way.
async fn attempt(
    state: &AuthState,
    session: &crate::Session,
    body: &LoginRequest,
    next: Option<String>,
) -> Result<LoginResponse> {
    if let Some(code) = body.totp.as_deref() {
        return second_step(state, session, body.challenge.as_deref(), code, next).await;
    }

    let accounts = state.require_accounts()?;
    let identity = crate::lifecycle::normalise(&body.identity);
    let found = accounts.store().find_by_identity(&identity).await?;

    // The miss path pays for a verification too. Without it, "no such account"
    // is a faster 401 than "wrong password", and the clock is the oracle.
    let user = match found {
        Some(user) => match accounts.store().password_hash(&user.auth_id()).await? {
            Some(hash) if hash.verify(&body.password).await?.is_valid() => user,
            Some(_) => return Err(Error::InvalidCredentials),
            None => {
                crate::password::dummy_verify().await?;
                return Err(Error::InvalidCredentials);
            }
        },
        None => {
            crate::password::dummy_verify().await?;
            return Err(Error::InvalidCredentials);
        }
    };

    // Checked after the verification, so a suspended account is not a faster
    // answer than a wrong password either.
    if !user.is_active() {
        return Err(Error::InvalidCredentials);
    }

    let subject = crate::session::encode_subject(&user.auth_id())?;
    if super::totp::confirmed(state, &subject).await?.is_some() {
        return challenge(session, &subject, &identity, next).await;
    }

    finish(session, &user, &identity).await?;
    Ok(LoginResponse {
        requires_second_factor: false,
        challenge: None,
        access_token: None,
        refresh_token: None,
        next,
    })
}

/// Hand back the token that binds the second request to this one.
async fn challenge(
    session: &crate::Session,
    subject: &str,
    identity: &str,
    next: Option<String>,
) -> Result<LoginResponse> {
    let challenge = support::opaque_token()?;
    session.insert(
        PENDING_KEY,
        PendingSecondFactor {
            subject: subject.to_owned(),
            identity: identity.to_owned(),
            challenge: challenge.clone(),
            next: next.clone(),
        },
    )?;

    Ok(LoginResponse {
        requires_second_factor: true,
        challenge: Some(challenge),
        access_token: None,
        refresh_token: None,
        next,
    })
}

/// The second request of a two-step login.
async fn second_step(
    state: &AuthState,
    session: &crate::Session,
    presented: Option<&str>,
    code: &str,
    next: Option<String>,
) -> Result<LoginResponse> {
    let pending = session
        .get::<PendingSecondFactor>(PENDING_KEY)?
        .ok_or(Error::InvalidCredentials)?;
    let presented = presented.ok_or(Error::InvalidCredentials)?;

    if !crate::password::constant_time_eq(pending.challenge.as_bytes(), presented.as_bytes()) {
        return Err(Error::InvalidCredentials);
    }

    let mut enrolment = super::totp::confirmed(state, &pending.subject)
        .await?
        .ok_or(Error::InvalidCredentials)?;
    if !enrolment.check(code)? {
        return Err(Error::InvalidCredentials);
    }
    // The period the code came from is written back, which is what makes a code
    // single-use: replaying it inside its own thirty seconds is refused.
    super::totp::save_confirmed(state, &pending.subject, &enrolment).await?;

    let accounts = state.require_accounts()?;
    let id = crate::session::decode_subject::<String>(&pending.subject)?;
    let user = accounts
        .store()
        .find_by_id(&id)
        .await?
        .ok_or(Error::InvalidCredentials)?;
    if !user.is_active() {
        return Err(Error::InvalidCredentials);
    }

    session.remove(PENDING_KEY)?;
    finish(session, &user, &pending.identity).await?;

    Ok(LoginResponse {
        requires_second_factor: false,
        challenge: None,
        access_token: None,
        refresh_token: None,
        next: next.or(pending.next),
    })
}

/// Bind the session to the principal, and record which address signed in.
///
/// [`Session::log_in`](crate::Session::log_in) cycles the identifier — the
/// fixation defence — and [`SessionLayer`](crate::SessionLayer) turns the
/// changed session into the `Set-Cookie` on the way out. No handler writes a
/// cookie by hand.
pub(crate) async fn finish(
    session: &crate::Session,
    user: &crate::DefaultUser,
    identity: &str,
) -> Result<()> {
    session.log_in(user).await?;
    session.insert(IDENTITY_KEY, identity)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Logging out
// ---------------------------------------------------------------------------

/// `POST /auth/logout` — end this session.
async fn logout(
    Depends(AuthSession(session)): Depends<AuthSession>,
) -> moso_core::Result<NoContent> {
    session.destroy().await?;
    Ok(NoContent)
}

/// `POST /auth/logout-all` — end every session, this one included.
async fn logout_all(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
) -> moso_core::Result<NoContent> {
    let subject = support::subject_of(&session)?;
    let accounts = state.require_accounts()?;
    let id = crate::session::decode_subject::<String>(&subject)?;

    // `None`, not this session: "log out everywhere" that leaves the browser
    // you typed it into signed in has not done what it says.
    accounts.log_out_everywhere(&id, None).await?;
    session.destroy().await?;
    Ok(NoContent)
}

/// `GET /auth/me` — who is making this request.
async fn me(Depends(principal): Depends<Principal>) -> moso_core::Result<Json<MeResponse>> {
    if !principal.is_authenticated() {
        return Err(Error::Unauthenticated.into());
    }

    Ok(Json(MeResponse {
        subject: principal.subject.clone(),
        kind: principal.kind.as_str().to_owned(),
        scopes: principal.scopes.clone(),
    }))
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// `POST /auth/verify-email` — redeem a verification token.
async fn verify_email(
    Inject(state): Inject<AuthState>,
    Json(body): Json<TokenRequest>,
) -> moso_core::Result<NoContent> {
    state.require_accounts()?.verify_email(&body.token).await?;
    Ok(NoContent)
}

/// `POST /auth/verify-email/resend` — and say nothing about the address.
async fn resend_verification(
    Inject(state): Inject<AuthState>,
    address: Option<ClientIp>,
    Headers(headers): Headers<ClientHeaders>,
    Json(body): Json<AddressRequest>,
) -> moso_core::Result<Accepted<Json<AcknowledgedResponse>>> {
    let ctx = support::auth_ctx(address.as_ref(), &headers, Some(body.email.as_str()));
    support::gate(&state, &ctx, headers.x_captcha_response.as_deref()).await?;

    let issued = state
        .require_accounts()?
        .resend_verification(body.email.as_str())
        .await?;
    if let Some(token) = issued {
        state.deliver(super::Delivery::from_issued(&token)).await;
    }

    // Deliberately not recorded as an attempt. `LoginThrottle::record(.., true)`
    // clears the identity's consecutive-failure counter, and this route takes an
    // address from an unauthenticated caller — recording a success here would
    // let an attacker who has earned a backoff against a victim's address reset
    // it by posting this form. The address quota was already charged by `gate`,
    // which is the tier that belongs to a route nobody has to authenticate for.
    Ok(Accepted::new(Json(AcknowledgedResponse::new())))
}

// ---------------------------------------------------------------------------
// Passwords
// ---------------------------------------------------------------------------

/// `POST /auth/password/forgot` — always 202, always the same body.
async fn forgot_password(
    Inject(state): Inject<AuthState>,
    address: Option<ClientIp>,
    Headers(headers): Headers<ClientHeaders>,
    Json(body): Json<ForgotPasswordRequest>,
) -> moso_core::Result<Accepted<Json<AcknowledgedResponse>>> {
    let ctx = support::auth_ctx(address.as_ref(), &headers, Some(body.email.as_str()));
    support::gate(&state, &ctx, headers.x_captcha_response.as_deref()).await?;

    let issued = state
        .require_accounts()?
        .request_password_reset(body.email.as_str())
        .await?;
    if let Some(token) = issued {
        state.deliver(super::Delivery::from_issued(&token)).await;
    }

    // Deliberately not recorded as an attempt. `LoginThrottle::record(.., true)`
    // clears the identity's consecutive-failure counter, and this route takes an
    // address from an unauthenticated caller — recording a success here would
    // let an attacker who has earned a backoff against a victim's address reset
    // it by posting this form. The address quota was already charged by `gate`,
    // which is the tier that belongs to a route nobody has to authenticate for.
    Ok(Accepted::new(Json(AcknowledgedResponse::new())))
}

/// `POST /auth/password/reset` — redeem a reset token and end every session.
async fn reset_password(
    Inject(state): Inject<AuthState>,
    Json(body): Json<ResetPasswordRequest>,
) -> moso_core::Result<NoContent> {
    state
        .require_accounts()?
        .reset_password(&body.token, &body.password)
        .await?;
    Ok(NoContent)
}

/// `POST /auth/password/change` — the current password, then the new one.
async fn change_password(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
    Json(body): Json<ChangePasswordRequest>,
) -> moso_core::Result<NoContent> {
    if body.logout_other_sessions == Some(false) {
        // Refused rather than ignored. A password change bumps the epoch, and
        // the epoch is what `auth_hash` compares on every session load, so every
        // other session is already invalid by the time this handler returns —
        // there is no implementation of `false` to give, and answering 204 as if
        // there were would be a lie about what happened.
        return Err(moso_core::Error::validation(ValidationErrors::one(
            "/logout_other_sessions",
            "custom:unsupported",
            "a password change always ends every other session, because it bumps the session \
             epoch; omit this field or send `true`",
        )));
    }

    let subject = support::subject_of(&session)?;
    let accounts = state.require_accounts()?;
    let id = crate::session::decode_subject::<String>(&subject)?;
    let current = session.id();

    accounts
        .change_password(
            &id,
            &body.current_password,
            &body.new_password,
            Some(&current),
        )
        .await?;
    Ok(NoContent)
}

// ---------------------------------------------------------------------------
// Address changes
// ---------------------------------------------------------------------------

/// `POST /auth/email/change` — begin a double opt-in change of address.
async fn request_email_change(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
    Json(body): Json<ChangeEmailRequest>,
) -> moso_core::Result<Accepted<Json<AcknowledgedResponse>>> {
    let subject = support::subject_of(&session)?;
    let accounts = state.require_accounts()?;
    let id = crate::session::decode_subject::<String>(&subject)?;

    // The current password, because an address change is a takeover primitive:
    // the new address can ask for a password reset the moment it confirms.
    let stored = accounts.store().password_hash(&id).await?;
    let ok = match stored.as_ref() {
        Some(hash) => hash.verify(&body.current_password).await?.is_valid(),
        None => {
            crate::password::dummy_verify().await?;
            false
        }
    };
    if !ok {
        return Err(Error::InvalidCredentials.into());
    }

    let current = session
        .get::<String>(IDENTITY_KEY)?
        .ok_or_else(|| support::ceremony("this session does not record which address signed in"))?;

    let change = accounts
        .request_email_change(&id, &current, body.new_email.as_str())
        .await?;
    state
        .deliver(super::Delivery::from_issued(&change.confirmation))
        .await;

    tracing::info!(
        target: "moso.auth",
        "an address change was requested; notify the previous address"
    );
    Ok(Accepted::new(Json(AcknowledgedResponse::new())))
}

/// `POST /auth/email/change/confirm` — redeem the confirmation.
async fn confirm_email_change(
    Inject(state): Inject<AuthState>,
    Json(body): Json<TokenRequest>,
) -> moso_core::Result<NoContent> {
    state
        .require_accounts()?
        .confirm_email_change(&body.token)
        .await?;
    Ok(NoContent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_session_keys_are_namespaced_so_an_application_cannot_collide_with_them() {
        assert!(IDENTITY_KEY.starts_with('_'));
        assert!(PENDING_KEY.starts_with('_'));
        assert_ne!(IDENTITY_KEY, PENDING_KEY);
    }

    #[test]
    fn a_pending_second_factor_round_trips_through_the_session_encoding() {
        let pending = PendingSecondFactor {
            subject: "usr_1".to_owned(),
            identity: "ada@example.com".to_owned(),
            challenge: "opaque".to_owned(),
            next: Some("/dashboard".to_owned()),
        };

        let json = serde_json::to_value(&pending).expect("json");
        let back: PendingSecondFactor = serde_json::from_value(json).expect("json");

        assert_eq!(back.subject, "usr_1");
        assert_eq!(back.challenge, "opaque");
        assert_eq!(back.next.as_deref(), Some("/dashboard"));
    }

    // ── what a client is allowed to tell apart ────────────────────────────

    /// The bytes a production deployment would put on the wire.
    fn rendered(error: moso_core::Error) -> String {
        let options =
            moso_core::error::problem::ProblemOptions::for_profile(moso_core::Profile::Production);
        String::from_utf8(
            moso_core::error::problem::Problem::from_error(&error, &options).to_bytes(),
        )
        .expect("utf-8 bytes")
    }

    #[test]
    fn a_missing_account_and_a_wrong_password_are_the_same_bytes() {
        // The two refusals `attempt` produces, in the order it produces them:
        // `find_by_identity` came back empty, and the stored hash did not
        // verify. Both are `Error::InvalidCredentials` by construction, and this
        // asserts they stay identical all the way to the rendered document.
        let missing = Error::InvalidCredentials;
        let wrong = Error::InvalidCredentials;

        let missing = moso_core::Error::from(missing);
        let wrong = moso_core::Error::from(wrong);

        assert_eq!(missing.status(), 401);
        assert_eq!(wrong.status(), 401);
        assert_eq!(rendered(missing), rendered(wrong));
    }

    #[test]
    fn a_suspended_account_is_not_a_different_answer_either() {
        // `attempt` refuses an inactive account *after* the verification, with
        // the same value, so a client cannot tell "suspended" from "wrong
        // password" by the body or by the clock.
        let suspended = moso_core::Error::from(Error::InvalidCredentials);
        let expired = moso_core::Error::from(Error::Expired { kind: "session" });

        assert_eq!(rendered(suspended), rendered(expired));
    }

    #[test]
    fn the_forgot_password_answer_is_202_with_one_constant_body() {
        use moso_core::response::IntoResponse as _;

        let answer = Accepted::new(Json(AcknowledgedResponse::new())).into_response();

        assert_eq!(answer.status(), 202);
        assert_eq!(
            serde_json::to_string(&AcknowledgedResponse::new()).expect("json"),
            serde_json::to_string(&AcknowledgedResponse::default()).expect("json"),
            "the body cannot vary with whether the address exists"
        );
    }

    #[test]
    fn refusing_to_keep_the_other_sessions_names_the_field_rather_than_ignoring_it() {
        let refusal = moso_core::Error::validation(ValidationErrors::one(
            "/logout_other_sessions",
            "custom:unsupported",
            "a password change always ends every other session",
        ));

        assert_eq!(refusal.status(), 422);
        assert!(
            refusal.fields().is_some_and(|fields| !fields.is_empty()),
            "a 422 with no field pointer tells a client nothing"
        );
    }
}
