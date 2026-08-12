//! What authorization failures look like, and what each becomes over HTTP.
//!
//! There are only four, and the split matters: a *denial* is a normal outcome
//! that carries a reason, a *missing resource* is a 404 that must not become an
//! existence oracle, an *unknown permission* is a programming mistake caught at
//! boot, and everything else is the store the roles came from being unreachable.
//!
//! Examples that need a `Permission`, a `Role` or a request are `no_run`:
//! constructing one takes a registry, and a doctest that carried a whole
//! registry would document the fixture rather than the method. Everything that
//! can run, runs.

use std::borrow::Cow;

/// The result of every fallible operation in this crate.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// A boxed error from a role store, kept as a source without naming its crate.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Something stopped an authorization decision from being made, or made it a no.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The actor is not allowed to do this.
    ///
    /// Carries the reason the policy gave. Whether the reason reaches the
    /// client depends on the profile: in development it goes into the 403 body,
    /// in production it is logged and the body says "forbidden" — because a
    /// reason like "not the author" tells an attacker who the author is.
    #[error("{action} on {resource} denied: {reason}")]
    Denied {
        /// The action's name.
        action: &'static str,
        /// The resource's name, and its id when there is one.
        resource: Cow<'static, str>,
        /// Why, from [`Decision`](crate::Decision).
        reason: Cow<'static, str>,
    },

    /// The resource the policy was asked about does not exist.
    ///
    /// Produced *before* the policy runs, which leaks existence to an
    /// unauthorised caller. That is the right default — a 403 on a resource
    /// that does not exist is worse, because it confirms the id — and
    /// [`Masked<S>`](crate::Masked) inverts it for the cases where existence
    /// itself is sensitive.
    #[error("no {resource} with that identifier")]
    NotFound {
        /// The resource's name.
        resource: &'static str,
    },

    /// A permission name does not exist in the registry.
    ///
    /// A boot error, not a runtime one: `#[requires("posts.pubish")]` is caught
    /// by [`PermissionRegistry::suggest`](crate::PermissionRegistry::suggest)
    /// before the first request, with a "did you mean" naming the closest real
    /// permission.
    #[error("unknown permission `{name}`{}", suggestion_hint(.suggestion))]
    UnknownPermission {
        /// The name that was written.
        name: String,
        /// The closest registered name, when one is close enough.
        suggestion: Option<String>,
    },

    /// The role store could not be reached.
    ///
    /// Deliberately **not** treated as a denial: an unreachable role store must
    /// produce a 503, because degrading it to "no permissions" turns a cache
    /// outage into a site-wide lockout, and degrading it to "all permissions"
    /// is unthinkable.
    #[error("role source is unavailable: {detail}")]
    Unavailable {
        /// What the store reported.
        detail: String,
        /// The source, when there was one.
        #[source]
        source: Option<BoxError>,
    },
}

/// Render the "did you mean" tail of an [`Error::UnknownPermission`].
fn suggestion_hint(suggestion: &Option<String>) -> String {
    match suggestion {
        Some(name) => format!(" — did you mean `{name}`?"),
        None => String::new(),
    }
}

/// What a 403 says when the process is not in a development profile.
///
/// A policy's reason is written for whoever debugs it — "not the author" tells
/// an attacker who the author is — so it is logged and not sent.
pub const GENERIC_DENIAL: &str = "You do not have permission to perform this action.";

impl Error {
    /// A denial carrying the policy's reason.
    ///
    /// ```
    /// use moso_authz::Error;
    ///
    /// let err = Error::denied("publish", "Post#42", "not the author and not an admin");
    /// assert!(err.is_denied());
    /// assert_eq!(err.reason(), Some("not the author and not an admin"));
    /// ```
    pub fn denied(
        action: &'static str,
        resource: impl Into<Cow<'static, str>>,
        reason: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::Denied {
            action,
            resource: resource.into(),
            reason: reason.into(),
        }
    }

    /// A missing resource.
    ///
    /// ```
    /// use moso_authz::Error;
    ///
    /// assert!(!Error::not_found("Post").is_denied());
    /// ```
    #[must_use]
    pub fn not_found(resource: &'static str) -> Self {
        Self::NotFound { resource }
    }

