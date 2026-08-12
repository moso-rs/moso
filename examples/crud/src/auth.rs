//! Who is calling, and what they are allowed to do.
//!
//! Two mechanisms, deliberately distinct:
//!
//! - [`ApiKeyGuard`] is a **route** concern that authenticates the *client*. It
//!   runs before extraction, applies to every write route at once, and
//!   contributes its 401 and its security requirement to the OpenAPI document.
//!   Its check is not hand-rolled: it delegates to the `moso-auth` battery's
//!   [`ApiKeyAuthenticator`], which parses the
//!   `mso_…` key, looks its prefix up in a store with one indexed query, and
//!   verifies the secret against a stored SHA-256 in constant time. The example
//!   seeds one key at boot (see [`crate::seeded`]); a real deployment issues
//!   them.
//! - [`Actor`] and [`Editor`] are **request** concerns that model *who the
//!   client is acting as*. They are dependencies: resolved on demand, memoised
//!   for the length of one request, and injected into the handlers that ask for
//!   them. Authentication (is this a valid key?) and identity (which author?)
//!   are orthogonal, so the battery owns the first and these headers stand in
//!   for the second — a real deployment resolves the author from the
//!   authenticated principal instead, and nothing else changes.

use moso::auth::{ApiKey, ApiKeyAuthenticator, ApiKeyStore, KeyEnvironment, MemoryApiKeyStore};
use moso::deps::http::request::Parts;
use moso::openapi::SecurityRequirement;
use moso::prelude::*;
use moso::{BoxFuture, Guard, ProviderReq};

use std::sync::Arc;

/// The header a write request must carry its API key in.
pub const API_KEY_HEADER: &str = "x-api-key";

/// The name the API key scheme is declared under in the document.
pub const API_KEY_SCHEME: &str = "api_key";

/// The header this example takes an author's name from.
pub const AUTHOR_HEADER: &str = "x-author";

/// The header that promotes a caller to an editor.
pub const ROLE_HEADER: &str = "x-role";

/// The author used when the caller does not name themselves.
pub const ANONYMOUS: &str = "anonymous";

// ---------------------------------------------------------------------------
// The battery-backed key store
// ---------------------------------------------------------------------------

/// Seed one API key into a fresh in-memory store and hand back the authenticator
/// plus the one-time secret.
///
/// The secret is returned because a generated key is random and shown exactly
/// once: the composition root provides it (see [`crate::DemoApiKey`]) so that a
/// fresh `cargo run` — and every test — has a working credential. A real
/// deployment never seeds a key at boot; it mints them through an admin flow and
/// stores only their hashes.
///
/// # Errors
/// Only if the OS random generator is unavailable, which
/// [`ApiKey::generate`](moso::auth::ApiKey::generate) reports.
pub async fn seed_api_key() -> Result<(ApiKeyAuthenticator, String)> {
    let store: Arc<dyn ApiKeyStore> = Arc::new(MemoryApiKeyStore::new());
    let new = ApiKey::generate("example writer", "example", KeyEnvironment::Live)
        .map_err(Error::internal)?;
    let secret = new.secret.expose().to_owned();
    store.insert(&new.record).await.map_err(Error::internal)?;

    // Read keys from the same `x-api-key` header the document advertises, rather
    // than only `Authorization: Bearer`, so the wire contract is unchanged.
    let authenticator = ApiKeyAuthenticator::new(store).header(API_KEY_HEADER);
    Ok((authenticator, secret))
}

// ---------------------------------------------------------------------------
// The guard
// ---------------------------------------------------------------------------

/// Rejects any request that does not present a valid API key.
///
/// Applied with `.guard(ApiKeyGuard)` to the write half of the posts router, so
/// one line protects four operations and documents all four.
#[derive(Debug, Clone, Copy)]
pub struct ApiKeyGuard;

impl Guard for ApiKeyGuard {
    fn describe(&self, op: &mut OperationBuilder) {
        op.security(SecurityRequirement::scheme(API_KEY_SCHEME));
        op.response(
            401,
            ResponseSpec::problem("The `x-api-key` header is absent or not a valid key."),
        );
    }

    fn check<'a>(&'a self, parts: &'a Parts, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // `App::build()` proved the authenticator is provided, so this
            // cannot be a missing-provider failure at request time.
            let authenticator = ctx.provider::<ApiKeyAuthenticator>()?;

