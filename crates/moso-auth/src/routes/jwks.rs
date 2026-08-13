//! The JWKS document, at the path consumers already look at.
//!
//! `GET /.well-known/jwks.json` is mounted at the **root**: the path is
//! well-known, and a verifier that has been told to fetch a JWKS will not go
//! looking under `/auth`.
//!
//! The document is [`Jwt::jwks`](crate::Jwt::jwks) and nothing else. In
//! particular this handler does not assemble one: a symmetric key must never
//! appear in a JWKS — publishing it would publish the signing key — and the
//! decision to drop it belongs next to the signer, not here.

use moso_core::extract::Json;
use moso_core::{Inject, Router};

use super::{AuthState, OpaqueJson};

/// Mount the JWKS document.
pub(crate) fn mount() -> Router {
    Router::new()
        .get("/.well-known/jwks.json", document)
        .tag(super::AUTH_TAG)
        .responds(503, super::unavailable_response())
}

/// `GET /.well-known/jwks.json` — the public half of the signing keys.
async fn document(Inject(state): Inject<AuthState>) -> moso_core::Result<Json<OpaqueJson>> {
    Ok(Json(OpaqueJson(state.require_jwt()?.jwks())))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moso_core::SecretBytes;

    use super::*;
    use crate::{Jwt, JwtAlgorithm, JwtConfig};

    /// A state whose signer has one Ed25519 key.
    fn signing_state() -> AuthState {
        let config = JwtConfig {
            access_ttl: Duration::from_secs(900),
            ..JwtConfig::default()
        };
        let key = SecretBytes::new(crate::jwks::random_bytes(32).expect("randomness"));
        let jwt = Jwt::<crate::Claims>::issuer(config, "k1", key).expect("a signer");

        AuthState::new(crate::MemorySessionStore::shared()).jwt(jwt)
    }

    #[test]
    fn the_document_is_the_signers_own_and_carries_the_key_id() {
        let state = signing_state();
        let document = state.require_jwt().expect("a signer").jwks();

        assert!(document["keys"].is_array());
        assert_eq!(document["keys"][0]["kid"], "k1");
        assert_eq!(document["keys"][0]["alg"], JwtAlgorithm::EdDSA.as_str());
    }

    #[test]
    fn a_deployment_with_no_signer_says_which_builder_call_configures_one() {
        let state = AuthState::new(crate::MemorySessionStore::shared());

        let error = state.require_jwt().expect_err("no signer");
        assert!(error.to_string().contains("AuthState::jwt"), "{error}");
    }
}
