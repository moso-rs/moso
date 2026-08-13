//! Turning what each provider returns into one [`OAuthProfile`].
//!
//! Five of the seven built-in providers speak OpenID Connect and return the
//! standard claims. Two do not, and their shapes are the reason this is a
//! module rather than a `serde` derive:
//!
//! | Provider | Subject | Address | Verified? |
//! | --- | --- | --- | --- |
//! | Google, GitLab, Slack | `sub` | `email` | `email_verified`, and it means it |
//! | Microsoft Entra | `sub` | `email` | **never asserted** — see below |
//! | Apple | `sub` (identity token only) | `email` | `email_verified`, sometimes as a string |
//! | GitHub | `id`, a number | a **second request** to `/user/emails` | `verified` on the chosen address |
//! | Discord | `id`, a string | `email` | `verified` |
//!
//! **Entra never asserts verification.** Microsoft's `userinfo` returns no
//! `email_verified` claim and its identity tokens do not carry one either, so
//! Moso reports `false`. Under the default
//! [`LinkPolicy`](crate::LinkPolicy) that means an Entra login will not
//! silently attach itself to an existing local account — which is correct, and
//! is exactly the case the policy exists for: a tenant administrator can set a
//! user's mail attribute to anything.
//!
//! **GitHub's `/user` returns the *public* address**, which most users do not
//! set, and returns `null` for everyone else. The address a GitHub login is
//! worth anything for is the primary verified one, and it takes a second
//! request.

use crate::oauth::idtoken::IdToken;
use crate::oauth::{OAuthProfile, ProviderId, TokenSet};
use crate::{Error, Result};

/// The pieces a profile is assembled from.
#[derive(Default)]
pub(crate) struct RawProfile {
    /// The `userinfo` response, when the provider has such an endpoint.
    pub userinfo: Option<serde_json::Value>,
    /// The verified identity token, when the provider issued one.
    pub id_token: Option<IdToken>,
    /// GitHub's `/user/emails`, which is a second request.
    pub emails: Option<serde_json::Value>,
}

impl RawProfile {
    /// Assemble the profile.
    ///
    /// # Errors
    ///
    /// [`Error::Ceremony`] when the provider returned nothing identifying —
    /// which is a provider that answered 200 with a body that is not a profile,
    /// and is worth failing loudly rather than creating an account keyed on an
    /// empty string.
    pub(crate) fn into_profile(self, id: &ProviderId, tokens: TokenSet) -> Result<OAuthProfile> {
        // The identity token wins over `userinfo` for identity claims: it is
        // signed by the provider and bound to this request by its `nonce`,
        // whereas `userinfo` is whatever came back from a second call made with
        // a bearer token.
        let claims = self
            .id_token
            .as_ref()
            .map(|t| t.raw.clone())
            .unwrap_or(serde_json::Value::Null);
        let userinfo = self.userinfo.unwrap_or(serde_json::Value::Null);

        let (subject, email, email_verified, name, picture) = match id {
            ProviderId::GitHub => github(&userinfo, self.emails.as_ref())?,
            ProviderId::Discord => discord(&userinfo)?,
            _ => oidc(id, &claims, &userinfo)?,
        };

        let raw = merge(claims, userinfo);

        Ok(OAuthProfile {
            provider: id.clone(),
            subject,
            email,
            email_verified,
            name,
            picture,
            raw,
            tokens,
        })
    }
}

/// A five-tuple of subject, address, verification, name and avatar.
type Mapped = (String, Option<String>, bool, Option<String>, Option<String>);

/// The OpenID Connect standard claims, from the identity token first and the
/// `userinfo` response second.
fn oidc(
    id: &ProviderId,
    claims: &serde_json::Value,
    userinfo: &serde_json::Value,
) -> Result<Mapped> {
    let subject = string(claims, "sub")
        .or_else(|| string(userinfo, "sub"))
        .ok_or_else(|| missing(id, "sub"))?;

    let email = string(claims, "email").or_else(|| string(userinfo, "email"));

    // Entra asserts nothing about an address, so nothing is what Moso reports.
    let email_verified = match id {
        ProviderId::Microsoft => false,
        _ => flag(claims, "email_verified") || flag(userinfo, "email_verified"),
    };

    let name = string(claims, "name")
        .or_else(|| string(userinfo, "name"))
        .or_else(|| string(userinfo, "preferred_username"));
    let picture = string(claims, "picture").or_else(|| string(userinfo, "picture"));

    Ok((subject, email, email_verified, name, picture))
}