            let Some(presented) = authenticator.presented_in(&parts.headers) else {
                return Err(Error::unauthenticated()
                    .with_detail("Present your API key in the `x-api-key` header"));
            };

            // The battery does the parse, the indexed prefix lookup, the
            // constant-time secret check and the expiry/revocation checks. Every
            // failure it can report is, to a caller without the key,
            // indistinguishable — which is the point.
            authenticator
                .authenticate(&presented)
                .await
                .map(|_key| ())
                .map_err(|_| {
                    Error::unauthenticated()
                        .with_detail("The `x-api-key` header is not a valid key")
                })
        })
    }
}

// ---------------------------------------------------------------------------
// The dependencies
// ---------------------------------------------------------------------------

/// Who the request is acting as.
///
/// A hand-written `Dependency`: resolved from the request, cached for the rest
/// of it, and available to any handler that asks for `Depends<Actor>`. Two
/// parameters asking for it in the same request get the same value, resolved
/// once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// The name posts are attributed to.
    pub name: String,
    /// Whether this caller may publish and may see every draft.
    pub editor: bool,
}

impl Actor {
    /// The anonymous reader.
    #[must_use]
    pub fn anonymous() -> Self {
        Self {
            name: ANONYMOUS.to_owned(),
            editor: false,
        }
    }

    /// Read an actor out of the request headers.
    ///
    /// Split out of [`Dependency::resolve`] so that every branch can be tested
    /// without building a `RequestCtx`.
    #[must_use]
    pub fn from_headers(headers: &moso::deps::http::HeaderMap) -> Self {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };

        Self {
            name: header(AUTHOR_HEADER).unwrap_or(ANONYMOUS).to_owned(),
            editor: header(ROLE_HEADER).is_some_and(|role| role.eq_ignore_ascii_case("editor")),
        }
    }
}

impl Dependency for Actor {
    const PROVIDER_REQ: &'static [ProviderReq] = &[];

    async fn resolve(ctx: &RequestCtx) -> Result<Self> {
        Ok(Self::from_headers(ctx.headers()))
    }
}

/// An [`Actor`] that passed the editor check.
///
/// `#[derive(Dependency)]` writes the resolve-then-check body: it resolves the
/// `from` type through the same per-request cache, tests the named field, and
/// answers 403 with this message when it is false. Asking for `Depends<Editor>`
/// in a handler signature is the whole authorisation rule.
#[derive(Dependency, Debug, Clone)]
#[depends(from = Actor, check = "editor", error = "only an editor may publish a post")]
pub struct Editor(pub Actor);

#[cfg(test)]
mod tests {
    use super::*;
    use moso::deps::http::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                moso::deps::http::HeaderName::from_bytes(name.as_bytes()).expect("valid name"),
                HeaderValue::from_str(value).expect("valid value"),
            );
        }
        map
    }

    #[test]
    fn a_request_with_no_headers_is_anonymous_and_not_an_editor() {
        let actor = Actor::from_headers(&HeaderMap::new());
        assert_eq!(actor, Actor::anonymous());
    }

    #[test]
    fn the_author_header_names_the_actor() {
        let actor = Actor::from_headers(&headers(&[(AUTHOR_HEADER, "ada")]));
        assert_eq!(actor.name, "ada");
        assert!(!actor.editor);
    }

    #[test]
    fn the_role_header_is_matched_case_insensitively() {
        assert!(Actor::from_headers(&headers(&[(ROLE_HEADER, "Editor")])).editor);
        assert!(!Actor::from_headers(&headers(&[(ROLE_HEADER, "author")])).editor);
    }

    #[test]
    fn a_blank_author_header_does_not_become_a_blank_author() {
        let actor = Actor::from_headers(&headers(&[(AUTHOR_HEADER, "   ")]));
        assert_eq!(actor.name, ANONYMOUS);
    }

    #[tokio::test]
    async fn a_seeded_key_authenticates_and_a_wrong_one_does_not() {
        let (authenticator, secret) = seed_api_key().await.expect("a key is seeded");
        assert!(authenticator.authenticate(&secret).await.is_ok());
        assert!(
            authenticator
                .authenticate("mso_live_deadbeef_nope")
                .await
                .is_err()
        );
    }
}