    /// The role store could not be reached.
    ///
    /// ```
    /// use moso_authz::Error;
    ///
    /// let err = Error::unavailable("connection refused");
    /// assert!(err.to_string().contains("connection refused"));
    /// ```
    #[must_use]
    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self::Unavailable {
            detail: detail.into(),
            source: None,
        }
    }

    /// Whether this is [`Error::Denied`].
    ///
    /// ```
    /// use moso_authz::Error;
    ///
    /// assert!(Error::denied("read", "Post", "no").is_denied());
    /// ```
    #[must_use]
    pub fn is_denied(&self) -> bool {
        matches!(self, Self::Denied { .. })
    }

    /// The policy's reason, for a log line or an explain trace.
    ///
    /// ```
    /// use moso_authz::Error;
    ///
    /// assert_eq!(Error::not_found("Post").reason(), None);
    /// ```
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Denied { reason, .. } => Some(reason),
            _ => None,
        }
    }

    /// The same denial rewritten as a 404.
    ///
    /// What an [`Authorized`](crate::Authorized) built on a
    /// [`Masked<S>`](crate::Masked) source calls. A non-denial is returned
    /// unchanged, so applying it twice is safe.
    ///
    /// ```
    /// use moso_authz::Error;
    ///
    /// let masked = Error::denied("read", "Invoice#1", "not yours").masked("Invoice");
    /// assert!(!masked.is_denied());
    /// ```
    #[must_use]
    pub fn masked(self, resource: &'static str) -> Self {
        match self {
            Self::Denied { .. } => Self::NotFound { resource },
            other => other,
        }
    }

    /// The problem this becomes over HTTP, with the profile deciding how much
    /// of a denial's reason the client sees.
    ///
    /// [`From<Error>`](#impl-From<Error>-for-Error) is this with
    /// `development = false`, because a conversion that guessed wrong would
    /// leak by default. Every path that knows the profile calls this instead.
    ///
    /// ```
    /// use moso_authz::Error;
    ///
    /// let denial = Error::denied("publish", "Post#42", "not the author");
    /// assert!(denial.into_response(true).to_string().contains("not the author"));
    ///
    /// let denial = Error::denied("publish", "Post#42", "not the author");
    /// assert!(!denial.into_response(false).to_string().contains("not the author"));
    /// ```
    #[must_use]
    pub fn into_response(self, development: bool) -> moso_core::Error {
        match self {
            Self::Denied {
                action,
                resource,
                reason,
            } => {
                tracing::info!(
                    target: "moso::authz",
                    action,
                    resource = %resource,
                    reason = %reason,
                    "authorization denied"
                );
                if development {
                    moso_core::Error::forbidden(format!("{action} on {resource} denied: {reason}"))
                } else {
                    moso_core::Error::forbidden(GENERIC_DENIAL)
                }
            }
            Self::NotFound { resource } => moso_core::Error::not_found(resource),
            unknown @ Self::UnknownPermission { .. } => moso_core::Error::internal_msg(format!(
                "{unknown}\n  note: this should have been caught at boot by \
                 `PermissionRegistry::check`; the request was refused",
            )),
            Self::Unavailable { detail, source } => {
                let error = moso_core::Error::unavailable(format!(
                    "the store this request's permissions come from is unreachable: {detail}"
                ));
                match source {
                    Some(source) => error.with_source(source),
                    None => error,
                }
            }
        }
    }
}

impl From<Error> for moso_core::Error {
    /// An authorization failure becomes the HTTP problem it means.
    ///
    /// [`Error::Denied`] is a 403 whose detail is the fixed [`GENERIC_DENIAL`];
    /// [`Error::NotFound`] is a 404; [`Error::UnknownPermission`] is a 500 (it
    /// should have been caught at boot); [`Error::Unavailable`] is a 503 marked
    /// retryable.
    ///
    /// The reason is withheld because a bare `?` does not know the profile.
    /// [`Error::into_response`] is the form that does, and every path inside
    /// this crate uses it.
    fn from(error: Error) -> Self {
        error.into_response(false)
    }
}
