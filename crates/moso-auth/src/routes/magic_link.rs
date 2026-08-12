//! Signing in by a one-time link.
//!
//! # A magic link is a login, so it is not a lifecycle token
//!
//! [`LifecycleTokens`](crate::LifecycleTokens) is scoped by
//! [`TokenPurpose`](crate::TokenPurpose), and every value of that enum names
//! something a token *redeems*: verify this address, reset this password,
//! confirm this change. A magic link redeems none of them — it signs somebody
//! in — and issuing one under a borrowed purpose would make a verification
//! link redeemable as a login. So this flow has its own store and its own
//! namespace, and the two vocabularies never meet.
//!
//! # What is stored is the digest, never the token
//!
//! [`MagicLink::hash_of`](crate::MagicLink::hash_of) is the key. The link that
//! goes in the email is the only copy of the token that ever exists; a stolen
//! store is a list of SHA-256 digests of values with 256 bits of entropy, which
//! is not a list of anything.
//!
//! | Route | Answer |
//! | --- | --- |
//! | `POST /auth/magic-link` | 202 with the same body whether or not the address is known |
//! | `GET /auth/magic-link/{token}` | 303 to `next`, having signed the browser in |

use std::sync::Arc;

use chrono::{DateTime, Utc};
use moso_core::extract::ClientIp;
use moso_core::extract::{Headers, Json, Path};
use moso_core::response::{Accepted, Redirect};
use moso_core::{Depends, Inject, Router};
use serde::{Deserialize, Serialize};

use super::support::{self, ClientHeaders};
use super::{AcknowledgedResponse, AuthState, Delivery, DeliveryPurpose, MagicLinkRequest};
use crate::config::MAGIC_LINK_TTL;
use crate::{AccountStore, AuthSession, AuthUser, Error, MagicLink};

/// The component name a store failure in this module is reported under.
const COMPONENT: &str = "magic-link store";

/// Where a link with no `next` lands.
const DEFAULT_NEXT: &str = "/";

moso_kv::namespace! {
    /// One outstanding magic link, keyed by the digest of its token.
    pub(crate) MagicLinkToken: str => MagicLinkClaim, ttl = MAGIC_LINK_TTL, on_failure = fail;
}

/// What a redeemed link says about who it signs in.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct MagicLinkClaim {
    /// The address the link was sent to, normalised.
    pub(crate) identity: String,
    /// When the link stops working. Checked as well as the TTL, because a TTL
    /// is the store's promise and this is ours.
    pub(crate) expires_at: DateTime<Utc>,
    /// Where to land, already validated against the allowlist.
    pub(crate) next: Option<String>,
}

/// Mount the magic-link routes.
pub(crate) fn mount(allowlist: Arc<[String]>) -> Router {
    let request_allowlist = Arc::clone(&allowlist);

    Router::new()
        .post(
            "/auth/magic-link",
            move |Inject(state): Inject<AuthState>,
                  address: Option<ClientIp>,
                  Headers(headers): Headers<ClientHeaders>,
                  Json(body): Json<MagicLinkRequest>| {
                let allowlist = Arc::clone(&request_allowlist);
                async move { request(&state, address.as_ref(), &headers, body, &allowlist).await }
            },
        )
        .get("/auth/magic-link/{token}", consume)
        .tag(super::AUTH_TAG)
        .responds(401, super::unauthenticated_response())
        .responds(429, super::throttled_response())
        .responds(503, super::unavailable_response())
}

/// `POST /auth/magic-link` — mint a link, and say nothing about the address.
async fn request(
    state: &AuthState,
    address: Option<&ClientIp>,
    headers: &ClientHeaders,
    body: MagicLinkRequest,
    allowlist: &[String],
) -> moso_core::Result<Accepted<Json<AcknowledgedResponse>>> {
    let ctx = support::auth_ctx(address, headers, Some(body.email.as_str()));
    support::gate(state, &ctx, headers.x_captcha_response.as_deref()).await?;

    // Validated here, before it is stored, so that a store an attacker could
    // write to still cannot produce an open redirect at redemption.
    let next = support::checked_next(body.next.as_deref(), allowlist)?;
    let identity = crate::lifecycle::normalise(body.email.as_str());

    // Minted on both paths, so the unknown-address path costs a token mint and
    // a store write exactly as the known one does.
    let link = MagicLink::issue(identity.clone(), MAGIC_LINK_TTL)?;
    let known = state
        .require_accounts()?
        .store()
        .find_by_identity(&identity)
        .await?
        .is_some();

    let kv = state.require_kv()?;
    kv.set_ttl::<MagicLinkToken>(
        link.hash(),
        &MagicLinkClaim {
            identity: identity.clone(),
            expires_at: link.expires_at(),
            next,
        },
        MAGIC_LINK_TTL,
    )
    .await
    .map_err(|error| support::kv_failed(COMPONENT, "set", error))?;

    if known {
        state
            .deliver(Delivery::new(
                DeliveryPurpose::MagicLink,
                identity,
                link.expires_at(),
                link.token().expose(),
            ))
            .await;
    } else {
        // The row is removed rather than never written: writing and deleting is
        // the same two round trips the known path makes, and nothing may be
        // redeemable for an address with no account.
        kv.delete::<MagicLinkToken>(link.hash())
            .await
            .map_err(|error| support::kv_failed(COMPONENT, "delete", error))?;
    }

    // Not recorded as an attempt, for the reason `password::register` gives:
    // a success clears the identity's backoff, and this route is reachable by
    // anybody who knows an address.
    Ok(Accepted::new(Json(AcknowledgedResponse::new())))
}

