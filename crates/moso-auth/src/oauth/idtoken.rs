//! The OIDC identity token, and the `nonce` check that is the whole reason it
//! is read at all.
//!
//! # Why the signature is not re-verified here
//!
//! In the **authorization code flow**, the identity token is fetched by this
//! process, over TLS, directly from the token endpoint, using a URL that came
//! from a discovery document or from the built-in table. Nothing untrusted
//! touched it. OpenID Connect Core §3.1.3.7 says so explicitly:
//!
//! > If the ID Token is received via direct communication between the Client
//! > and the Token Endpoint (which it is in this flow), the TLS server
//! > validation MAY be used to validate the issuer in place of checking the
//! > token signature.
//!
//! Re-verifying it would mean fetching a JWKS, caching it, rotating it, and
//! being wrong about `kid` handling — an entire second trust path, in exchange
//! for defending against an attacker who has already broken TLS to the token
//! endpoint and can therefore simply return whatever tokens they like.
//!
//! This is **not** true of the implicit or hybrid flows, where the token
//! arrives through the browser. Moso does not implement those, for the same
//! reason it always sends PKCE.
//!
//! What *is* checked here is everything the transport cannot establish:
//! `iss`, `aud`, `azp`, `exp`, `iat`, and `nonce`. A token with the right
//! signature and the wrong audience is a token minted for a different
//! application, and accepting it is a real cross-client attack.

use std::borrow::Cow;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// URL-safe base64 without padding — RFC 7515's encoding for a JWT segment.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// How far a clock may be out before `exp` and `iat` are believed.
///
/// Sixty seconds is the usual allowance; a server whose clock is further out
/// than that has a problem that silently widening the window will not fix.
///
/// ```
/// assert_eq!(moso_auth::oauth::CLOCK_SKEW.as_secs(), 60);
/// ```
pub const CLOCK_SKEW: std::time::Duration = std::time::Duration::from_secs(60);

/// The claims of an OIDC identity token.
///
/// The registered claims plus the profile ones every provider sends. Anything
/// else stays in [`IdToken::raw`].
///
/// ```no_run
/// use moso_auth::oauth::IdTokenClaims;
///
/// # fn f(c: &IdTokenClaims) {
/// let _ = &c.sub;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IdTokenClaims {
    /// Who issued it.
    pub iss: String,
    /// Whose account it describes — the stable join key.
    pub sub: String,
    /// Who it was minted for. A string or an array, per the specification.
    #[serde(default)]
    pub aud: Audience,
    /// The authorized party, when `aud` has more than one entry.
    #[serde(default)]
    pub azp: Option<String>,
    /// When it stops being valid, as a Unix timestamp.
    pub exp: i64,
    /// When it was issued.
    #[serde(default)]
    pub iat: Option<i64>,
    /// The value that binds it to one authorization request.
    #[serde(default)]
    pub nonce: Option<String>,
    /// The address, when the provider sent one.
    #[serde(default)]
    pub email: Option<String>,
    /// Whether the provider says it verified that address.
    ///
    /// Apple sends this as the *string* `"true"`, so the deserialiser accepts
    /// both spellings. A provider that omits it is treated as unverified.
    #[serde(default, deserialize_with = "lenient_bool")]
    pub email_verified: bool,
    /// A display name.
    #[serde(default)]
    pub name: Option<String>,
    /// An avatar.
    #[serde(default)]
    pub picture: Option<String>,
}

/// A JWT `aud`, which is either one string or a list of them.
///
/// ```
/// use moso_auth::oauth::Audience;
///
/// let one: Audience = serde_json::from_str(r#""client-id""#).unwrap();
/// assert!(one.contains("client-id"));
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Audience {
    /// No `aud` at all, which is not valid for an identity token but is what a
    /// malformed one looks like.
    #[default]
    Absent,
    /// One audience.
    One(String),
    /// Several, in which case `azp` says which one asked.
    Many(Vec<String>),
}

impl Audience {
    /// Whether `client_id` is among the audiences.
    ///
    /// ```
    /// use moso_auth::oauth::Audience;
    ///
    /// assert!(Audience::One("a".to_owned()).contains("a"));
    /// assert!(!Audience::Absent.contains("a"));
    /// ```
    #[must_use]
    pub fn contains(&self, client_id: &str) -> bool {
        match self {
            Self::Absent => false,
            Self::One(one) => one == client_id,
            Self::Many(many) => many.iter().any(|a| a == client_id),
        }
    }

