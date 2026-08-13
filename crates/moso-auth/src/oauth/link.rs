//! Account linking, and the one rule in this crate whose loosening is an
//! account-takeover path.
//!
//! # The attack
//!
//! An application supports "sign in with X" and matches the address X asserts
//! against its own accounts. A victim has an account at `ada@example.com`. The
//! attacker signs up at X claiming `ada@example.com`, X does not verify it —
//! or verifies it lazily, or lets a user set it freely on a self-hosted
//! instance — and the attacker signs in as the victim. No password was needed,
//! no email was read, nothing was brute-forced. The application handed the
//! account over because a third party said a name.
//!
//! It is not theoretical. It has been found in production at a long list of
//! companies, it is the reason the OAuth security BCP tells relying parties not
//! to key on `email`, and it is why [`LinkPolicy::default`] refuses.
//!
//! # The rule
//!
//! Linking a provider identity to an *existing* local account requires one of:
//!
//! - the provider asserts a **verified** address that matches the account, or
//! - the request already carries an **authenticated session** — the user is
//!   logged in and is adding a login method, which proves account ownership
//!   independently of anything the provider said.
//!
//! Anything else creates a new account instead. That is a worse experience for
//! a user with two accounts and a far better one than losing theirs.

use std::borrow::Cow;

use crate::oauth::OAuthProfile;
use crate::{Error, Result};

/// What linking a provider account to a local one requires.
///
/// ```
/// use moso_auth::LinkPolicy;
///
/// // The default refuses to link on an unverified address, which is the
/// // documented account-takeover path.
/// assert_eq!(LinkPolicy::default(), LinkPolicy::VerifiedEmailOrSession);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkPolicy {
    /// Link when the provider asserts a **verified** address matching a local
    /// account, or when the request already carries an authenticated session.
    /// The default.
    #[default]
    VerifiedEmailOrSession,
    /// Link only from an authenticated session. The strictest option: a new
    /// social login always creates a new account.
    SessionOnly,
    /// Link on any matching address, verified or not.
    ///
    /// **Dangerous.** Anybody who can create an account at the provider
    /// claiming a victim's address takes over the victim's account here. Only
    /// safe when every configured provider verifies addresses and is trusted to
    /// keep doing so.
    AnyEmail,
}

impl LinkPolicy {
    /// What this policy is called in a log line or an audit record.
    ///
    /// ```
    /// use moso_auth::LinkPolicy;
    ///
    /// assert_eq!(LinkPolicy::AnyEmail.as_str(), "any_email");
    /// ```
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VerifiedEmailOrSession => "verified_email_or_session",
            Self::SessionOnly => "session_only",
            Self::AnyEmail => "any_email",
        }
    }

    /// Whether this policy will ever match on an address alone.
    ///
    /// `false` for [`SessionOnly`](Self::SessionOnly), which is what an audit
    /// or a boot report reads to say "this deployment never links by email".
    ///
    /// ```
    /// use moso_auth::LinkPolicy;
    ///
    /// assert!(!LinkPolicy::SessionOnly.matches_by_email());
    /// assert!(LinkPolicy::default().matches_by_email());
    /// ```
    #[must_use]
    pub fn matches_by_email(self) -> bool {
        !matches!(self, Self::SessionOnly)
    }
}

/// Decide whether a profile may be linked to an existing account.
///
/// The one place the account-takeover rule is enforced, so it cannot be
/// forgotten in a hand-written callback.
///
/// `has_session` is "this request is already authenticated as the account being
/// linked to" — not "some session exists". Passing `true` for a session
/// belonging to a different user turns the check off for that request, which is
/// the mistake this function exists to prevent.
///
/// # Errors
///
/// [`Error::Ceremony`] when the policy refuses, with a
/// reason naming which condition failed.
///
/// ```
/// use moso_auth::{check_link, LinkPolicy, OAuthProfile};
///
/// fn go(profile: &OAuthProfile, authenticated: bool) -> moso_auth::Result<()> {
///     check_link(profile, LinkPolicy::default(), authenticated)
/// }
/// ```
pub fn check_link(profile: &OAuthProfile, policy: LinkPolicy, has_session: bool) -> Result<()> {
    // An authenticated session is proof of account ownership that does not
    // depend on the provider at all, so it satisfies every policy.
    if has_session {
        return Ok(());
    }

    match policy {
        LinkPolicy::SessionOnly => Err(refused(Cow::Borrowed(
            "this deployment links a provider only from an authenticated session, and this \
             request has none; sign in first, then add the provider",
        ))),

        LinkPolicy::VerifiedEmailOrSession => match (&profile.email, profile.email_verified) {
            (Some(_), true) => Ok(()),
            (Some(_), false) => Err(refused(Cow::Owned(format!(
                "`{}` did not say it verified this address, and linking on an unverified one \
                 lets anybody who can claim it at the provider take over the local account; \
                 sign in and add the provider from the account settings, or set \
                 `LinkPolicy::AnyEmail` if every provider you configure is trusted to verify",
                profile.provider.as_str()
            )))),
            (None, _) => Err(refused(Cow::Owned(format!(
                "`{}` returned no address, so there is nothing to match a local account against; \
                 check that the `email` scope was requested and granted",
                profile.provider.as_str()
            )))),
        },

        LinkPolicy::AnyEmail => {
            if profile.email.is_some() {
                Ok(())
            } else {
                Err(refused(Cow::Owned(format!(
                    "`{}` returned no address, so there is nothing to match a local account \
                     against; check that the `email` scope was requested and granted",
                    profile.provider.as_str()
                ))))
            }
        }
    }
}