/// `GET /auth/magic-link/{token}` — redeem it once, and land where it said.
async fn consume(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
    Path(token): Path<String>,
) -> moso_core::Result<Redirect> {
    let kv = state.require_kv()?;
    let digest = MagicLink::hash_of(&token);

    let claim = kv
        .get::<MagicLinkToken>(&digest)
        .await
        .map_err(|error| support::kv_failed(COMPONENT, "get", error))?
        .ok_or(Error::InvalidCredentials)?;

    // Deleted before anything else is decided, so two requests racing on the
    // same link cannot both sign in.
    let claimed = kv
        .delete::<MagicLinkToken>(&digest)
        .await
        .map_err(|error| support::kv_failed(COMPONENT, "delete", error))?;
    if !claimed {
        return Err(Error::InvalidCredentials.into());
    }

    if claim.expires_at <= Utc::now() {
        return Err(Error::Expired { kind: "magic link" }.into());
    }

    let user = state
        .require_accounts()?
        .store()
        .find_by_identity(&claim.identity)
        .await?
        .ok_or(Error::InvalidCredentials)?;
    if !user.is_active() {
        return Err(Error::InvalidCredentials.into());
    }

    super::password::finish(&session, &user, &claim.identity).await?;

    Ok(Redirect::to(
        claim.next.unwrap_or_else(|| DEFAULT_NEXT.to_owned()),
    ))
}

/// The identity a magic link signs in, for a test that needs to read one back.
#[cfg(test)]
async fn claim_for(state: &AuthState, token: &str) -> crate::Result<Option<MagicLinkClaim>> {
    state
        .require_kv()?
        .get::<MagicLinkToken>(&MagicLink::hash_of(token))
        .await
        .map_err(|error| support::kv_failed(COMPONENT, "get", error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AuthState {
        AuthState::new(crate::MemorySessionStore::shared())
            .kv(moso_kv::Kv::in_memory("magic-test").expect("an in-memory kv"))
    }

    #[tokio::test]
    async fn a_link_is_stored_under_the_digest_of_its_token_and_never_under_the_token() {
        let state = state();
        let link = MagicLink::issue("ada@example.com", MAGIC_LINK_TTL).expect("issued");

        state
            .require_kv()
            .expect("a kv")
            .set_ttl::<MagicLinkToken>(
                link.hash(),
                &MagicLinkClaim {
                    identity: "ada@example.com".to_owned(),
                    expires_at: link.expires_at(),
                    next: None,
                },
                MAGIC_LINK_TTL,
            )
            .await
            .expect("stored");

        let key = state
            .require_kv()
            .expect("a kv")
            .key::<MagicLinkToken>(link.hash())
            .expect("a key");
        assert!(!key.as_str().contains(link.token().expose()));
        assert!(key.as_str().contains(link.hash()));

        let claim = claim_for(&state, link.token().expose())
            .await
            .expect("read")
            .expect("found");
        assert_eq!(claim.identity, "ada@example.com");
    }

    #[tokio::test]
    async fn a_link_that_was_never_issued_reads_as_nothing() {
        assert!(
            claim_for(&state(), "not-a-token")
                .await
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn the_digest_of_a_token_is_not_the_token() {
        let link = MagicLink::issue("ada@example.com", MAGIC_LINK_TTL).expect("issued");

        assert_eq!(link.hash().len(), 64);
        assert_ne!(link.hash(), link.token().expose());
        assert_eq!(link.hash(), MagicLink::hash_of(link.token().expose()));
    }
}