    /// How many audiences there are.
    ///
    /// ```
    /// use moso_auth::oauth::Audience;
    ///
    /// assert_eq!(Audience::Many(vec!["a".to_owned(), "b".to_owned()]).len(), 2);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Absent => 0,
            Self::One(_) => 1,
            Self::Many(many) => many.len(),
        }
    }

    /// Whether there is no audience at all.
    ///
    /// ```
    /// use moso_auth::oauth::Audience;
    ///
    /// assert!(Audience::Absent.is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Accept `true`, `"true"` and `1` for a boolean claim.
///
/// Apple sends `email_verified` as a string; Google has sent it both ways over
/// the years. A strict deserialiser here would make an Apple login fail at the
/// last step with "invalid type: string".
fn lenient_bool<'de, D>(deserializer: D) -> core::result::Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Bool(b) => b,
        serde_json::Value::String(s) => s.eq_ignore_ascii_case("true"),
        serde_json::Value::Number(n) => n.as_i64().is_some_and(|n| n != 0),
        _ => false,
    })
}

/// A parsed identity token.
///
/// ```no_run
/// use moso_auth::oauth::IdToken;
///
/// # fn f(t: &IdToken) {
/// let _ = &t.claims.sub;
/// # }
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct IdToken {
    /// The claims Moso understands.
    pub claims: IdTokenClaims,
    /// Every claim, including the ones it does not.
    pub raw: serde_json::Value,
    /// The `alg` from the header, for a log line and for a caller that wants to
    /// verify the signature itself against the provider's JWKS.
    pub algorithm: String,
    /// The `kid` from the header, for the same reason.
    pub key_id: Option<String>,
}

impl IdToken {
    /// Parse a compact-serialisation JWT without verifying its signature.
    ///
    /// Safe **only** for a token fetched directly from a token endpoint over
    /// TLS. See the module documentation.
    ///
    /// # Errors
    ///
    /// [`Error::Ceremony`] when the token is not three base64url segments, when
    /// a segment is not JSON, or when the header declares `alg: none` — which
    /// is not something a token endpoint sends and is the signature of a
    /// hand-rolled forgery.
    ///
    /// ```no_run
    /// use moso_auth::oauth::IdToken;
    ///
    /// # fn f(token: &str) -> moso_auth::Result<IdToken> {
    /// IdToken::parse(token)
    /// # }
    /// ```
    pub fn parse(token: &str) -> Result<Self> {
        let mut segments = token.split('.');
        let (Some(header), Some(payload), Some(signature), None) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            return Err(refused(Cow::Borrowed(
                "the identity token is not a three-segment JWT",
            )));
        };

        let header: serde_json::Value = decode_segment(header, "header")?;
        let algorithm = header
            .get("alg")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();

        if algorithm.eq_ignore_ascii_case("none") || signature.is_empty() {
            return Err(refused(Cow::Borrowed(
                "the identity token is unsigned (`alg: none`); a token endpoint does not issue \
                 those, so this one was not minted by the provider",
            )));
        }

        let raw: serde_json::Value = decode_segment(payload, "payload")?;
        let claims: IdTokenClaims = serde_json::from_value(raw.clone())
            .map_err(|e| refused(Cow::Owned(format!("the identity token is missing {e}"))))?;