/// GitHub: a numeric id, and the address from a second request.
fn github(user: &serde_json::Value, emails: Option<&serde_json::Value>) -> Result<Mapped> {
    let subject = user
        .get("id")
        .and_then(|v| match v {
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::String(s) => Some(s.clone()),
            _ => None,
        })
        .ok_or_else(|| missing(&ProviderId::GitHub, "id"))?;

    // `/user/emails` is a list; the one that matters is primary *and* verified.
    // Falling back to any verified address is right — a user with two verified
    // addresses and no primary flag is still verified. Falling back to an
    // unverified one is not, so it is reported as unverified rather than
    // dropped.
    let chosen = emails.and_then(|list| list.as_array()).and_then(|list| {
        list.iter()
            .find(|e| flag(e, "primary") && flag(e, "verified"))
            .or_else(|| list.iter().find(|e| flag(e, "verified")))
            .or_else(|| list.first())
    });

    let (email, verified) = match chosen {
        Some(entry) => (string(entry, "email"), flag(entry, "verified")),
        // The public address, which most users do not set. Never verified as
        // far as this response is concerned.
        None => (string(user, "email"), false),
    };

    Ok((
        subject,
        email,
        verified,
        string(user, "name").or_else(|| string(user, "login")),
        string(user, "avatar_url"),
    ))
}

/// Discord: a snowflake id, `verified`, and an avatar that has to be built.
fn discord(user: &serde_json::Value) -> Result<Mapped> {
    let subject = string(user, "id").ok_or_else(|| missing(&ProviderId::Discord, "id"))?;

    // Discord returns an avatar *hash*, and the URL is built from it. An
    // account with no avatar has `null` here and is served a default by
    // Discord's CDN, which is not worth constructing.
    let picture = string(user, "avatar")
        .map(|hash| format!("https://cdn.discordapp.com/avatars/{subject}/{hash}.png"));

    Ok((
        subject,
        string(user, "email"),
        flag(user, "verified"),
        string(user, "global_name").or_else(|| string(user, "username")),
        picture,
    ))
}

/// Merge the identity token's claims and the `userinfo` response into one
/// object, with the token winning — it is the signed one.
fn merge(claims: serde_json::Value, userinfo: serde_json::Value) -> serde_json::Value {
    match (claims, userinfo) {
        (serde_json::Value::Object(mut a), serde_json::Value::Object(b)) => {
            for (key, value) in b {
                a.entry(key).or_insert(value);
            }
            serde_json::Value::Object(a)
        }
        (serde_json::Value::Null, other) | (other, _) => other,
    }
}

