//! The "your devices" listing, and revoking what it shows.
//!
//! # A listing is not a list of credentials
//!
//! A session identifier *is* the credential — signing a cookie with it is all
//! that stands between a client and the account. So the listing hands out a
//! [`handle`](crate::routes::SessionSummary::handle) instead: the SHA-256 of the
//! identifier, which is stable enough to revoke by and useless to present. The
//! revoke path re-derives the handle for each of the caller's own sessions and
//! matches on that, so a handle can only ever name a session its owner already
//! has.
//!
//! | Route | What it does |
//! | --- | --- |
//! | `GET /auth/sessions` | every live session, newest activity first |
//! | `POST /auth/sessions` | re-key *this* session, keeping its contents |
//! | `DELETE /auth/sessions` | revoke every session but this one |
//! | `DELETE /auth/sessions/{handle}` | revoke the one the listing names |

use moso_core::extract::{Json, Path};
use moso_core::response::NoContent;
use moso_core::{Depends, Inject, Router};

use super::support;
use super::{AuthState, SessionSummary};
use crate::{AuthSession, Result};

/// Mount the session listing and its revocations.
pub(crate) fn mount() -> Router {
    Router::new()
        .get("/auth/sessions", list)
        .post("/auth/sessions", rekey)
        .delete("/auth/sessions", revoke_others)
        .delete("/auth/sessions/{handle}", revoke_one)
        .tag(super::AUTH_TAG)
        .responds(401, super::unauthenticated_response())
        .responds(503, super::unavailable_response())
}

/// `GET /auth/sessions` — every live session this account has.
async fn list(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
) -> moso_core::Result<Json<Vec<SessionSummary>>> {
    let subject = support::subject_of(&session)?;
    let current = support::handle_of(&session.id());

    let mut summaries = summarise(&state, &subject, &current).await?;
    summaries.sort_by(|left, right| right.last_seen_at.cmp(&left.last_seen_at));
    Ok(Json(summaries))
}

/// `POST /auth/sessions` — give this session a new identifier.
///
/// The operation a user who suspects their cookie leaked actually wants: the
/// contents survive, the old identifier stops working, and
/// [`SessionLayer`](crate::SessionLayer) writes the replacement cookie on the
/// way out. It is deliberately not a second spelling of `/auth/login`.
async fn rekey(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
) -> moso_core::Result<Json<SessionSummary>> {
    let subject = support::subject_of(&session)?;
    session.cycle_id().await?;
    // Written back now, so the listing below sees the new identifier rather
    // than the one that has just stopped working.
    session.save().await?;

    let current = support::handle_of(&session.id());
    let summaries = summarise(&state, &subject, &current).await?;
    summaries
        .into_iter()
        .find(|summary| summary.current)
        .map(Json)
        .ok_or_else(|| {
            moso_core::Error::from(support::ceremony(
                "the re-keyed session is not in its own listing",
            ))
        })
}

/// `DELETE /auth/sessions` — end every other session.
async fn revoke_others(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
) -> moso_core::Result<NoContent> {
    let subject = support::subject_of(&session)?;
    let current = session.id();

    state
        .session_store()
        .delete_for_user(&subject, Some(&current))
        .await?;
    Ok(NoContent)
}

/// `DELETE /auth/sessions/{handle}` — end the one the listing names.
///
/// A handle that names nothing this account owns is a 404, not a 403: a 403
/// would confirm that the session exists and belongs to somebody else, which is
/// the one thing the answer must not say.
async fn revoke_one(
    Inject(state): Inject<AuthState>,
    Depends(AuthSession(session)): Depends<AuthSession>,
    Path(handle): Path<String>,
) -> moso_core::Result<NoContent> {
    let subject = support::subject_of(&session)?;

    let records = state.session_store().list_for_user(&subject).await?;
    let found = records.into_iter().find(|record| {
        crate::jwks::ct_eq(support::handle_of(&record.id).as_bytes(), handle.as_bytes())
    });

    let Some(record) = found else {
        return Err(moso_core::Error::not_found("Session"));
    };

    if record.id.as_str() == session.id().as_str() {
        // Revoking the session making the request is a logout, and it has to
        // clear the cookie as well as the row.
        session.destroy().await?;
        return Ok(NoContent);
    }

    state.session_store().delete(&record.id).await?;
    Ok(NoContent)
}

/// Every session `subject` owns, as the listing spells them.
async fn summarise(state: &AuthState, subject: &str, current: &str) -> Result<Vec<SessionSummary>> {
    let records = state.session_store().list_for_user(subject).await?;

    Ok(records
        .into_iter()
        .map(|record| {
            let handle = support::handle_of(&record.id);
            SessionSummary {
                current: crate::jwks::ct_eq(handle.as_bytes(), current.as_bytes()),
                handle,
                label: record.device.label.clone(),
                ip: record.device.ip.clone(),
                created_at: support::rfc3339(record.created_at),
                last_seen_at: support::rfc3339(record.last_seen_at),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::store::MemorySessionStore;
    use crate::{DeviceInfo, SessionId, SessionRecord, SessionStore};

    /// A state over a store holding two sessions for one subject.
    async fn state_with_two() -> (AuthState, SessionId, SessionId) {
        let store = Arc::new(MemorySessionStore::new());
        let first = SessionId::generate();
        let second = SessionId::generate();

        for id in [&first, &second] {
            let mut record = SessionRecord::new(id.clone());
            record.user_id = Some("usr_1".to_owned());
            record.device = DeviceInfo {
                label: Some("Firefox on macOS".to_owned()),
                ip: Some("203.0.113.7".to_owned()),
                user_agent: None,
            };
            store
                .save(&record, std::time::Duration::from_secs(60))
                .await
                .expect("saved");
        }

        (
            AuthState::new(store as Arc<dyn SessionStore>),
            first,
            second,
        )
    }

    #[tokio::test]
    async fn a_listing_names_a_session_by_its_handle_and_never_by_its_identifier() {
        let (state, first, second) = state_with_two().await;
        let current = support::handle_of(&first);

        let summaries = summarise(&state, "usr_1", &current).await.expect("listed");

        assert_eq!(summaries.len(), 2);
        for summary in &summaries {
            assert_ne!(summary.handle, first.as_str());
            assert_ne!(summary.handle, second.as_str());
            assert_eq!(summary.handle.len(), 64);
        }
    }

    #[tokio::test]
    async fn exactly_one_entry_in_a_listing_is_the_session_that_asked() {
        let (state, first, _) = state_with_two().await;
        let current = support::handle_of(&first);

        let summaries = summarise(&state, "usr_1", &current).await.expect("listed");

        assert_eq!(
            summaries.iter().filter(|summary| summary.current).count(),
            1
        );
        assert!(
            summaries
                .iter()
                .find(|summary| summary.current)
                .is_some_and(|summary| summary.handle == current)
        );
    }

    #[tokio::test]
    async fn a_listing_carries_the_device_a_session_was_created_from() {
        let (state, first, _) = state_with_two().await;

        let summaries = summarise(&state, "usr_1", &support::handle_of(&first))
            .await
            .expect("listed");

        assert_eq!(summaries[0].label.as_deref(), Some("Firefox on macOS"));
        assert_eq!(summaries[0].ip.as_deref(), Some("203.0.113.7"));
    }
}