        Ok(Self {
            claims,
            raw,
            algorithm,
            key_id: header
                .get("kid")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        })
    }

    /// Check every claim the transport cannot establish.
    ///
    /// - `iss` must equal the expected issuer exactly, when one is known.
    /// - `aud` must contain the client id; when there is more than one
    ///   audience, `azp` must be the client id too.
    /// - `exp` must be in the future and `iat` not far in it, within
    ///   [`CLOCK_SKEW`].
    /// - `nonce` must equal the one this authorization request sent, when the
    ///   request sent one. **This is the replay defence**: without it, an
    ///   identity token captured from another sign-in of the same user at the
    ///   same application can be replayed here.
    ///
    /// # Errors
    ///
    /// [`Error::Ceremony`], naming which claim failed — in the log; a caller
    /// shows the client the same failure for all of them.
    ///
    /// ```no_run
    /// # use moso_auth::oauth::IdToken;
    /// # fn f(t: &IdToken) -> moso_auth::Result<()> {
    /// t.check(Some("https://accounts.google.com"), "my-client-id", Some("the-nonce"), 0)
    /// # }
    /// ```
    pub fn check(
        &self,
        issuer: Option<&str>,
        client_id: &str,
        nonce: Option<&str>,
        now: i64,
    ) -> Result<()> {
        let now = if now == 0 { unix_now() } else { now };
        let skew = i64::try_from(CLOCK_SKEW.as_secs()).unwrap_or(60);

        if let Some(expected) = issuer
            && self.claims.iss != expected
        {
            return Err(refused(Cow::Owned(format!(
                "the identity token says it was issued by `{}`, and this provider is configured \
                 as `{expected}`",
                self.claims.iss
            ))));
        }
        if self.claims.iss.is_empty() {
            return Err(refused(Cow::Borrowed(
                "the identity token carries no issuer",
            )));
        }
        if self.claims.sub.is_empty() {
            return Err(refused(Cow::Borrowed(
                "the identity token carries no subject, so there is no account to attach it to",
            )));
        }

        if !self.claims.aud.contains(client_id) {
            return Err(refused(Cow::Owned(format!(
                "the identity token was minted for a different application ({:?}, not `{}`); a \
                 token from another client is not a login here",
                self.claims.aud, client_id
            ))));
        }
        if self.claims.aud.len() > 1 && self.claims.azp.as_deref() != Some(client_id) {
            return Err(refused(Cow::Borrowed(
                "the identity token names several audiences and its `azp` is not this client, so \
                 it was authorized for somebody else",
            )));
        }

        if self.claims.exp + skew < now {
            return Err(refused(Cow::Owned(format!(
                "the identity token expired {}s ago",
                now - self.claims.exp
            ))));
        }
        if let Some(iat) = self.claims.iat
            && iat > now + skew
        {
            return Err(refused(Cow::Owned(format!(
                "the identity token claims to have been issued {}s in the future; check the \
                 server clock",
                iat - now
            ))));
        }

        match (nonce, self.claims.nonce.as_deref()) {
            (None, _) => {}
            (Some(expected), Some(actual)) if constant_time_eq(expected, actual) => {}
            (Some(_), Some(_)) => {
                return Err(refused(Cow::Borrowed(
                    "the identity token's `nonce` is not the one this authorization request sent, \
                     so the token belongs to a different sign-in",
                )));
            }
            (Some(_), None) => {
                return Err(refused(Cow::Borrowed(
                    "this authorization request sent a `nonce` and the identity token carries \
                     none, so nothing binds the token to this sign-in",
                )));
            }
        }

        Ok(())
    }
}

/// Decode one base64url JWT segment as JSON.
fn decode_segment(segment: &str, which: &'static str) -> Result<serde_json::Value> {
    let bytes = B64.decode(segment).map_err(|_| {
        refused(Cow::Owned(format!(
            "the identity token's {which} is not base64url"
        )))
    })?;
    serde_json::from_slice(&bytes).map_err(|e| {
        refused(Cow::Owned(format!(
            "the identity token's {which} is not JSON: {e}"
        )))
    })
}