/// A string field, if it is present and is a non-empty string.
fn string(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A boolean field, accepting the string spelling providers sometimes use.
fn flag(value: &serde_json::Value, key: &str) -> bool {
    match value.get(key) {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("true"),
        Some(serde_json::Value::Number(n)) => n.as_i64().is_some_and(|n| n != 0),
        _ => false,
    }
}

/// The provider answered without the one field the flow cannot proceed without.
fn missing(id: &ProviderId, field: &'static str) -> Error {
    Error::Ceremony {
        ceremony: "oauth",
        reason: std::borrow::Cow::Owned(format!(
            "`{}` answered without a `{field}`, so there is no stable identifier to key an \
             account on",
            id.as_str()
        )),
    }
}

#[cfg(test)]
mod tests {
    use moso_core::config::SecretString;

    use super::*;

    fn tokens() -> TokenSet {
        TokenSet {
            access_token: SecretString::new("at"),
            refresh_token: None,
            id_token: None,
            expires_at: None,
            scopes: Vec::new(),
        }
    }

    fn build(id: ProviderId, raw: RawProfile) -> Result<OAuthProfile> {
        raw.into_profile(&id, tokens())
    }

    /// The OIDC path, which five of the seven providers take.
    #[test]
    fn the_standard_claims_map_straight_through() {
        let profile = build(
            ProviderId::Google,
            RawProfile {
                userinfo: Some(serde_json::json!({
                    "sub": "1234567890",
                    "email": "ada@example.com",
                    "email_verified": true,
                    "name": "Ada Lovelace",
                    "picture": "https://example.com/a.png",
                })),
                ..RawProfile::default()
            },
        )
        .expect("the profile maps");

        assert_eq!(profile.subject, "1234567890");
        assert_eq!(profile.email.as_deref(), Some("ada@example.com"));
        assert!(profile.email_verified);
        assert_eq!(profile.name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(profile.identity_key(), "google:1234567890");
    }

    /// Entra asserts nothing about an address, so Moso reports nothing — which
    /// means the default link policy will not attach an Entra login to an
    /// existing account without a session.
    #[test]
    fn entra_never_reports_a_verified_address() {
        let profile = build(
            ProviderId::Microsoft,
            RawProfile {
                userinfo: Some(serde_json::json!({
                    "sub": "abc",
                    "email": "ada@contoso.com",
                    // Even if a claim appeared, Entra does not guarantee it.
                    "email_verified": true,
                })),
                ..RawProfile::default()
            },
        )
        .expect("the profile maps");

        assert!(
            !profile.email_verified,
            "Entra does not assert verification, so Moso must not either"
        );
        assert_eq!(profile.verified_email(), None);
    }

    /// GitHub's primary verified address wins over everything else in the list.
    #[test]
    fn github_takes_the_primary_verified_address() {
        let profile = build(
            ProviderId::GitHub,
            RawProfile {
                userinfo: Some(serde_json::json!({
                    "id": 583231,
                    "login": "octocat",
                    "name": "The Octocat",
                    "email": null,
                    "avatar_url": "https://example.com/o.png",
                })),
                emails: Some(serde_json::json!([
                    {"email": "old@example.com", "primary": false, "verified": true},
                    {"email": "ada@example.com", "primary": true, "verified": true},
                    {"email": "new@example.com", "primary": false, "verified": false},
                ])),
                ..RawProfile::default()
            },
        )
        .expect("the profile maps");

        assert_eq!(profile.subject, "583231", "a numeric id becomes a string");
        assert_eq!(profile.email.as_deref(), Some("ada@example.com"));
        assert!(profile.email_verified);
        assert_eq!(profile.name.as_deref(), Some("The Octocat"));
    }

    /// With no primary flag, any verified address is still verified.
    #[test]
    fn github_falls_back_to_any_verified_address() {
        let profile = build(
            ProviderId::GitHub,
            RawProfile {
                userinfo: Some(serde_json::json!({"id": 1, "login": "a"})),
                emails: Some(serde_json::json!([
                    {"email": "x@example.com", "primary": false, "verified": false},
                    {"email": "y@example.com", "primary": false, "verified": true},
                ])),
                ..RawProfile::default()
            },
        )
        .expect("the profile maps");
        assert_eq!(profile.email.as_deref(), Some("y@example.com"));
        assert!(profile.email_verified);
    }

    /// An unverified GitHub address is reported, and reported as unverified —
    /// dropping it would lose information the application may want, and marking
    /// it verified would be the takeover path.
    #[test]
    fn github_reports_an_unverified_address_as_unverified() {
        let profile = build(
            ProviderId::GitHub,
            RawProfile {
                userinfo: Some(serde_json::json!({"id": 1, "login": "a"})),
                emails: Some(serde_json::json!([
                    {"email": "x@example.com", "primary": true, "verified": false},
                ])),
                ..RawProfile::default()
            },
        )
        .expect("the profile maps");
        assert_eq!(profile.email.as_deref(), Some("x@example.com"));
        assert!(!profile.email_verified);
        assert_eq!(profile.verified_email(), None);
    }

    /// Without the second request, only the public address is available, and it
    /// is never verified as far as `/user` is concerned.
    #[test]
    fn github_without_the_email_call_is_unverified() {
        let profile = build(
            ProviderId::GitHub,
            RawProfile {
                userinfo: Some(serde_json::json!({
                    "id": 1, "login": "a", "email": "public@example.com"
                })),
                ..RawProfile::default()
            },
        )
        .expect("the profile maps");
        assert_eq!(profile.email.as_deref(), Some("public@example.com"));
        assert!(!profile.email_verified);
    }

    /// GitHub's display name falls back to the login, because `name` is
    /// optional and usually empty.
    #[test]
    fn github_falls_back_to_the_login_for_a_name() {
        let profile = build(
            ProviderId::GitHub,
            RawProfile {
                userinfo: Some(serde_json::json!({"id": 1, "login": "octocat"})),
                ..RawProfile::default()
            },
        )
        .expect("the profile maps");
        assert_eq!(profile.name.as_deref(), Some("octocat"));
    }

    /// Discord's avatar is a hash, and the URL has to be built from it.
    #[test]
    fn discord_builds_its_avatar_url() {
        let profile = build(
            ProviderId::Discord,
            RawProfile {
                userinfo: Some(serde_json::json!({
                    "id": "80351110224678912",
                    "username": "nelly",
                    "global_name": "Nelly",
                    "avatar": "8342729096ea3675442027381ff50dfe",
                    "email": "nelly@example.com",
                    "verified": true,
                })),
                ..RawProfile::default()
            },
        )
        .expect("the profile maps");

        assert_eq!(profile.subject, "80351110224678912");
        assert_eq!(profile.name.as_deref(), Some("Nelly"));
        assert_eq!(
            profile.picture.as_deref(),
            Some(
                "https://cdn.discordapp.com/avatars/80351110224678912/\
                 8342729096ea3675442027381ff50dfe.png"
            )
        );
        assert!(profile.email_verified);
    }

    /// An account with no avatar gets no URL rather than a broken one.
    #[test]
    fn discord_without_an_avatar_has_no_picture() {
        let profile = build(
            ProviderId::Discord,
            RawProfile {
                userinfo: Some(serde_json::json!({
                    "id": "1", "username": "n", "avatar": null
                })),
                ..RawProfile::default()
            },
        )
        .expect("the profile maps");
        assert!(profile.picture.is_none());
    }

    /// A provider that answers 200 with a body that is not a profile fails
    /// loudly, rather than creating an account keyed on an empty string.
    #[test]
    fn a_response_without_a_subject_is_refused() {
        for (id, body) in [
            (ProviderId::Google, serde_json::json!({"email": "a@b.c"})),
            (ProviderId::GitHub, serde_json::json!({"login": "a"})),
            (ProviderId::Discord, serde_json::json!({"username": "a"})),
        ] {
            let error = build(
                id.clone(),
                RawProfile {
                    userinfo: Some(body),
                    ..RawProfile::default()
                },
            )
            .expect_err("no subject means no account");
            assert!(
                format!("{error}").contains("stable identifier"),
                "{id:?}: {error}"
            );
        }
    }

    /// The signed identity token wins over `userinfo`, which is a second call
    /// made with a bearer token and is not bound to this request.
    #[test]
    fn the_identity_token_wins_over_userinfo() {
        use base64::Engine as _;
        const B64: base64::engine::general_purpose::GeneralPurpose =
            base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let payload = B64.encode(
            serde_json::to_vec(&serde_json::json!({
                "iss": "https://accounts.google.com",
                "sub": "from-the-token",
                "aud": "c",
                "exp": 9_999_999_999_i64,
                "email": "token@example.com",
                "email_verified": true,
            }))
            .expect("serialises"),
        );
        let header = B64.encode(br#"{"alg":"RS256"}"#);
        let token = IdToken::parse(&format!("{header}.{payload}.sig")).expect("parses");

        let profile = build(
            ProviderId::Google,
            RawProfile {
                id_token: Some(token),
                userinfo: Some(serde_json::json!({
                    "sub": "from-userinfo",
                    "email": "userinfo@example.com",
                    "hd": "example.com",
                })),
                ..RawProfile::default()
            },
        )
        .expect("the profile maps");

        assert_eq!(profile.subject, "from-the-token");
        assert_eq!(profile.email.as_deref(), Some("token@example.com"));
        assert_eq!(
            profile.raw["hd"],
            serde_json::json!("example.com"),
            "userinfo's extra claims survive the merge"
        );
    }

    /// An empty string is not an identifier; treating it as one would key an
    /// account on nothing.
    #[test]
    fn empty_strings_are_not_values() {
        let error = build(
            ProviderId::Google,
            RawProfile {
                userinfo: Some(serde_json::json!({"sub": "", "email": ""})),
                ..RawProfile::default()
            },
        )
        .expect_err("an empty subject is no subject");
        assert!(format!("{error}").contains("stable identifier"), "{error}");
    }
}