/// A refusal, tagged as the OAuth ceremony.
fn refused(reason: Cow<'static, str>) -> Error {
    Error::Ceremony {
        ceremony: "oauth",
        reason,
    }
}

#[cfg(test)]
mod tests {
    use moso_core::config::SecretString;

    use super::*;
    use crate::oauth::{ProviderId, TokenSet};

    fn profile(email: Option<&str>, verified: bool) -> OAuthProfile {
        OAuthProfile {
            provider: ProviderId::GitHub,
            subject: "42".to_owned(),
            email: email.map(str::to_owned),
            email_verified: verified,
            name: None,
            picture: None,
            raw: serde_json::Value::Null,
            tokens: TokenSet {
                access_token: SecretString::new("t"),
                refresh_token: None,
                id_token: None,
                expires_at: None,
                scopes: Vec::new(),
            },
        }
    }

    /// The acceptance criterion: the default refuses an unverified-email
    /// auto-link. This is the test that must never be relaxed.
    #[test]
    fn the_default_refuses_an_unverified_address() {
        let error = check_link(
            &profile(Some("ada@example.com"), false),
            LinkPolicy::default(),
            false,
        )
        .expect_err("an unverified address must not link");

        let message = format!("{error}");
        assert!(message.contains("verified"), "{message}");
        assert!(
            message.contains("take over"),
            "the message must say why, not just no: {message}"
        );
    }

    /// A verified address links under the default, which is the whole point of
    /// the distinction.
    #[test]
    fn the_default_accepts_a_verified_address() {
        check_link(
            &profile(Some("ada@example.com"), true),
            LinkPolicy::default(),
            false,
        )
        .expect("a verified address links");
    }

    /// An authenticated session satisfies every policy, because it proves
    /// ownership without the provider's help.
    #[test]
    fn an_authenticated_session_satisfies_every_policy() {
        for policy in [
            LinkPolicy::VerifiedEmailOrSession,
            LinkPolicy::SessionOnly,
            LinkPolicy::AnyEmail,
        ] {
            check_link(&profile(Some("ada@example.com"), false), policy, true)
                .unwrap_or_else(|e| panic!("{policy:?} must accept a session: {e}"));
            check_link(&profile(None, false), policy, true)
                .unwrap_or_else(|e| panic!("{policy:?} must accept a session: {e}"));
        }
    }

    /// `SessionOnly` refuses even a verified address: a new social login always
    /// creates a new account.
    #[test]
    fn session_only_refuses_even_a_verified_address() {
        let error = check_link(
            &profile(Some("ada@example.com"), true),
            LinkPolicy::SessionOnly,
            false,
        )
        .expect_err("SessionOnly links only from a session");
        assert!(format!("{error}").contains("sign in first"), "{error}");
    }

    /// `AnyEmail` is the documented override, and it does what it says — which
    /// is exactly why its documentation says "dangerous".
    #[test]
    fn any_email_is_the_documented_override() {
        check_link(
            &profile(Some("ada@example.com"), false),
            LinkPolicy::AnyEmail,
            false,
        )
        .expect("AnyEmail links on an unverified address, by construction");
    }

    /// No address at all is refused by every email-matching policy, and the
    /// message names the likely cause rather than saying "no".
    #[test]
    fn a_missing_address_is_refused_with_the_likely_cause() {
        for policy in [LinkPolicy::VerifiedEmailOrSession, LinkPolicy::AnyEmail] {
            let error = check_link(&profile(None, true), policy, false)
                .expect_err("there is nothing to match");
            let message = format!("{error}");
            assert!(message.contains("scope"), "{policy:?}: {message}");
        }
    }

    /// Every refusal is an OAuth ceremony failure, so the caller's error
    /// mapping does not have to special-case linking.
    #[test]
    fn every_refusal_is_a_ceremony_failure() {
        let error =
            check_link(&profile(None, false), LinkPolicy::default(), false).expect_err("refused");
        assert!(matches!(
            error,
            Error::Ceremony {
                ceremony: "oauth",
                ..
            }
        ));
    }

    /// The audit spellings are a wire format: they end up in an audit table.
    #[test]
    fn the_policy_names_are_stable() {
        assert_eq!(
            LinkPolicy::VerifiedEmailOrSession.as_str(),
            "verified_email_or_session"
        );
        assert_eq!(LinkPolicy::SessionOnly.as_str(), "session_only");
        assert_eq!(LinkPolicy::AnyEmail.as_str(), "any_email");
        assert!(!LinkPolicy::SessionOnly.matches_by_email());
        assert!(LinkPolicy::AnyEmail.matches_by_email());
    }
}