/// Compare two strings without returning early on the first difference.
///
/// A `nonce` comparison is not a classic timing oracle — the attacker does not
/// get to iterate quickly — but it costs nothing to do properly and the habit
/// is what keeps the one that *does* matter correct.
fn constant_time_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq as _;
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// Seconds since the Unix epoch.
fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
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
    use super::*;

    /// Build a token with the given claims. Signed with a fixed nonsense
    /// signature, which is exactly what the code flow does not check — and the
    /// tests below prove it *does* check everything else.
    fn token(claims: serde_json::Value) -> String {
        let header = B64.encode(br#"{"alg":"RS256","kid":"k1"}"#);
        let payload = B64.encode(serde_json::to_vec(&claims).expect("claims serialise"));
        format!("{header}.{payload}.c2lnbmF0dXJl")
    }

    fn claims(now: i64) -> serde_json::Value {
        serde_json::json!({
            "iss": "https://accounts.google.com",
            "sub": "1234567890",
            "aud": "my-client-id",
            "exp": now + 3600,
            "iat": now,
            "nonce": "the-nonce",
            "email": "ada@example.com",
            "email_verified": true,
            "name": "Ada",
        })
    }

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn a_well_formed_token_parses_and_checks() {
        let parsed = IdToken::parse(&token(claims(NOW))).expect("parses");
        assert_eq!(parsed.claims.sub, "1234567890");
        assert_eq!(parsed.algorithm, "RS256");
        assert_eq!(parsed.key_id.as_deref(), Some("k1"));
        assert!(parsed.claims.email_verified);

        parsed
            .check(
                Some("https://accounts.google.com"),
                "my-client-id",
                Some("the-nonce"),
                NOW,
            )
            .expect("every claim checks out");
    }

    /// The acceptance criterion: a `nonce` that does not match must be refused.
    /// Without it, an identity token captured from another sign-in of the same
    /// user at the same application can be replayed.
    #[test]
    fn a_mismatched_nonce_is_refused() {
        let parsed = IdToken::parse(&token(claims(NOW))).expect("parses");
        let error = parsed
            .check(None, "my-client-id", Some("a-different-nonce"), NOW)
            .expect_err("a replayed token must not be accepted");
        assert!(format!("{error}").contains("nonce"), "{error}");
    }

    /// A token with no `nonce` at all, when one was sent, is the same attack
    /// with the field stripped.
    #[test]
    fn a_missing_nonce_is_refused_when_one_was_sent() {
        let mut c = claims(NOW);
        c.as_object_mut().expect("object").remove("nonce");
        let parsed = IdToken::parse(&token(c)).expect("parses");

        let error = parsed
            .check(None, "my-client-id", Some("the-nonce"), NOW)
            .expect_err("a token with no nonce is not bound to this sign-in");
        assert!(format!("{error}").contains("nonce"), "{error}");

        // …and a provider that is not OIDC sends no nonce and is not checked.
        parsed
            .check(None, "my-client-id", None, NOW)
            .expect("no nonce was sent, so none is required");
    }

    /// A token minted for another client is a real cross-application attack:
    /// the signature verifies, the issuer is right, and it still is not a login
    /// here.
    #[test]
    fn a_token_for_another_client_is_refused() {
        let parsed = IdToken::parse(&token(claims(NOW))).expect("parses");
        let error = parsed
            .check(None, "somebody-elses-client-id", Some("the-nonce"), NOW)
            .expect_err("the audience does not match");
        assert!(
            format!("{error}").contains("different application"),
            "{error}"
        );
    }

    /// Several audiences require `azp` to name this client.
    #[test]
    fn several_audiences_require_azp() {
        let mut c = claims(NOW);
        c["aud"] = serde_json::json!(["my-client-id", "another-client"]);
        let parsed = IdToken::parse(&token(c.clone())).expect("parses");
        let error = parsed
            .check(None, "my-client-id", Some("the-nonce"), NOW)
            .expect_err("azp is missing");
        assert!(format!("{error}").contains("azp"), "{error}");

        c["azp"] = serde_json::json!("my-client-id");
        IdToken::parse(&token(c))
            .expect("parses")
            .check(None, "my-client-id", Some("the-nonce"), NOW)
            .expect("azp names this client");
    }

    /// An issuer that is not the configured one means the token came from
    /// somewhere else entirely.
    #[test]
    fn a_wrong_issuer_is_refused() {
        let parsed = IdToken::parse(&token(claims(NOW))).expect("parses");
        let error = parsed
            .check(
                Some("https://login.microsoftonline.com/common/v2.0"),
                "my-client-id",
                Some("the-nonce"),
                NOW,
            )
            .expect_err("the issuer does not match");
        assert!(format!("{error}").contains("issued by"), "{error}");
    }

    /// Expiry is checked with a clock-skew allowance in one direction only.
    #[test]
    fn expiry_is_checked_with_a_bounded_allowance() {
        let mut c = claims(NOW);
        c["exp"] = serde_json::json!(NOW - 30);
        IdToken::parse(&token(c.clone()))
            .expect("parses")
            .check(None, "my-client-id", None, NOW)
            .expect("thirty seconds is inside the skew allowance");

        c["exp"] = serde_json::json!(NOW - 3600);
        let error = IdToken::parse(&token(c))
            .expect("parses")
            .check(None, "my-client-id", None, NOW)
            .expect_err("an hour is not");
        assert!(format!("{error}").contains("expired"), "{error}");
    }

    /// A token issued far in the future means a broken clock somewhere, and
    /// saying which is the useful part.
    #[test]
    fn a_future_issue_time_names_the_clock() {
        let mut c = claims(NOW);
        c["iat"] = serde_json::json!(NOW + 7200);
        let error = IdToken::parse(&token(c))
            .expect("parses")
            .check(None, "my-client-id", None, NOW)
            .expect_err("issued in the future");
        assert!(format!("{error}").contains("clock"), "{error}");
    }

    /// `alg: none` is not something a token endpoint sends. It is the oldest
    /// JWT forgery there is, and it must be refused structurally rather than by
    /// a signature check that is deliberately not run here.
    #[test]
    fn an_unsigned_token_is_refused() {
        let header = B64.encode(br#"{"alg":"none"}"#);
        let payload = B64.encode(serde_json::to_vec(&claims(NOW)).expect("serialises"));

        for token in [
            format!("{header}.{payload}."),
            format!("{header}.{payload}.x"),
        ] {
            let error = IdToken::parse(&token).expect_err("`alg: none` must be refused");
            assert!(format!("{error}").contains("unsigned"), "{error}");
        }

        // …and so is a signed-looking token with an empty signature.
        let header = B64.encode(br#"{"alg":"RS256"}"#);
        let error = IdToken::parse(&format!("{header}.{payload}."))
            .expect_err("an empty signature must be refused");
        assert!(format!("{error}").contains("unsigned"), "{error}");
    }

    /// Malformed input is refused with a reason, not a panic.
    #[test]
    fn malformed_tokens_are_refused() {
        for bad in ["", "a.b", "a.b.c.d", "!!!.!!!.x"] {
            let error = IdToken::parse(bad).expect_err("malformed");
            assert!(matches!(
                error,
                Error::Ceremony {
                    ceremony: "oauth",
                    ..
                }
            ));
        }
    }

    /// Apple sends `email_verified` as a string. A strict deserialiser would
    /// make every Apple login fail at the last step.
    #[test]
    fn a_string_email_verified_is_accepted() {
        let mut c = claims(NOW);
        c["email_verified"] = serde_json::json!("true");
        assert!(
            IdToken::parse(&token(c.clone()))
                .expect("parses")
                .claims
                .email_verified
        );

        c["email_verified"] = serde_json::json!("false");
        assert!(
            !IdToken::parse(&token(c.clone()))
                .expect("parses")
                .claims
                .email_verified
        );

        c["email_verified"] = serde_json::json!(1);
        assert!(
            IdToken::parse(&token(c))
                .expect("parses")
                .claims
                .email_verified
        );
    }

    /// Claims Moso does not model stay reachable, because an application will
    /// eventually need one.
    #[test]
    fn unknown_claims_survive_in_raw() {
        let mut c = claims(NOW);
        c["hd"] = serde_json::json!("example.com");
        let parsed = IdToken::parse(&token(c)).expect("parses");
        assert_eq!(parsed.raw["hd"], serde_json::json!("example.com"));
    }

    /// The audience shape is either form the specification allows.
    #[test]
    fn the_audience_accepts_both_shapes() {
        let one: Audience = serde_json::from_str(r#""a""#).expect("parses");
        assert!(one.contains("a"));
        assert_eq!(one.len(), 1);

        let many: Audience = serde_json::from_str(r#"["a","b"]"#).expect("parses");
        assert!(many.contains("b"));
        assert_eq!(many.len(), 2);

        assert!(Audience::Absent.is_empty());
        assert!(!Audience::Absent.contains("a"));
    }

    /// A token with no subject cannot be attached to an account, and saying so
    /// is better than creating one keyed on an empty string.
    #[test]
    fn a_subjectless_token_is_refused() {
        let mut c = claims(NOW);
        c["sub"] = serde_json::json!("");
        let error = IdToken::parse(&token(c))
            .expect("parses")
            .check(None, "my-client-id", None, NOW)
            .expect_err("no subject");
        assert!(format!("{error}").contains("subject"), "{error}");
    }
}
