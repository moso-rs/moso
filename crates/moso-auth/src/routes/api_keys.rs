//! Minting, listing and revoking API keys.
//!
//! # The secret exists once
//!
//! [`ApiKey::generate`](crate::ApiKey::generate) returns the record and the
//! secret side by side, and only the record is stored — the key itself is the
//! SHA-256 of a high-entropy value, so a stolen database is a list of digests.
//! `POST /auth/api-keys` is therefore the one and only response that carries
//! [`CreatedApiKey::key`](crate::routes::CreatedApiKey::key); the listing after
//! it shows the prefix and nothing else.
//!
//! # A key belongs to whoever created it
//!
//! Every route here reads the owner from the session rather than from the
//! request, and the revoke path refuses a prefix that does not belong to the
//! caller with a 404 rather than a 403 — a 403 would confirm that the key
//! exists, which is the only thing a prefix is good for guessing.

use std::time::Duration;

use chrono::Utc;
use moso_core::extract::{Json, Path};
use moso_core::response::{Created, NoContent};
use moso_core::{Depends, Inject, Router};

use super::support;
use super::{ApiKeySummary, AuthState, CreateApiKeyRequest, CreatedApiKey};
use crate::{ApiKey, AuthSession, KeyEnvironment};

/// How many seconds a day has, for `expires_in_days`.
const SECONDS_PER_DAY: u64 = 86_400;

/// Mount the API-key routes.
pub(crate) fn mount() -> Router {
    Router::new()
        .get("/auth/api-keys", list)
        .post("/auth/api-keys", create)
        .delete("/auth/api-keys", revoke_all)
        .delete("/auth/api-keys/{prefix}", revoke_one)
        .tag(super::AUTH_TAG)
        .responds(401, super::unauthenticated_response())
        .responds(503, super::unavailable_response())
}

/// `GET /auth/api-keys` — the caller's keys, without their secrets.
async fn list(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
) -> moso_core::Result<Json<Vec<ApiKeySummary>>> {
    let owner = support::subject_of(&session)?;
    let keys = state.require_api_keys()?.list_for_owner(&owner).await?;

    Ok(Json(keys.iter().map(summarise).collect()))
}

/// `POST /auth/api-keys` — mint one, and show the secret exactly here.
async fn create(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
    Json(body): Json<CreateApiKeyRequest>,
) -> moso_core::Result<Created<Json<CreatedApiKey>>> {
    let owner = support::subject_of(&session)?;
    let store = state.require_api_keys()?;

    let environment = if body.test_key.unwrap_or(false) {
        KeyEnvironment::Test
    } else {
        KeyEnvironment::Live
    };

    let mut minted = ApiKey::generate(body.name.clone(), owner, environment)?;
    if let Some(scopes) = body.scopes.clone() {
        minted = minted.with_scopes(scopes);
    }
    if let Some(days) = body.expires_in_days {
        minted = minted.expiring_in(Duration::from_secs(u64::from(days) * SECONDS_PER_DAY));
    }

    store.insert(&minted.record).await?;

    let created = CreatedApiKey {
        key: minted.secret.expose().to_owned(),
        prefix: minted.record.prefix.clone(),
        name: minted.record.name.clone(),
        expires_at: minted.record.expires_at.map(support::rfc3339),
    };
    let location = format!("/auth/api-keys/{}", created.prefix);
    Ok(Created::at(location, Json(created)))
}

/// `DELETE /auth/api-keys` — revoke every key the caller owns.
async fn revoke_all(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
) -> moso_core::Result<NoContent> {
    let owner = support::subject_of(&session)?;
    let store = state.require_api_keys()?;

    for key in store.list_for_owner(&owner).await? {
        if key.revoked_at.is_none() {
            store.revoke(key.id).await?;
        }
    }
    Ok(NoContent)
}

/// `DELETE /auth/api-keys/{prefix}` — revoke the one the listing names.
async fn revoke_one(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
    Path(prefix): Path<String>,
) -> moso_core::Result<NoContent> {
    let owner = support::subject_of(&session)?;
    let store = state.require_api_keys()?;

    let found = store.find_by_prefix(&prefix).await?;
    // One answer for "no such prefix" and "somebody else's prefix": the second
    // would otherwise confirm that a guessed prefix is a real key.
    let Some(key) = found.filter(|key| key.owner == owner) else {
        return Err(moso_core::Error::not_found("API key"));
    };

    store.revoke(key.id).await?;
    Ok(NoContent)
}

/// One stored key, as the listing spells it.
fn summarise(key: &ApiKey) -> ApiKeySummary {
    ApiKeySummary {
        prefix: key.prefix.clone(),
        name: key.name.clone(),
        environment: key.environment.as_str().to_owned(),
        scopes: key.scopes.clone(),
        created_at: support::rfc3339(key.created_at),
        expires_at: key.expires_at.map(support::rfc3339),
        last_used_at: key.last_used_at.map(support::rfc3339),
        revoked: !key.is_usable(Utc::now()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_entry_carries_the_prefix_and_never_the_secret() {
        let minted = ApiKey::generate("deploy bot", "usr_1", KeyEnvironment::Live)
            .expect("a key")
            .with_scopes(["deploy.run"]);

        let summary = summarise(&minted.record);

        assert_eq!(summary.prefix, minted.record.prefix);
        assert_eq!(summary.environment, "live");
        assert_eq!(summary.scopes, ["deploy.run"]);
        assert!(!summary.revoked);

        let rendered = serde_json::to_string(&summary).expect("json");
        assert!(
            !rendered.contains(minted.secret.expose()),
            "a listing must never carry a secret"
        );
        assert!(!rendered.contains(&minted.record.hash));
    }

    #[test]
    fn an_expired_key_reads_as_revoked_rather_than_as_live() {
        let minted = ApiKey::generate("expired", "usr_1", KeyEnvironment::Test)
            .expect("a key")
            .expiring_in(Duration::from_secs(0));

        assert!(summarise(&minted.record).revoked);
    }
}
