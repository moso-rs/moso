//! JSON Web Tokens, with the opinion the documentation states plainly:
//! **sessions for browsers, JWT for service-to-service and mobile.**
//!
//! JWT-as-session is a common and expensive mistake — no revocation, no logout,
//! silent expiry — and this module supports it without recommending it.
//!
//! Three structural decisions:
//!
//! 1. **The verifier is constructed with a fixed algorithm.** The header's `alg`
//!    is never read to decide anything, so `alg: none` and RS256/HS256 confusion
//!    are not defended against, they are unrepresentable.
//! 2. **HS256 requires an explicit opt-in and logs a warning.** A symmetric
//!    secret shared between services is how tokens leak: every verifier can
//!    also sign.
//! 3. **Refresh tokens rotate, with reuse detection.** A replayed refresh token
//!    revokes the whole family. This is the OAuth best current practice and
//!    almost nobody implements it.
//!
//! # What "never trusted" means concretely
//!
//! [`Jwt::verify`] reads the header's `alg` exactly once, to *compare* it with
//! the algorithm the verifier was built with, and rejects any difference. It is
//! never used to select a key, a curve or a verification routine. Three attacks
//! fall out of that structure rather than out of a check:
//!
//! | Attack | Why it cannot happen |
//! | --- | --- |
//! | `alg: none` | `"none"` is not a [`JwtAlgorithm`], so it can never equal the configured one |
//! | RS256 → HS256 confusion | An `Rs256` verifier holds an RSA modulus, not a byte string that HMAC could key |
//! | `kid` pointing at a foreign key | Keys are parsed at construction; an unknown `kid` is a rejection, not a fetch |
//!
//! ```no_run
//! use moso_auth::{Claims, Jwt, JwtConfig};
//! use moso_core::config::SecretBytes;
//!
//! # fn f(pkcs8: SecretBytes) -> moso_auth::Result<()> {
//! let jwt: Jwt = Jwt::issuer(JwtConfig::default(), "2026-07", pkcs8)?;
//! let token = jwt.issue(&Claims::new("usr_123"), std::time::Duration::from_secs(900))?;
//! assert_eq!(jwt.verify(&token)?.subject(), "usr_123");
//! # Ok(()) }
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use moso_core::config::{SecretBytes, SecretString};
use serde::{Deserialize, Serialize};

use crate::jwks::{JwkSet, VerifyingKey, b64u, random_bytes, sha256_hex, unb64u};
use crate::{Error, Result};

/// A signing algorithm.
///
/// ```
/// use moso_auth::JwtAlgorithm;
///
/// assert_eq!(JwtAlgorithm::default(), JwtAlgorithm::EdDSA);
/// assert!(!JwtAlgorithm::EdDSA.is_symmetric());
/// assert!(JwtAlgorithm::HS256.is_symmetric());
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum JwtAlgorithm {
    /// Ed25519. The default: small keys, small signatures, no parameter to get
    /// wrong.
    #[default]
    EdDSA,
    /// ECDSA over P-256, for consumers that do not speak Ed25519.
    ES256,
    /// RSA with SHA-256, for consumers that speak nothing else.
    RS256,
    /// HMAC with SHA-256. Symmetric, and therefore a liability across service
    /// boundaries — every verifier can forge. Requires
    /// [`JwtConfig::allow_symmetric`] and logs a warning at boot.
    HS256,
}

impl JwtAlgorithm {
    /// Whether the same key signs and verifies.
    ///
    /// ```
    /// use moso_auth::JwtAlgorithm;
    ///
    /// assert!(JwtAlgorithm::HS256.is_symmetric());
    /// ```
    #[must_use]
    pub const fn is_symmetric(self) -> bool {
        matches!(self, Self::HS256)
    }

    /// The `alg` value written into the header.
    ///
    /// ```
    /// use moso_auth::JwtAlgorithm;
    ///
    /// assert_eq!(JwtAlgorithm::EdDSA.as_str(), "EdDSA");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EdDSA => "EdDSA",
            Self::ES256 => "ES256",
            Self::RS256 => "RS256",
            Self::HS256 => "HS256",
        }
    }
}

/// The registered claims, plus room for the application's own.
///
/// ```
/// use moso_auth::Claims;
///
/// let claims = Claims::new("usr_123").with_audience("api.example.com");
/// assert_eq!(claims.subject(), "usr_123");
/// assert_eq!(claims.aud.as_deref(), Some("api.example.com"));
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Claims {
    /// Who the token is about.
    pub sub: String,
    /// Who issued it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,
    /// Who it is for. Verified — an unaudienced token is a token any service
    /// will accept.
    ///
    /// RFC 7519 § 4.1.3 allows `aud` to be a string *or* an array of them, and
    /// real identity providers emit both. Both are accepted on the way in; when
    /// a token carries several audiences this field holds the first, and the
    /// *check* against [`JwtConfig::audience`] has already been made against
    /// the whole list — see [`Jwt::verify`]. Serialising always writes the
    /// string form.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "audience_in"
    )]
    pub aud: Option<String>,
    /// When it expires, as a Unix timestamp.
    pub exp: i64,
    /// When it was issued.
    pub iat: i64,
    /// Not valid before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nbf: Option<i64>,
    /// A unique identifier, for replay detection and revocation lists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jti: Option<String>,
    /// The application's own claims. Kept as a map so adding one is not a
    /// breaking change to this struct.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Read `aud` in either of the two spellings RFC 7519 allows.
///
/// A multi-audience token keeps its first entry here. Nothing security-relevant
/// rests on that choice: [`verify_with`] compares the *raw* `aud` against the
/// configured audience before this runs, so narrowing it afterwards cannot
/// widen what is accepted.
fn audience_in<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> core::result::Result<Option<String>, D::Error> {
    use serde::de::Error as _;

    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(one)) => Ok(Some(one)),
        Some(serde_json::Value::Array(many)) => {
            Ok(many.into_iter().find_map(|entry| match entry {
                serde_json::Value::String(text) => Some(text),
                _ => None,
            }))
        }
        Some(_) => Err(D::Error::custom(
            "`aud` must be a string or an array of strings (RFC 7519 § 4.1.3)",
        )),
    }
}

impl Claims {
    /// Claims about `subject`, expiring at the issuer's default.
    ///
    /// `iat` is stamped now and `exp` is left at zero: [`Jwt::issue`] overwrites
    /// it from the `ttl` it is given, so that "how long is this token good for"
    /// is a decision made at the one call site that knows.
    ///
    /// ```
    /// use moso_auth::Claims;
    ///
    /// let claims = Claims::new("usr_123");
    /// assert_eq!(claims.exp, 0, "the issuer fills this in");
    /// assert!(claims.iat > 0);
    /// ```
    #[must_use]
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            sub: subject.into(),
            iss: None,
            aud: None,
            exp: 0,
            iat: Utc::now().timestamp(),
            nbf: None,
            jti: None,
            extra: serde_json::Map::new(),
        }
    }

    /// Set the audience.
    ///
    /// ```
    /// use moso_auth::Claims;
    ///
    /// let claims = Claims::new("usr_1").with_audience("api.example.com");
    /// assert_eq!(claims.aud.as_deref(), Some("api.example.com"));
    /// ```
    #[must_use]
    pub fn with_audience(mut self, audience: impl Into<String>) -> Self {
        self.aud = Some(audience.into());
        self
    }

    /// Add an application claim.
    ///
    /// ```
    /// use moso_auth::Claims;
    ///
    /// let claims = Claims::new("usr_1").with_claim("tenant", serde_json::json!("acme"));
    /// assert_eq!(claims.claim::<String>("tenant").unwrap().as_deref(), Some("acme"));
    /// ```
    #[must_use]
    pub fn with_claim(mut self, name: impl Into<String>, value: serde_json::Value) -> Self {
        self.extra.insert(name.into(), value);
        self
    }

    /// Who the token is about.
    ///
    /// ```
    /// use moso_auth::Claims;
    ///
    /// assert_eq!(Claims::new("usr_1").subject(), "usr_1");
    /// ```
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.sub
    }

    /// Read an application claim.
    ///
    /// ```
    /// use moso_auth::Claims;
    ///
    /// let claims = Claims::new("usr_1").with_claim("seats", serde_json::json!(3));
    /// assert_eq!(claims.claim::<u32>("seats").unwrap(), Some(3));
    /// assert_eq!(claims.claim::<u32>("absent").unwrap(), None);
    /// assert!(claims.claim::<String>("seats").is_err());
    /// ```
    ///
    /// # Errors
    ///
    /// A deserialisation failure when the claim is not a `T`.
    pub fn claim<T: serde::de::DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        let Some(value) = self.extra.get(name) else {
            return Ok(None);
        };
        serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|error| {
                Error::Config(format!("claim `{name}` is not the expected type: {error}").into())
            })
    }
}

/// How tokens are issued and verified.
///
/// ```
/// use moso_auth::{JwtAlgorithm, JwtConfig};
/// use std::time::Duration;
///
/// let config = JwtConfig::default();
/// assert_eq!(config.algorithm, JwtAlgorithm::EdDSA);
/// assert_eq!(config.access_ttl, Duration::from_secs(900));
/// assert!(!config.allow_symmetric);
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct JwtConfig {
    /// Which algorithm. Fixed at construction; the header is never trusted.
    pub algorithm: JwtAlgorithm,
    /// Who issues the tokens.
    pub issuer: Option<String>,
    /// Who they are for. Verified on the way in.
    pub audience: Option<String>,
    /// How long an access token lives. Fifteen minutes, because a token with no
    /// revocation path should not outlive the operator's ability to notice.
    pub access_ttl: Duration,
    /// How long a refresh token lives.
    pub refresh_ttl: Duration,
    /// How much clock skew to tolerate on `exp` and `nbf`.
    pub leeway: Duration,
    /// Whether [`JwtAlgorithm::HS256`] may be used at all.
    pub allow_symmetric: bool,
}

impl Default for JwtConfig {
    fn default() -> Self {
        Self {
            algorithm: JwtAlgorithm::EdDSA,
            issuer: None,
            audience: None,
            access_ttl: Duration::from_secs(900),
            refresh_ttl: Duration::from_secs(30 * 24 * 3600),
            leeway: Duration::from_secs(60),
            allow_symmetric: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Signing
// ---------------------------------------------------------------------------

/// A private key, in the shape its algorithm signs with.
enum Signer {
    /// Ed25519.
    Ed25519(Box<ring::signature::Ed25519KeyPair>),
    /// ECDSA over P-256, producing the fixed-width `r ‖ s` JWS spelling.
    P256(Box<ring::signature::EcdsaKeyPair>),
    /// RSA PKCS#1 v1.5 with SHA-256.
    Rsa(Box<ring::signature::RsaKeyPair>),
    /// HMAC-SHA256. Also the verifying key, which is the problem with it.
    Hmac(Box<ring::hmac::Key>),
}

/// The signing half of a [`Jwt`].
struct SigningKey {
    /// Which key, written into every header this key signs.
    kid: String,
    /// The key itself.
    signer: Signer,
}

impl SigningKey {
    /// Parse a private key and derive the matching public key.
    ///
    /// Accepted encodings, by algorithm:
    ///
    /// | Algorithm | Private key |
    /// | --- | --- |
    /// | `EdDSA` | a PKCS#8 v1 or v2 Ed25519 key, or the bare 32-byte seed |
    /// | `ES256` | a PKCS#8 P-256 key |
    /// | `RS256` | a PKCS#8 RSA key, or a PKCS#1 `RSAPrivateKey` |
    /// | `HS256` | the shared secret |
    fn parse(algorithm: JwtAlgorithm, kid: String, key: &[u8]) -> Result<(Self, VerifyingKey)> {
        let rejected = |detail: &str| {
            Error::Config(format!("the {} signing key {detail}", algorithm.as_str()).into())
        };
        match algorithm {
            JwtAlgorithm::EdDSA => {
                use ring::signature::Ed25519KeyPair;

                let pair = Ed25519KeyPair::from_pkcs8(key)
                    .or_else(|_| Ed25519KeyPair::from_pkcs8_maybe_unchecked(key))
                    .or_else(|_| Ed25519KeyPair::from_seed_unchecked(key))
                    .map_err(|error| {
                        rejected(&format!(
                            "is neither a PKCS#8 Ed25519 key nor a 32-byte seed: {error}"
                        ))
                    })?;
                let public = {
                    use ring::signature::KeyPair as _;
                    pair.public_key().as_ref().to_vec()
                };
                Ok((
                    Self {
                        kid,
                        signer: Signer::Ed25519(Box::new(pair)),
                    },
                    VerifyingKey::parse(algorithm, &public)?,
                ))
            }
            JwtAlgorithm::ES256 => {
                use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};

                let rng = ring::rand::SystemRandom::new();
                let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, key, &rng)
                    .map_err(|error| rejected(&format!("is not a PKCS#8 P-256 key: {error}")))?;
                let public = {
                    use ring::signature::KeyPair as _;
                    pair.public_key().as_ref().to_vec()
                };
                Ok((
                    Self {
                        kid,
                        signer: Signer::P256(Box::new(pair)),
                    },
                    VerifyingKey::parse(algorithm, &public)?,
                ))
            }
            JwtAlgorithm::RS256 => {
                use ring::signature::{RsaKeyPair, RsaPublicKeyComponents};

                let pair = RsaKeyPair::from_pkcs8(key)
                    .or_else(|_| RsaKeyPair::from_der(key))
                    .map_err(|error| {
                        rejected(&format!(
                            "is neither a PKCS#8 nor a PKCS#1 RSA key: {error}. ring also \
                             refuses keys below 2048 bits, which is the point"
                        ))
                    })?;
                let components: RsaPublicKeyComponents<Vec<u8>> =
                    RsaPublicKeyComponents::from(pair.public());
                Ok((
                    Self {
                        kid,
                        signer: Signer::Rsa(Box::new(pair)),
                    },
                    VerifyingKey::Rsa {
                        n: components.n,
                        e: components.e,
                    },
                ))
            }
            JwtAlgorithm::HS256 => {
                if key.is_empty() {
                    return Err(rejected("must not be empty"));
                }
                Ok((
                    Self {
                        kid,
                        signer: Signer::Hmac(Box::new(ring::hmac::Key::new(
                            ring::hmac::HMAC_SHA256,
                            key,
                        ))),
                    },
                    VerifyingKey::parse(algorithm, key)?,
                ))
            }
        }
    }

    /// Sign the JWS signing input.
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        let unavailable = |detail: &str| Error::Unavailable {
            component: "token signer",
            detail: detail.to_owned(),
            source: None,
        };
        match &self.signer {
            Signer::Ed25519(pair) => Ok(pair.sign(message).as_ref().to_vec()),
            Signer::P256(pair) => {
                let rng = ring::rand::SystemRandom::new();
                pair.sign(&rng, message)
                    .map(|signature| signature.as_ref().to_vec())
                    .map_err(|_| unavailable("ECDSA signing needs randomness and had none"))
            }
            Signer::Rsa(pair) => {
                let rng = ring::rand::SystemRandom::new();
                let mut signature = vec![0u8; pair.public().modulus_len()];
                pair.sign(
                    &ring::signature::RSA_PKCS1_SHA256,
                    &rng,
                    message,
                    &mut signature,
                )
                .map_err(|_| unavailable("RSA signing needs randomness and had none"))?;
                Ok(signature)
            }
            Signer::Hmac(key) => Ok(ring::hmac::sign(key, message).as_ref().to_vec()),
        }
    }
}

// ---------------------------------------------------------------------------
// Jwt
// ---------------------------------------------------------------------------

/// Issues and verifies tokens.
///
/// Generic over the claims type so an application can use its own, defaulting to
/// [`Claims`].
///
/// ```no_run
/// use moso_auth::{Claims, Jwt};
///
/// # fn f(jwt: &Jwt, claims: &Claims) -> moso_auth::Result<()> {
/// let token = jwt.issue(claims, std::time::Duration::from_secs(300))?;
/// let round_tripped = jwt.verify(&token)?;
/// assert_eq!(round_tripped.subject(), claims.subject());
/// # Ok(()) }
/// ```
pub struct Jwt<C = Claims> {
    /// How tokens are issued and verified.
    config: JwtConfig,
    /// The current signing key, with its `kid`.
    signing_key: Option<SigningKey>,
    /// Every key a token may have been signed with, by `kid`.
    verifying_keys: Vec<(String, VerifyingKey)>,
    /// The claims type, which holds no data.
    marker: core::marker::PhantomData<fn() -> C>,
}

impl<C> Jwt<C>
where
    C: Serialize + serde::de::DeserializeOwned,
{
    /// A verifier with no signing key. For a service that only consumes tokens.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the algorithm is symmetric
    /// and `allow_symmetric` is off, when a key does not parse, when two keys
    /// share a `kid`, or when `keys` is empty — a verifier with no keys accepts
    /// nothing, and finding that out at boot beats finding it out in
    /// production.
    ///
    /// ```
    /// # use moso_auth::{Claims, Jwt, JwtConfig};
    /// # fn f(c: JwtConfig, keys: Vec<(String, Vec<u8>)>) -> moso_auth::Result<Jwt<Claims>> {
    /// Jwt::verifier(c, keys)
    /// # }
    /// # assert!(f(JwtConfig::default(), vec![]).is_err());
    /// ```
    pub fn verifier(config: JwtConfig, keys: Vec<(String, Vec<u8>)>) -> Result<Self> {
        check_algorithm(&config)?;
        if keys.is_empty() {
            return Err(Error::Config(
                "a JWT verifier needs at least one key; pass the issuer's public keys, or use \
                 `RemoteJwks` to fetch them"
                    .into(),
            ));
        }
        let mut verifying_keys = Vec::with_capacity(keys.len());
        for (kid, bytes) in keys {
            if verifying_keys
                .iter()
                .any(|(existing, _): &(String, VerifyingKey)| *existing == kid)
            {
                return Err(Error::Config(
                    format!("two verifying keys share the kid `{kid}`; a kid must name one key")
                        .into(),
                ));
            }
            let key = VerifyingKey::parse(config.algorithm, &bytes)?;
            verifying_keys.push((kid, key));
        }
        Ok(Self {
            config,
            signing_key: None,
            verifying_keys,
            marker: core::marker::PhantomData,
        })
    }

    /// An issuer, which is also a verifier.
    ///
    /// The public half is derived from the private key and registered under the
    /// same `kid`, so a service that issues can always verify what it issued and
    /// [`jwks`](Jwt::jwks) has something to publish.
    ///
    /// # Errors
    ///
    /// As [`verifier`](Jwt::verifier), minus the empty-key case.
    ///
    /// ```no_run
    /// # use moso_core::config::SecretBytes;
    /// # use moso_auth::{Claims, Jwt, JwtConfig};
    /// # fn f(c: JwtConfig, kid: String, k: SecretBytes) -> moso_auth::Result<Jwt<Claims>> {
    /// Jwt::issuer(c, kid, k)
    /// # }
    /// ```
    pub fn issuer(config: JwtConfig, kid: impl Into<String>, key: SecretBytes) -> Result<Self> {
        check_algorithm(&config)?;
        let kid = kid.into();
        if kid.is_empty() {
            return Err(Error::Config(
                "a signing key needs a kid; without one a JWKS cannot name it and rotation \
                 means an outage"
                    .into(),
            ));
        }
        let (signing_key, verifying_key) =
            SigningKey::parse(config.algorithm, kid.clone(), key.expose())?;
        Ok(Self {
            config,
            signing_key: Some(signing_key),
            verifying_keys: vec![(kid, verifying_key)],
            marker: core::marker::PhantomData,
        })
    }

    /// Register an additional public key, for a rotation in progress.
    ///
    /// Rotation is two-phase: publish the new key while still verifying the old
    /// one, then stop. Without this, "rotate a key" means "reject every token in
    /// flight".
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the key does not parse or the `kid` is already taken.
    ///
    /// ```no_run
    /// # use moso_auth::{Claims, Jwt};
    /// # fn f(jwt: Jwt<Claims>, old: Vec<u8>) -> moso_auth::Result<Jwt<Claims>> {
    /// jwt.also_verifying("2026-06", &old)
    /// # }
    /// ```
    pub fn also_verifying(mut self, kid: impl Into<String>, key: &[u8]) -> Result<Self> {
        let kid = kid.into();
        if self
            .verifying_keys
            .iter()
            .any(|(existing, _)| *existing == kid)
        {
            return Err(Error::Config(
                format!("the kid `{kid}` is already registered").into(),
            ));
        }
        let parsed = VerifyingKey::parse(self.config.algorithm, key)?;
        self.verifying_keys.push((kid, parsed));
        Ok(self)
    }

    /// The configuration this instance was built with.
    ///
    /// ```no_run
    /// # use moso_auth::{Claims, Jwt};
    /// # fn f(jwt: &Jwt<Claims>) { let _ = jwt.config().access_ttl; }
    /// ```
    #[must_use]
    pub fn config(&self) -> &JwtConfig {
        &self.config
    }

    /// Issue a token.
    ///
    /// `exp` is always overwritten from `ttl` — that is what passing a `ttl`
    /// means. `iat` is stamped when absent or zero, and `iss` and `aud` are
    /// filled from the configuration when the claims do not already carry them,
    /// so a verifier configured with an audience accepts what this issuer
    /// produces.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when this instance has no
    /// signing key, or when `C` does not serialise to a JSON object — a JWT
    /// payload is an object by definition.
    ///
    /// ```no_run
    /// # use moso_auth::{Claims, Jwt};
    /// # fn f(j: &Jwt, c: &Claims) -> moso_auth::Result<String> {
    /// j.issue(c, std::time::Duration::from_secs(300))
    /// # }
    /// ```
    pub fn issue(&self, claims: &C, ttl: Duration) -> Result<String> {
        let signing_key = self.signing_key.as_ref().ok_or_else(|| {
            Error::Config(
                "this Jwt was built with `Jwt::verifier` and has no signing key; build it with \
                 `Jwt::issuer` to issue tokens"
                    .into(),
            )
        })?;

        let mut payload = match serde_json::to_value(claims) {
            Ok(serde_json::Value::Object(map)) => map,
            Ok(_) => {
                return Err(Error::Config(
                    "a JWT payload must serialise to a JSON object".into(),
                ));
            }
            Err(error) => {
                return Err(Error::Config(
                    format!("the claims did not serialise: {error}").into(),
                ));
            }
        };

        let now = Utc::now().timestamp();
        let ttl_seconds = i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        payload.insert(
            "exp".to_owned(),
            serde_json::Value::from(now.saturating_add(ttl_seconds)),
        );
        let missing_iat = payload
            .get("iat")
            .is_none_or(|value| value.as_i64().unwrap_or_default() == 0);
        if missing_iat {
            payload.insert("iat".to_owned(), serde_json::Value::from(now));
        }
        if let Some(issuer) = &self.config.issuer
            && !payload.contains_key("iss")
        {
            payload.insert("iss".to_owned(), serde_json::Value::from(issuer.clone()));
        }
        if let Some(audience) = &self.config.audience
            && !payload.contains_key("aud")
        {
            payload.insert("aud".to_owned(), serde_json::Value::from(audience.clone()));
        }

        let mut header = serde_json::Map::new();
        header.insert(
            "alg".to_owned(),
            serde_json::Value::from(self.config.algorithm.as_str()),
        );
        header.insert("typ".to_owned(), serde_json::Value::from("JWT"));
        header.insert(
            "kid".to_owned(),
            serde_json::Value::from(signing_key.kid.clone()),
        );

        let encode = |value: &serde_json::Map<String, serde_json::Value>| -> Result<String> {
            serde_json::to_vec(value)
                .map(|bytes| b64u(&bytes))
                .map_err(|error| Error::Config(format!("could not encode a JWT: {error}").into()))
        };
        let mut token = encode(&header)?;
        token.push('.');
        token.push_str(&encode(&payload)?);
        let signature = signing_key.sign(token.as_bytes())?;
        token.push('.');
        token.push_str(&b64u(&signature));
        Ok(token)
    }

    /// Verify a token and return its claims.
    ///
    /// The header's `alg` is compared against the configured one and the token
    /// is rejected on any difference — it is never used to *select* a verifier.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidCredentials`] for a
    /// bad signature, an unknown `kid`, a wrong `alg`, a wrong audience or
    /// issuer; [`Error::Expired`] past `exp` plus the
    /// leeway.
    ///
    /// ```no_run
    /// # use moso_auth::{Claims, Jwt};
    /// # fn f(j: &Jwt, t: &str) -> moso_auth::Result<Claims> { j.verify(t) }
    /// ```
    pub fn verify(&self, token: &str) -> Result<C> {
        verify_with(&self.config, &self.verifying_keys, token)
    }

    /// The public keys, as a JWKS document for `/.well-known/jwks.json`.
    ///
    /// Empty for a symmetric configuration, which is one more reason not to use
    /// one: there is nothing to publish, so every consumer needs the secret.
    ///
    /// ```no_run
    /// # use moso_auth::Jwt;
    /// # fn f(j: &Jwt) { let _: serde_json::Value = j.jwks(); }
    /// ```
    #[must_use]
    pub fn jwks(&self) -> serde_json::Value {
        let keys = self
            .verifying_keys
            .iter()
            .filter_map(|(kid, key)| key.to_jwk(kid, self.config.algorithm))
            .collect();
        serde_json::to_value(JwkSet::new(keys)).unwrap_or_else(|_| {
            // `JwkSet` is a struct of `String`s and `Option<String>`s; the only
            // way `to_value` fails is a map with non-string keys, which this
            // type cannot express. The fallback keeps the signature infallible
            // rather than resting on that reasoning.
            serde_json::json!({ "keys": [] })
        })
    }
}

/// Refuse a symmetric algorithm that was not explicitly allowed, and warn about
/// one that was.
fn check_algorithm(config: &JwtConfig) -> Result<()> {
    if !config.algorithm.is_symmetric() {
        return Ok(());
    }
    if !config.allow_symmetric {
        return Err(Error::Config(
            "HS256 is symmetric: every service that can verify a token can also mint one. Set \
             `JwtConfig::allow_symmetric = true` if that is genuinely what you want, or switch \
             to the default EdDSA"
                .into(),
        ));
    }
    tracing::warn!(
        target: "moso_auth::jwt",
        algorithm = "HS256",
        "a symmetric JWT algorithm is enabled: every holder of the verification key can also \
         forge tokens. Prefer EdDSA unless the token never leaves this process"
    );
    Ok(())
}

impl<C> core::fmt::Debug for Jwt<C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Jwt")
            .field("algorithm", &self.config.algorithm)
            .field("keys", &self.verifying_keys.len())
            .field("can_issue", &self.signing_key.is_some())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// The header members this crate reads.
#[derive(Deserialize)]
struct Header {
    /// The algorithm, read only to be compared with the configured one.
    alg: String,
    /// Which key signed it.
    #[serde(default)]
    kid: Option<String>,
    /// Extensions the issuer says must be understood. Moso understands none, so
    /// any entry here is a rejection: that is what `crit` is for.
    #[serde(default)]
    crit: Option<Vec<String>>,
}

/// The registered claims, all optional, for the checks that happen before `C`
/// is even deserialised.
#[derive(Deserialize)]
struct Registered {
    /// Expiry. Required — see [`verify_with`].
    #[serde(default)]
    exp: Option<i64>,
    /// Not before.
    #[serde(default)]
    nbf: Option<i64>,
    /// Issued at.
    #[serde(default)]
    iat: Option<i64>,
    /// Issuer.
    #[serde(default)]
    iss: Option<String>,
    /// Audience: a string or an array of them, per RFC 7519 § 4.1.3.
    #[serde(default)]
    aud: Option<serde_json::Value>,
}

/// Split, check, verify, then check the time claims — in that order.
///
/// The order matters: nothing that costs a database lookup or an allocation of
/// unbounded size happens before the signature is verified.
fn verify_with<C: serde::de::DeserializeOwned>(
    config: &JwtConfig,
    keys: &[(String, VerifyingKey)],
    token: &str,
) -> Result<C> {
    let payload_bytes = split_and_check_signature(config, keys, token)?;

    let registered: Registered =
        serde_json::from_slice(&payload_bytes).map_err(|_| Error::InvalidCredentials)?;

    let now = Utc::now().timestamp();
    let leeway = i64::try_from(config.leeway.as_secs()).unwrap_or(i64::MAX);

    // `exp` is required. RFC 7519 makes it optional; a token that never expires
    // and cannot be revoked is not a credential this crate is willing to accept.
    let exp = registered.exp.ok_or(Error::InvalidCredentials)?;
    if now.saturating_sub(leeway) >= exp {
        return Err(Error::Expired { kind: "token" });
    }
    if let Some(nbf) = registered.nbf
        && now.saturating_add(leeway) < nbf
    {
        return Err(Error::InvalidCredentials);
    }
    if let Some(iat) = registered.iat
        && iat > now.saturating_add(leeway)
    {
        // Issued in the future: either the issuer's clock is wrong by more than
        // the leeway, or the token is forged.
        return Err(Error::InvalidCredentials);
    }
    if let Some(expected) = &config.issuer
        && registered.iss.as_deref() != Some(expected.as_str())
    {
        return Err(Error::InvalidCredentials);
    }
    if let Some(expected) = &config.audience
        && !audience_contains(registered.aud.as_ref(), expected)
    {
        return Err(Error::InvalidCredentials);
    }

    serde_json::from_slice(&payload_bytes).map_err(|_| Error::InvalidCredentials)
}

/// Split a compact JWS, check the algorithm, find the key and verify.
///
/// Returns the raw payload bytes, once and only once the signature over them
/// has been verified.
fn split_and_check_signature(
    config: &JwtConfig,
    keys: &[(String, VerifyingKey)],
    token: &str,
) -> Result<Vec<u8>> {
    // Exactly three parts. Two is the unsecured JWT of RFC 7519 § 6 — the
    // `alg: none` shape — and five is a JWE, which this crate does not decrypt.
    let mut parts = token.split('.');
    let (Some(header_part), Some(payload_part), Some(signature_part), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Error::InvalidCredentials);
    };

    let header: Header =
        serde_json::from_slice(&unb64u(header_part)?).map_err(|_| Error::InvalidCredentials)?;

    // The one and only reading of `alg`, and it is a comparison. `"none"` is not
    // a `JwtAlgorithm`, so it can never equal `config.algorithm.as_str()`.
    if header.alg != config.algorithm.as_str() {
        return Err(Error::InvalidCredentials);
    }
    if header.crit.as_ref().is_some_and(|crit| !crit.is_empty()) {
        return Err(Error::InvalidCredentials);
    }

    let signature = unb64u(signature_part)?;
    let signing_input_len = header_part.len() + 1 + payload_part.len();
    let signing_input = &token.as_bytes()[..signing_input_len];

    let verified = match &header.kid {
        // A `kid` names one key. Falling back to "try the others" when it does
        // not match would make the field decorative.
        Some(kid) => keys
            .iter()
            .find(|(known, _)| known == kid)
            .is_some_and(|(_, key)| key.verify(config.algorithm, signing_input, &signature)),
        None => keys
            .iter()
            .any(|(_, key)| key.verify(config.algorithm, signing_input, &signature)),
    };
    if !verified {
        return Err(Error::InvalidCredentials);
    }

    unb64u(payload_part)
}

/// Whether the token's `aud` names `expected`, in either of the two spellings
/// RFC 7519 allows.
fn audience_contains(aud: Option<&serde_json::Value>, expected: &str) -> bool {
    match aud {
        Some(serde_json::Value::String(one)) => one == expected,
        Some(serde_json::Value::Array(many)) => many
            .iter()
            .any(|value| value.as_str().is_some_and(|text| text == expected)),
        _ => false,
    }
}

/// The well-known path a JWKS document is published at.
///
/// ```
/// assert_eq!(moso_auth::jwt::JWKS_PATH, "/.well-known/jwks.json");
/// ```
pub const JWKS_PATH: &str = "/.well-known/jwks.json";

// ---------------------------------------------------------------------------
// RemoteJwks
// ---------------------------------------------------------------------------

/// How long a fetched key set is used before being refetched.
const DEFAULT_JWKS_TTL: Duration = Duration::from_secs(3600);

/// The shortest interval between fetches.
const DEFAULT_MIN_REFETCH: Duration = Duration::from_secs(300);

/// How long a single fetch may take.
const JWKS_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// What is currently known about the remote key set.
#[derive(Default)]
struct JwksCache {
    /// The keys, shared rather than cloned per request.
    keys: Arc<Vec<(String, VerifyingKey)>>,
    /// When they were last successfully fetched.
    fetched_at: Option<Instant>,
    /// When a fetch was last *attempted*, successfully or not. This is what the
    /// refetch throttle reads: throttling only successes would let a failing
    /// endpoint be hammered.
    attempted_at: Option<Instant>,
}

/// Verifies tokens from another service, fetching and caching its JWKS.
///
/// ```no_run
/// use moso_auth::{Claims, RemoteJwks};
///
/// # async fn f(j: &RemoteJwks, token: &str) -> moso_auth::Result<Claims> {
/// j.verify(token).await
/// # }
/// ```
#[derive(Debug)]
pub struct RemoteJwks {
    /// Where the document is.
    url: String,
    /// How long to cache it.
    cache_ttl: Duration,
    /// The shortest interval between refetches when an unknown `kid` arrives.
    ///
    /// Without it, a token with a made-up `kid` is a request to the issuer, and
    /// a stream of them is a denial of service against somebody else.
    min_refetch: Duration,
    /// How tokens are verified once the keys are in hand.
    config: JwtConfig,
    /// What is known right now.
    cache: tokio::sync::RwLock<JwksCache>,
    /// Held across a fetch so that a hundred concurrent misses make one request.
    fetching: tokio::sync::Mutex<()>,
}

impl core::fmt::Debug for JwksCache {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("JwksCache")
            .field("keys", &self.keys.len())
            .field("fetched", &self.fetched_at.is_some())
            .finish()
    }
}

impl RemoteJwks {
    /// A verifier over the JWKS at `url`.
    ///
    /// Caches for an hour and refetches at most every five minutes.
    ///
    /// ```
    /// use moso_auth::{JwtConfig, RemoteJwks};
    ///
    /// let jwks = RemoteJwks::new("https://idp.example.com/jwks", JwtConfig::default());
    /// assert_eq!(jwks.url(), "https://idp.example.com/jwks");
    /// ```
    #[must_use]
    pub fn new(url: impl Into<String>, config: JwtConfig) -> Self {
        Self {
            url: url.into(),
            cache_ttl: DEFAULT_JWKS_TTL,
            min_refetch: DEFAULT_MIN_REFETCH,
            config,
            cache: tokio::sync::RwLock::new(JwksCache::default()),
            fetching: tokio::sync::Mutex::new(()),
        }
    }

    /// How long a fetched key set is used before being refetched.
    ///
    /// ```
    /// use moso_auth::{JwtConfig, RemoteJwks};
    ///
    /// let jwks = RemoteJwks::new("https://idp.example.com/jwks", JwtConfig::default())
    ///     .cache_for(std::time::Duration::from_secs(600));
    /// assert_eq!(jwks.cache_ttl(), std::time::Duration::from_secs(600));
    /// ```
    #[must_use]
    pub fn cache_for(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    /// The shortest interval between fetches.
    ///
    /// ```
    /// use moso_auth::{JwtConfig, RemoteJwks};
    ///
    /// let jwks = RemoteJwks::new("https://idp.example.com/jwks", JwtConfig::default())
    ///     .refetch_at_most_every(std::time::Duration::from_secs(60));
    /// assert_eq!(jwks.min_refetch(), std::time::Duration::from_secs(60));
    /// ```
    #[must_use]
    pub fn refetch_at_most_every(mut self, interval: Duration) -> Self {
        self.min_refetch = interval;
        self
    }

    /// Where the document is.
    ///
    /// ```
    /// use moso_auth::{JwtConfig, RemoteJwks};
    ///
    /// assert_eq!(
    ///     RemoteJwks::new("https://idp.example.com/jwks", JwtConfig::default()).url(),
    ///     "https://idp.example.com/jwks",
    /// );
    /// ```
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// How long a fetched key set is used.
    ///
    /// ```
    /// use moso_auth::{JwtConfig, RemoteJwks};
    ///
    /// let jwks = RemoteJwks::new("https://idp.example.com/jwks", JwtConfig::default());
    /// assert_eq!(jwks.cache_ttl(), std::time::Duration::from_secs(3600));
    /// ```
    #[must_use]
    pub fn cache_ttl(&self) -> Duration {
        self.cache_ttl
    }

    /// The shortest interval between fetches.
    ///
    /// ```
    /// use moso_auth::{JwtConfig, RemoteJwks};
    ///
    /// let jwks = RemoteJwks::new("https://idp.example.com/jwks", JwtConfig::default());
    /// assert_eq!(jwks.min_refetch(), std::time::Duration::from_secs(300));
    /// ```
    #[must_use]
    pub fn min_refetch(&self) -> Duration {
        self.min_refetch
    }

    /// Verify a token, fetching keys if needed.
    ///
    /// # Errors
    ///
    /// As [`Jwt::verify`], plus
    /// [`Error::Unavailable`] when the JWKS cannot be fetched and nothing is cached.
    ///
    /// ```no_run
    /// # use moso_auth::{Claims, RemoteJwks};
    /// # async fn f(j: &RemoteJwks, t: &str) -> moso_auth::Result<Claims> { j.verify(t).await }
    /// ```
    pub async fn verify(&self, token: &str) -> Result<Claims> {
        self.verify_as::<Claims>(token).await
    }

    /// Verify a token into the application's own claims type.
    ///
    /// # Errors
    ///
    /// As [`verify`](RemoteJwks::verify).
    ///
    /// ```no_run
    /// # use moso_auth::{Claims, RemoteJwks};
    /// # async fn f(j: &RemoteJwks, t: &str) -> moso_auth::Result<Claims> {
    /// j.verify_as::<Claims>(t).await
    /// # }
    /// ```
    pub async fn verify_as<C: serde::de::DeserializeOwned>(&self, token: &str) -> Result<C> {
        let kid = kid_of(token);
        let keys = self.keys_for(kid.as_deref()).await?;
        verify_with(&self.config, &keys, token)
    }

    /// Fetch the document now, for a startup warm-up.
    ///
    /// Ignores the refetch throttle: this is an explicit request, not a reaction
    /// to a token.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::RemoteJwks;
    /// # async fn f(j: &RemoteJwks) -> moso_auth::Result<()> { j.refresh().await }
    /// ```
    pub async fn refresh(&self) -> Result<()> {
        let _guard = self.fetching.lock().await;
        self.fetch_into_cache().await.map(|_| ())
    }

    /// How many keys are cached right now, for a health check or a test.
    ///
    /// ```no_run
    /// # use moso_auth::RemoteJwks;
    /// # async fn f(j: &RemoteJwks) { let _: usize = j.cached_key_count().await; }
    /// ```
    pub async fn cached_key_count(&self) -> usize {
        self.cache.read().await.keys.len()
    }

    /// The keys to verify with, fetching if the cache cannot answer.
    async fn keys_for(&self, kid: Option<&str>) -> Result<Arc<Vec<(String, VerifyingKey)>>> {
        if let Some(keys) = self.cached_if_usable(kid).await {
            return Ok(keys);
        }

        let _guard = self.fetching.lock().await;
        // Another task may have fetched while this one waited for the lock.
        if let Some(keys) = self.cached_if_usable(kid).await {
            return Ok(keys);
        }

        let throttled = {
            let cache = self.cache.read().await;
            cache
                .attempted_at
                .is_some_and(|at| at.elapsed() < self.min_refetch)
        };
        if throttled {
            let cache = self.cache.read().await;
            if cache.fetched_at.is_some() {
                // Serve what is cached even though it is stale or missing the
                // kid: the alternative is to turn a stream of made-up kids into
                // a stream of requests at the identity provider.
                tracing::debug!(
                    target: "moso_auth::jwt",
                    url = %self.url,
                    kid = kid.unwrap_or("<none>"),
                    "jwks refetch throttled; using the cached key set"
                );
                return Ok(Arc::clone(&cache.keys));
            }
            return Err(Error::Unavailable {
                component: "jwks endpoint",
                detail: format!(
                    "{} could not be fetched and nothing is cached; the next attempt is allowed \
                     in at most {:?}",
                    self.url, self.min_refetch
                ),
                source: None,
            });
        }

        match self.fetch_into_cache().await {
            Ok(keys) => Ok(keys),
            Err(error) => {
                let cache = self.cache.read().await;
                if cache.fetched_at.is_some() {
                    // A stale key set beats an outage: the keys were valid a
                    // moment ago and the endpoint being down is not evidence
                    // that they are not.
                    tracing::warn!(
                        target: "moso_auth::jwt",
                        url = %self.url,
                        %error,
                        "jwks refetch failed; falling back to the cached key set"
                    );
                    return Ok(Arc::clone(&cache.keys));
                }
                Err(error)
            }
        }
    }

    /// The cached keys, when they are fresh and contain the wanted `kid`.
    async fn cached_if_usable(
        &self,
        kid: Option<&str>,
    ) -> Option<Arc<Vec<(String, VerifyingKey)>>> {
        let cache = self.cache.read().await;
        let fetched_at = cache.fetched_at?;
        if fetched_at.elapsed() >= self.cache_ttl {
            return None;
        }
        let has_kid = kid.is_none_or(|wanted| cache.keys.iter().any(|(known, _)| known == wanted));
        has_kid.then(|| Arc::clone(&cache.keys))
    }

    /// Fetch, parse and store. The caller holds `fetching`.
    async fn fetch_into_cache(&self) -> Result<Arc<Vec<(String, VerifyingKey)>>> {
        {
            let mut cache = self.cache.write().await;
            cache.attempted_at = Some(Instant::now());
        }
        let document = crate::jwks::fetch(&self.url, JWKS_FETCH_TIMEOUT).await?;

        let mut keys = Vec::with_capacity(document.keys.len());
        for jwk in &document.keys {
            let Some(kid) = jwk.kid.clone() else {
                // A key with no `kid` cannot be named by a token header. Moso
                // keeps it under the empty name, which `kid_of` never produces,
                // so it participates only in the no-kid path.
                match VerifyingKey::from_jwk(jwk, self.config.algorithm) {
                    Ok(key) => keys.push((String::new(), key)),
                    Err(error) => tracing::debug!(
                        target: "moso_auth::jwt",
                        url = %self.url,
                        %error,
                        "skipping an unusable key with no kid"
                    ),
                }
                continue;
            };
            if !jwk.is_signing_key() {
                continue;
            }
            match VerifyingKey::from_jwk(jwk, self.config.algorithm) {
                Ok(key) => keys.push((kid, key)),
                // A JWKS routinely carries keys for algorithms this verifier
                // does not use. Skipping them is correct; failing the whole
                // fetch would make one foreign key an outage.
                Err(error) => tracing::debug!(
                    target: "moso_auth::jwt",
                    url = %self.url,
                    kid = %kid,
                    %error,
                    "skipping a key this verifier cannot use"
                ),
            }
        }

        if keys.is_empty() {
            return Err(Error::Unavailable {
                component: "jwks endpoint",
                detail: format!(
                    "{} published {} key(s), none usable for {}",
                    self.url,
                    document.keys.len(),
                    self.config.algorithm.as_str()
                ),
                source: None,
            });
        }

        let keys = Arc::new(keys);
        let mut cache = self.cache.write().await;
        cache.keys = Arc::clone(&keys);
        cache.fetched_at = Some(Instant::now());
        Ok(keys)
    }
}

/// The `kid` in a token's header, without verifying anything.
///
/// Used only to decide *which cached key to look for*. A forged header gets a
/// cache miss and then a signature failure; it never selects an algorithm.
fn kid_of(token: &str) -> Option<String> {
    let header_part = token.split('.').next()?;
    let header: Header = serde_json::from_slice(&unb64u(header_part).ok()?).ok()?;
    header.kid.filter(|kid| !kid.is_empty())
}

// ---------------------------------------------------------------------------
// Refresh tokens
// ---------------------------------------------------------------------------

/// How many bytes of entropy a refresh token carries.
const REFRESH_TOKEN_BYTES: usize = 32;

/// A refresh token, and the family it belongs to.
///
/// Rotation with reuse detection: every refresh issues a new token and
/// invalidates the old one. A *replayed* token — one already exchanged — means
/// somebody has a stolen copy, so the whole family is revoked and an audit
/// event is emitted. The legitimate user is logged out, which is the correct
/// outcome, because the alternative is an attacker with a token that never
/// expires.
///
/// ```
/// use moso_auth::RefreshToken;
///
/// let token = RefreshToken::mint("usr_1", "fam_1", std::time::Duration::from_secs(60)).unwrap();
/// assert_eq!(token.family(), "fam_1");
/// assert_eq!(RefreshToken::hash_of(token.expose()), token.hash());
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RefreshToken {
    /// The opaque token value. Only a SHA-256 of it is stored.
    ///
    /// Serialised through [`RefreshToken::expose`] rather than being skipped:
    /// unlike a configuration secret, this value's entire purpose is to reach
    /// the client that will present it next. A [`RefreshStore`] must still
    /// persist [`hash`](RefreshToken::hash) and never this.
    #[serde(serialize_with = "serialize_exposed")]
    pub token: SecretString,
    /// Which family it belongs to. Revoking the family revokes every
    /// descendant.
    pub family: String,
    /// Which subject it is for.
    pub subject: String,
    /// When it stops working.
    pub expires_at: DateTime<Utc>,
}

/// Write a [`SecretString`] as its value.
///
/// `SecretString`'s own `Serialize` always fails, which is right for a
/// configuration secret and wrong for a bearer token that has to be delivered.
fn serialize_exposed<S: serde::Serializer>(
    secret: &SecretString,
    serializer: S,
) -> core::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(secret.expose())
}

impl RefreshToken {
    /// Mint a token in `family` for `subject`, valid for `ttl`.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the system random generator fails.
    ///
    /// ```
    /// use moso_auth::RefreshToken;
    ///
    /// let token = RefreshToken::mint("usr_1", "fam_1", std::time::Duration::from_secs(60))?;
    /// assert_eq!(token.subject, "usr_1");
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn mint(
        subject: impl Into<String>,
        family: impl Into<String>,
        ttl: Duration,
    ) -> Result<Self> {
        let bytes = random_bytes(REFRESH_TOKEN_BYTES)?;
        let seconds = i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        // A TTL beyond chrono's range is a configuration mistake, not a reason
        // to fail a login: clamp rather than panic on the addition.
        let delta = chrono::TimeDelta::try_seconds(seconds).unwrap_or(chrono::TimeDelta::MAX);
        let expires_at = Utc::now()
            .checked_add_signed(delta)
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        Ok(Self {
            token: SecretString::new(b64u(&bytes)),
            family: family.into(),
            subject: subject.into(),
            expires_at,
        })
    }

    /// A fresh family identifier.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the system random generator fails.
    ///
    /// ```
    /// use moso_auth::RefreshToken;
    ///
    /// let family = RefreshToken::new_family()?;
    /// assert_ne!(family, RefreshToken::new_family()?);
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    pub fn new_family() -> Result<String> {
        Ok(b64u(&random_bytes(16)?))
    }

    /// Which family this belongs to.
    ///
    /// ```
    /// # use moso_auth::RefreshToken;
    /// let token = RefreshToken::mint("u", "f", std::time::Duration::from_secs(1))?;
    /// assert_eq!(token.family(), "f");
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn family(&self) -> &str {
        &self.family
    }

    /// The token value, to hand to the client. The one place it is readable.
    ///
    /// ```
    /// # use moso_auth::RefreshToken;
    /// let token = RefreshToken::mint("u", "f", std::time::Duration::from_secs(1))?;
    /// assert!(!token.expose().is_empty());
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn expose(&self) -> &str {
        self.token.expose()
    }

    /// What a store persists: the hash, never the token.
    ///
    /// ```
    /// # use moso_auth::RefreshToken;
    /// let token = RefreshToken::mint("u", "f", std::time::Duration::from_secs(1))?;
    /// assert_eq!(token.hash().len(), 64, "hex SHA-256");
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn hash(&self) -> String {
        Self::hash_of(self.expose())
    }

    /// The hash of a presented token, for the lookup.
    ///
    /// ```
    /// use moso_auth::RefreshToken;
    ///
    /// assert_eq!(RefreshToken::hash_of("abc").len(), 64);
    /// ```
    #[must_use]
    pub fn hash_of(token: &str) -> String {
        sha256_hex(token.as_bytes())
    }

    /// Whether the token is still within its lifetime at `now`.
    ///
    /// ```
    /// # use moso_auth::RefreshToken;
    /// let token = RefreshToken::mint("u", "f", std::time::Duration::from_secs(60))?;
    /// assert!(token.is_live(chrono::Utc::now()));
    /// # Ok::<(), moso_auth::Error>(())
    /// ```
    #[must_use]
    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        now < self.expires_at
    }
}

/// What exchanging a refresh token produced.
///
/// ```no_run
/// use moso_auth::RefreshOutcome;
///
/// # fn f(o: &RefreshOutcome) {
/// let _ = matches!(o, RefreshOutcome::Rotated { .. });
/// # }
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum RefreshOutcome {
    /// A new access token and a new refresh token. The old one is dead.
    Rotated {
        /// The new access token.
        access: String,
        /// The new refresh token.
        refresh: RefreshToken,
    },
    /// The token was already exchanged. The family has been revoked and an
    /// audit event emitted; every token descended from it is now dead.
    ReuseDetected {
        /// Which family was revoked, for the audit trail.
        family: String,
    },
    /// The token is unknown or expired. No family to revoke.
    Invalid,
}

/// Where refresh-token families live.
///
/// Dyn-compatible: a table in most applications, a KV namespace in some.
///
/// ```no_run
/// use moso_auth::{RefreshOutcome, RefreshStore};
///
/// async fn exchange(store: &dyn RefreshStore, token: &str)
///     -> moso_auth::Result<RefreshOutcome>
/// {
///     store.exchange(token).await
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot store refresh tokens",
    label = "not a refresh store",
    note = "a refresh store implements `issue`, `exchange`, `revoke_family` and `revoke_subject`",
    note = "help: `exchange` must be atomic — the reuse detection is a compare-and-set, and a \
            read-then-write races exactly when it matters",
    note = "help: `moso_auth::store::TableRefreshStore` is the shipped table-backed one; \
            pass `moso_auth::store::descriptors()` to `moso db make-migration` for its table"
)]
pub trait RefreshStore: Send + Sync + 'static {
    /// Issue the first token of a new family.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn issue<'a>(
        &'a self,
        subject: &'a str,
        ttl: Duration,
    ) -> moso_core::BoxFuture<'a, Result<RefreshToken>>;

    /// Exchange a token for the next one, detecting reuse.
    ///
    /// Must be atomic. The whole guarantee is that two concurrent exchanges of
    /// the same token produce one rotation and one
    /// [`RefreshOutcome::ReuseDetected`], and a read-then-write races exactly
    /// when an attacker is racing the legitimate client.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn exchange<'a>(&'a self, token: &'a str) -> moso_core::BoxFuture<'a, Result<RefreshOutcome>>;

    /// Revoke a family and everything descended from it.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn revoke_family<'a>(&'a self, family: &'a str) -> moso_core::BoxFuture<'a, Result<u64>>;

    /// Revoke every family for a subject. What "log out everywhere" calls for
    /// token-authenticated clients.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn revoke_subject<'a>(&'a self, subject: &'a str) -> moso_core::BoxFuture<'a, Result<u64>>;
}

/// One stored refresh token.
#[derive(Clone, Debug)]
struct RefreshRow {
    /// Which family.
    family: String,
    /// Whose token.
    subject: String,
    /// When it stops working.
    expires_at: DateTime<Utc>,
    /// Whether it has already been exchanged. The single bit reuse detection
    /// rests on.
    used: bool,
}

/// What a [`MemoryRefreshStore`] holds, behind one lock.
#[derive(Default)]
struct RefreshState {
    /// Rows by token hash.
    rows: HashMap<String, RefreshRow>,
    /// Families that have been burned.
    revoked: std::collections::HashSet<String>,
}

/// A refresh-token store in process memory, with real reuse detection.
///
/// The atomicity [`RefreshStore::exchange`] demands is a mutex here. That is
/// enough for a single process and is *not* enough for two: a deployment with
/// more than one instance needs the table-backed store, where the compare-and-set
/// is an `UPDATE … WHERE used = false` and the row count is the answer.
///
/// It is a complete implementation, not a test double: `moso auth`'s own routes
/// use it when no other store is configured, and the reuse-detection behaviour
/// below is the behaviour a database store must reproduce.
///
/// ```no_run
/// use std::sync::Arc;
///
/// use moso_auth::{Claims, Jwt, MemoryRefreshStore, RefreshStore};
///
/// # async fn f(jwt: Arc<Jwt<Claims>>) -> moso_auth::Result<()> {
/// let store = MemoryRefreshStore::new(jwt);
/// let first = store.issue("usr_1", std::time::Duration::from_secs(3600)).await?;
/// let _rotated = store.exchange(first.expose()).await?;
/// # Ok(()) }
/// ```
pub struct MemoryRefreshStore {
    /// Mints the access token half of a rotation.
    jwt: Arc<Jwt<Claims>>,
    /// Every token and family this store knows.
    state: std::sync::Mutex<RefreshState>,
}

impl core::fmt::Debug for MemoryRefreshStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let live = self
            .state
            .lock()
            .map(|state| state.rows.len())
            .unwrap_or_default();
        f.debug_struct("MemoryRefreshStore")
            .field("tokens", &live)
            .finish_non_exhaustive()
    }
}

impl MemoryRefreshStore {
    /// A store that mints its access tokens with `jwt`.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_auth::{Claims, Jwt, MemoryRefreshStore};
    /// # fn f(jwt: Arc<Jwt<Claims>>) { let _ = MemoryRefreshStore::new(jwt); }
    /// ```
    #[must_use]
    pub fn new(jwt: Arc<Jwt<Claims>>) -> Self {
        Self {
            jwt,
            state: std::sync::Mutex::new(RefreshState::default()),
        }
    }

    /// How many tokens are on record, including used and revoked ones.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_auth::{Claims, Jwt, MemoryRefreshStore};
    /// # fn f(jwt: Arc<Jwt<Claims>>) { let _: usize = MemoryRefreshStore::new(jwt).len(); }
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.rows.len())
            .unwrap_or_default()
    }

    /// Whether nothing has been issued.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_auth::{Claims, Jwt, MemoryRefreshStore};
    /// # fn f(jwt: Arc<Jwt<Claims>>) { assert!(MemoryRefreshStore::new(jwt).is_empty()); }
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take the lock, mapping poisoning onto an outage rather than a panic.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, RefreshState>> {
        self.state.lock().map_err(|_| Error::Unavailable {
            component: "refresh store",
            detail: "the in-memory store's lock was poisoned by a panic".to_owned(),
            source: None,
        })
    }

    /// Record a freshly minted token.
    fn record(state: &mut RefreshState, token: &RefreshToken) {
        state.rows.insert(
            token.hash(),
            RefreshRow {
                family: token.family.clone(),
                subject: token.subject.clone(),
                expires_at: token.expires_at,
                used: false,
            },
        );
    }
}

impl RefreshStore for MemoryRefreshStore {
    fn issue<'a>(
        &'a self,
        subject: &'a str,
        ttl: Duration,
    ) -> moso_core::BoxFuture<'a, Result<RefreshToken>> {
        Box::pin(async move {
            let family = RefreshToken::new_family()?;
            let token = RefreshToken::mint(subject, family, ttl)?;
            let mut state = self.lock()?;
            Self::record(&mut state, &token);
            Ok(token)
        })
    }

    fn exchange<'a>(&'a self, token: &'a str) -> moso_core::BoxFuture<'a, Result<RefreshOutcome>> {
        Box::pin(async move {
            let hash = RefreshToken::hash_of(token);
            let now = Utc::now();

            // Everything that decides the outcome happens under one lock, so two
            // concurrent exchanges of the same token cannot both rotate.
            let decision = {
                let mut state = self.lock()?;
                let Some(row) = state.rows.get(&hash).cloned() else {
                    return Ok(RefreshOutcome::Invalid);
                };
                if state.revoked.contains(&row.family) {
                    return Ok(RefreshOutcome::Invalid);
                }
                if row.used {
                    // A token presented twice means a copy exists. Burn the
                    // family: the legitimate client is logged out, which is
                    // strictly better than an attacker holding a token that
                    // rotates forever.
                    state.revoked.insert(row.family.clone());
                    let burned = state
                        .rows
                        .iter()
                        .filter(|(_, other)| other.family == row.family)
                        .count();
                    state.rows.retain(|_, other| other.family != row.family);
                    tracing::warn!(
                        target: "moso_auth::audit",
                        event = "refresh_token_reuse",
                        family = %row.family,
                        subject = %row.subject,
                        revoked = burned,
                        "a refresh token was presented twice; the whole family has been revoked"
                    );
                    return Ok(RefreshOutcome::ReuseDetected { family: row.family });
                }
                if now >= row.expires_at {
                    state.rows.remove(&hash);
                    return Ok(RefreshOutcome::Invalid);
                }
                if let Some(existing) = state.rows.get_mut(&hash) {
                    existing.used = true;
                }
                row
            };

            let ttl = self.jwt.config().refresh_ttl;
            let next = RefreshToken::mint(&decision.subject, &decision.family, ttl)?;
            {
                let mut state = self.lock()?;
                Self::record(&mut state, &next);
            }
            let access = self.jwt.issue(
                &Claims::new(&decision.subject),
                self.jwt.config().access_ttl,
            )?;
            Ok(RefreshOutcome::Rotated {
                access,
                refresh: next,
            })
        })
    }

    fn revoke_family<'a>(&'a self, family: &'a str) -> moso_core::BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let mut state = self.lock()?;
            state.revoked.insert(family.to_owned());
            let before = state.rows.len();
            state.rows.retain(|_, row| row.family != family);
            Ok(u64::try_from(before - state.rows.len()).unwrap_or(u64::MAX))
        })
    }

    fn revoke_subject<'a>(&'a self, subject: &'a str) -> moso_core::BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let mut state = self.lock()?;
            let families: Vec<String> = state
                .rows
                .values()
                .filter(|row| row.subject == subject)
                .map(|row| row.family.clone())
                .collect();
            state.revoked.extend(families);
            let before = state.rows.len();
            state.rows.retain(|_, row| row.subject != subject);
            Ok(u64::try_from(before - state.rows.len()).unwrap_or(u64::MAX))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2048-bit RSA key in PKCS#8 DER, base64.
    ///
    /// Committed rather than generated: `ring` cannot generate RSA keys, and
    /// shelling out to `openssl` in a unit test makes the suite depend on what
    /// happens to be installed. This key exists only in this file, signs only
    /// test tokens, and is therefore not a secret.
    const RSA_PKCS8_BASE64: &str = concat!(
        "MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC5YHGpy9b59YHGmYoNHXdona",
        "3P+dmIrKKwon4VToOocInbT6Ioe6WURoQ6f3zx6p9xwgckuilWg4V26yxQ66BhfIkh9RDabUFU",
        "UwijDJZCcg7UJpLRl6Yb10SIBaW/cVZP9JQX6Vem8GzZAMO/bREZzzMyhtZyN1zQiXGz9jpvHK",
        "sg5QASpiXqbWOKsTMQAJ/TyZJwHj639TF/WF3hpgEz/8eM1NG1HGuq6Myvh5Ah1VqUOCSyI5Sz",
        "zb5t3buLz25dnn+mO4Mi1yqyzFPqThl7eGffjJ7kmxKwJRvyj1hXjL96CSqFM5E0HNAz3/b/lH",
        "/EYJs/QjjPoIuUZ54Gtbl/AgMBAAECggEABGzomvkGGOBx6w++NqV6SIQ6hQWsxniVgp2HEcvq",
        "Rclfh0nwsmS2z0vpM7068JlfrYdSCbimdksca2aUeFV1OxcrxprA4RdDFNfhYoTuykxd/rhAZ/",
        "Nvsaglt19s/NigMi2GygExD/QuyLrPQESRcgUiWulIr0yTgm1rtqBZp2IiBUKGDuFwkAOPTIDV",
        "Iitde21oi7bUJ4luNPccOCZSh97+ea2nggxRYmlaCIOLY2GH4G6afScRQBqzWaM7bnGivyR3rR",
        "gYAzRK953rerIyoS6ACInaP4Nxw5e2ArOn7kCFvWKoPlMj/4BE1UXqN8j9Goj5fpTebb41WIbM",
        "XPHtyQKBgQD5KO3qJpXLnousXzUv5S7VDXEI0+wtejAVSV/PL5vR+WpNDRtfNeFnD7QxZyjOdL",
        "8i2MqZlhIBvSu6HHlI2vB+TrvXytFJ63VSsWUYUPCSZYqxybeK/psB4XC3resFCBEZL5uCrl6D",
        "MgbMrojyCQzicMJH5o7onxN8P6qlxUzU+wKBgQC+d0DKmbDcWwqLuRaFzckTgl7it1i+EmBLlc",
        "3e+yFFNhSCYaVxFevaSKrpSqeMcl7mt8mNXWP2m+qRSJMVNmRGVpTplLC93mHg7n3MK1kOdLw1",
        "5X46GhvmUkiJ4pIlwwem/EnRt8sr9chs3t9NiVG8mCPXCawcrEyqI9C/tTjeTQKBgQDlbLS2GN",
        "Vx6wl9rSVSdtwKvhfJIyqkLZC86RVZt+LpE5q4XEtJ/lkRBzrLCsxeXs3pDmpvxenKxB/RfYqI",
        "dWFhTKpW56CTSkZ74HDQVSdQBkQRtUZWrF6a+rVJzNFNjsH/yQCO8nSApb3xFv1usLq2f1HF1x",
        "zcQi41CILfpa87pQKBgBCSQ5rdAFxLt4Esm18M5n/CCgtjtF7lLmelIwJRizWAXQxy/nf2Vkzp",
        "oaUmj6lSkhs0xl58T6Q3MJNvYwynbNYJ1m70NuRuIsn1NhC7fMYbNfzieLcJaoABjLoicmDCtT",
        "m8HZgXi5/JhKSkR31xgyELg6LD/quH+iubbiAf3lDJAoGBAJHjWKJz0srZHWVZnCWHzNCBrt5g",
        "PniRinlrt7rqLesyZI75mSPC9wAr9Pv+vPSRaVaoN2zj7lET/C15SH4GQmbVqYprxrHMQsrKIQ",
        "1y3cftir9MSk9FofsL4RmokG4NTXCKk0MvIUxU1izxnibP3kkj6Z/0LCKaWFLJQde+SvEf",
    );

    /// A PKCS#8 Ed25519 key.
    fn ed25519_key() -> SecretBytes {
        let rng = ring::rand::SystemRandom::new();
        let document = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).expect("generated");
        SecretBytes::new(document.as_ref().to_vec())
    }

    /// A PKCS#8 P-256 key.
    fn p256_key() -> SecretBytes {
        let rng = ring::rand::SystemRandom::new();
        let document = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .expect("generated");
        SecretBytes::new(document.as_ref().to_vec())
    }

    /// The committed RSA key, decoded.
    fn rsa_key() -> SecretBytes {
        use base64::Engine as _;

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(RSA_PKCS8_BASE64)
            .expect("the committed key is valid base64");
        SecretBytes::new(bytes)
    }

    /// An issuer over `algorithm`, with a key it can actually use.
    fn issuer(algorithm: JwtAlgorithm) -> Jwt<Claims> {
        let config = JwtConfig {
            algorithm,
            allow_symmetric: true,
            ..JwtConfig::default()
        };
        let key = match algorithm {
            JwtAlgorithm::EdDSA => ed25519_key(),
            JwtAlgorithm::ES256 => p256_key(),
            JwtAlgorithm::RS256 => rsa_key(),
            JwtAlgorithm::HS256 => SecretBytes::new(vec![7u8; 32]),
        };
        Jwt::issuer(config, "test-key", key).expect("built")
    }

    /// Sign an arbitrary header and payload with `jwt`'s own key.
    ///
    /// This is what makes the policy tests below mean something: the token is
    /// *correctly signed*, so the only thing that can reject it is the check
    /// under test. A test that hand-builds a token with a broken signature
    /// proves the signature check works and nothing else.
    fn sign_raw(
        jwt: &Jwt<Claims>,
        header: &serde_json::Value,
        payload: &serde_json::Value,
    ) -> String {
        let key = jwt.signing_key.as_ref().expect("an issuer");
        let mut token = b64u(&serde_json::to_vec(header).expect("header"));
        token.push('.');
        token.push_str(&b64u(&serde_json::to_vec(payload).expect("payload")));
        let signature = key.sign(token.as_bytes()).expect("signed");
        token.push('.');
        token.push_str(&b64u(&signature));
        token
    }

    /// A payload that is valid in every respect, as a starting point.
    fn good_payload() -> serde_json::Value {
        let now = Utc::now().timestamp();
        serde_json::json!({ "sub": "usr_1", "iat": now, "exp": now + 300 })
    }

    /// Every algorithm signs something this crate can verify. Without this,
    /// "supports ES256" is a claim about an enum variant.
    #[test]
    fn every_algorithm_round_trips() {
        for algorithm in [
            JwtAlgorithm::EdDSA,
            JwtAlgorithm::ES256,
            JwtAlgorithm::RS256,
            JwtAlgorithm::HS256,
        ] {
            let jwt = issuer(algorithm);
            let token = jwt
                .issue(&Claims::new("usr_1"), Duration::from_secs(300))
                .unwrap_or_else(|error| panic!("{algorithm:?} could not issue: {error}"));
            let claims = jwt
                .verify(&token)
                .unwrap_or_else(|error| panic!("{algorithm:?} could not verify: {error}"));
            assert_eq!(claims.subject(), "usr_1", "{algorithm:?}");
            assert!(claims.exp > claims.iat, "{algorithm:?}");
        }
    }

    /// The signature is over the header *and* the payload, for every algorithm.
    #[test]
    fn every_algorithm_covers_the_payload() {
        for algorithm in [
            JwtAlgorithm::EdDSA,
            JwtAlgorithm::ES256,
            JwtAlgorithm::RS256,
            JwtAlgorithm::HS256,
        ] {
            let jwt = issuer(algorithm);
            let token = jwt
                .issue(&Claims::new("usr_1"), Duration::from_secs(300))
                .unwrap();
            let parts: Vec<&str> = token.split('.').collect();
            let mut claims: serde_json::Value =
                serde_json::from_slice(&unb64u(parts[1]).unwrap()).unwrap();
            claims["sub"] = serde_json::Value::from("usr_admin");
            let forged = format!(
                "{}.{}.{}",
                parts[0],
                b64u(&serde_json::to_vec(&claims).unwrap()),
                parts[2]
            );
            assert!(
                matches!(jwt.verify(&forged), Err(Error::InvalidCredentials)),
                "{algorithm:?} accepted a rewritten payload"
            );
        }
    }

    /// The header says what it signed with, and the kid names the key.
    #[test]
    fn the_header_carries_the_algorithm_and_the_kid() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let token = jwt
            .issue(&Claims::new("usr_1"), Duration::from_secs(60))
            .unwrap();
        let header_part = token.split('.').next().unwrap();
        let header: serde_json::Value =
            serde_json::from_slice(&unb64u(header_part).unwrap()).unwrap();
        assert_eq!(header["alg"], "EdDSA");
        assert_eq!(header["typ"], "JWT");
        assert_eq!(header["kid"], "test-key");
    }

    /// `alg: none` is the oldest JWT attack there is. It is not defended
    /// against here; it is unrepresentable, and this test says so — the token
    /// below carries a *valid* Ed25519 signature and is still refused.
    #[test]
    fn alg_none_is_rejected_even_when_correctly_signed() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let signed_none = sign_raw(
            &jwt,
            &serde_json::json!({ "alg": "none", "typ": "JWT", "kid": "test-key" }),
            &good_payload(),
        );
        assert!(matches!(
            jwt.verify(&signed_none),
            Err(Error::InvalidCredentials)
        ));

        // And the classic unsecured spellings: no signature at all.
        let header = b64u(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = b64u(&serde_json::to_vec(&good_payload()).unwrap());
        assert!(jwt.verify(&format!("{header}.{payload}.")).is_err());
        assert!(jwt.verify(&format!("{header}.{payload}")).is_err());
    }

    /// Claiming HS256 on an RS256 verifier is the algorithm-confusion attack.
    /// The signature below is a genuine RSA signature over that very header, so
    /// the only thing rejecting it is the refusal to read `alg`.
    #[test]
    fn a_swapped_algorithm_in_the_header_is_rejected() {
        let jwt = issuer(JwtAlgorithm::RS256);
        let forged = sign_raw(
            &jwt,
            &serde_json::json!({ "alg": "HS256", "typ": "JWT", "kid": "test-key" }),
            &good_payload(),
        );
        assert!(matches!(
            jwt.verify(&forged),
            Err(Error::InvalidCredentials)
        ));
    }

    /// A `kid` that names no key is a rejection, not a fallback to trying every
    /// key. The token is correctly signed by the one key this verifier holds;
    /// only the name is wrong.
    #[test]
    fn an_unknown_kid_does_not_fall_back_to_trying_every_key() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let forged = sign_raw(
            &jwt,
            &serde_json::json!({ "alg": "EdDSA", "typ": "JWT", "kid": "some-other-key" }),
            &good_payload(),
        );
        assert!(matches!(
            jwt.verify(&forged),
            Err(Error::InvalidCredentials)
        ));

        // With the right name, the same key verifies the same payload.
        let honest = sign_raw(
            &jwt,
            &serde_json::json!({ "alg": "EdDSA", "typ": "JWT", "kid": "test-key" }),
            &good_payload(),
        );
        assert!(jwt.verify(&honest).is_ok());
    }

    /// A header with no `kid` at all is still verifiable, because a JWKS-less
    /// issuer is allowed to omit it.
    #[test]
    fn a_token_without_a_kid_is_verified_against_every_key() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let token = sign_raw(
            &jwt,
            &serde_json::json!({ "alg": "EdDSA", "typ": "JWT" }),
            &good_payload(),
        );
        assert_eq!(jwt.verify(&token).unwrap().subject(), "usr_1");
    }

    /// A token from one issuer must not verify against another's keys, however
    /// well-formed it is.
    #[test]
    fn a_token_signed_by_another_key_is_rejected() {
        let ours = issuer(JwtAlgorithm::EdDSA);
        let theirs = issuer(JwtAlgorithm::EdDSA);
        let token = theirs
            .issue(&Claims::new("usr_1"), Duration::from_secs(60))
            .unwrap();
        assert!(matches!(
            ours.verify(&token),
            Err(Error::InvalidCredentials)
        ));
    }

    /// A `crit` header names extensions the verifier must understand. Moso
    /// understands none, so any is a rejection — and, again, the signature is
    /// real.
    #[test]
    fn a_critical_header_extension_is_rejected() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let forged = sign_raw(
            &jwt,
            &serde_json::json!({ "alg": "EdDSA", "kid": "test-key", "crit": ["exp"] }),
            &good_payload(),
        );
        assert!(matches!(
            jwt.verify(&forged),
            Err(Error::InvalidCredentials)
        ));
    }

    /// Expiry is `Expired`, not `InvalidCredentials`: the holder can act on it.
    #[test]
    fn an_expired_token_says_so() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let token = jwt.issue(&Claims::new("usr_1"), Duration::ZERO).unwrap();
        // Zero TTL plus the default 60-second leeway means it is not expired
        // *yet*, which is the leeway doing its job.
        assert!(jwt.verify(&token).is_ok());

        let strict: Jwt<Claims> = Jwt::issuer(
            JwtConfig {
                leeway: Duration::ZERO,
                ..JwtConfig::default()
            },
            "k",
            ed25519_key(),
        )
        .unwrap();
        let token = strict.issue(&Claims::new("usr_1"), Duration::ZERO).unwrap();
        assert!(matches!(
            strict.verify(&token),
            Err(Error::Expired { kind: "token" })
        ));
    }

    /// A token with no `exp` is a credential that never dies, and this crate
    /// will not accept one — even correctly signed.
    #[test]
    fn a_token_without_an_expiry_is_rejected() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let forged = sign_raw(
            &jwt,
            &serde_json::json!({ "alg": "EdDSA", "kid": "test-key" }),
            &serde_json::json!({ "sub": "usr_1", "iat": Utc::now().timestamp() }),
        );
        assert!(matches!(
            jwt.verify(&forged),
            Err(Error::InvalidCredentials)
        ));
    }

    /// A token issued in the future is either a badly-set clock or a forgery,
    /// and the leeway is the line between them.
    #[test]
    fn a_token_issued_in_the_future_is_rejected() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let now = Utc::now().timestamp();
        let forged = sign_raw(
            &jwt,
            &serde_json::json!({ "alg": "EdDSA", "kid": "test-key" }),
            &serde_json::json!({ "sub": "usr_1", "iat": now + 3600, "exp": now + 7200 }),
        );
        assert!(matches!(
            jwt.verify(&forged),
            Err(Error::InvalidCredentials)
        ));

        // Inside the leeway it is accepted: clocks do drift.
        let skewed = sign_raw(
            &jwt,
            &serde_json::json!({ "alg": "EdDSA", "kid": "test-key" }),
            &serde_json::json!({ "sub": "usr_1", "iat": now + 30, "exp": now + 300 }),
        );
        assert!(jwt.verify(&skewed).is_ok());
    }

    /// A token that is not yet valid is refused, with the leeway applied.
    #[test]
    fn a_not_yet_valid_token_is_rejected() {
        let jwt: Jwt<Claims> = Jwt::issuer(
            JwtConfig {
                leeway: Duration::ZERO,
                ..JwtConfig::default()
            },
            "k",
            ed25519_key(),
        )
        .unwrap();
        let mut claims = Claims::new("usr_1");
        claims.nbf = Some(Utc::now().timestamp() + 600);
        let token = jwt.issue(&claims, Duration::from_secs(900)).unwrap();
        assert!(matches!(jwt.verify(&token), Err(Error::InvalidCredentials)));
    }

    /// An unaudienced token is a token any service accepts, so a configured
    /// audience is enforced — in both spellings RFC 7519 allows.
    #[test]
    fn the_audience_is_enforced_in_both_spellings() {
        let jwt: Jwt<Claims> = Jwt::issuer(
            JwtConfig {
                audience: Some("api.example.com".to_owned()),
                ..JwtConfig::default()
            },
            "k",
            ed25519_key(),
        )
        .unwrap();

        // Filled in from the configuration when the claims do not carry one.
        let token = jwt
            .issue(&Claims::new("usr_1"), Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            jwt.verify(&token).unwrap().aud.as_deref(),
            Some("api.example.com")
        );

        // The array spelling, correctly signed.
        let now = Utc::now().timestamp();
        let array = sign_raw(
            &jwt,
            &serde_json::json!({ "alg": "EdDSA", "kid": "k" }),
            &serde_json::json!({
                "sub": "usr_1", "iat": now, "exp": now + 300,
                "aud": ["other.example.com", "api.example.com"],
            }),
        );
        let verified = jwt.verify(&array).expect("an array audience must be read");
        assert_eq!(
            verified.aud.as_deref(),
            Some("other.example.com"),
            "the first entry is kept; the check ran against the whole list"
        );

        // Somebody else's audience.
        let other = jwt
            .issue(
                &Claims::new("usr_1").with_audience("admin.example.com"),
                Duration::from_secs(60),
            )
            .unwrap();
        assert!(matches!(jwt.verify(&other), Err(Error::InvalidCredentials)));

        // And no audience at all.
        let missing = sign_raw(
            &jwt,
            &serde_json::json!({ "alg": "EdDSA", "kid": "k" }),
            &serde_json::json!({ "sub": "usr_1", "iat": now, "exp": now + 300 }),
        );
        assert!(matches!(
            jwt.verify(&missing),
            Err(Error::InvalidCredentials)
        ));
    }

    /// A configured issuer is enforced too.
    #[test]
    fn the_issuer_is_enforced() {
        let config = JwtConfig {
            issuer: Some("https://accounts.example.com".to_owned()),
            ..JwtConfig::default()
        };
        let jwt: Jwt<Claims> = Jwt::issuer(config.clone(), "k", ed25519_key()).unwrap();
        let token = jwt
            .issue(&Claims::new("usr_1"), Duration::from_secs(60))
            .unwrap();
        assert!(jwt.verify(&token).is_ok());

        let checker: Jwt<Claims> = Jwt::verifier(
            JwtConfig {
                issuer: Some("https://evil.example.com".to_owned()),
                ..config
            },
            vec![("k".to_owned(), public_of(&jwt))],
        )
        .unwrap();
        assert!(matches!(
            checker.verify(&token),
            Err(Error::InvalidCredentials)
        ));
    }

    /// The public key of an issuer, read back out of its own JWKS.
    fn public_of(jwt: &Jwt<Claims>) -> Vec<u8> {
        let document = jwt.jwks();
        let x = document["keys"][0]["x"].as_str().expect("an OKP key");
        unb64u(x).expect("base64url")
    }

    /// HS256 must be an explicit decision, and the error must say what to do.
    #[test]
    fn a_symmetric_algorithm_requires_the_opt_in() {
        let config = JwtConfig {
            algorithm: JwtAlgorithm::HS256,
            ..JwtConfig::default()
        };
        let error =
            Jwt::<Claims>::issuer(config, "k", SecretBytes::new(vec![1u8; 32])).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("allow_symmetric"), "{text}");
        assert!(text.contains("EdDSA"), "{text}");
    }

    /// A verifier with no keys accepts nothing, so it is refused at boot.
    #[test]
    fn a_verifier_needs_at_least_one_key() {
        let error = Jwt::<Claims>::verifier(JwtConfig::default(), vec![]).unwrap_err();
        assert!(error.to_string().contains("at least one key"), "{error}");
    }

    /// Two keys under one `kid` makes the `kid` ambiguous, which is a
    /// configuration bug worth catching at boot.
    #[test]
    fn two_keys_may_not_share_a_kid() {
        let key = vec![0u8; 32];
        let error = Jwt::<Claims>::verifier(
            JwtConfig::default(),
            vec![("k".to_owned(), key.clone()), ("k".to_owned(), key)],
        )
        .unwrap_err();
        assert!(error.to_string().contains("share the kid"), "{error}");
    }

    /// A signing key with no `kid` cannot be published or rotated.
    #[test]
    fn an_issuer_needs_a_kid() {
        let error = Jwt::<Claims>::issuer(JwtConfig::default(), "", ed25519_key()).unwrap_err();
        assert!(error.to_string().contains("needs a kid"), "{error}");
    }

    /// A key that is not a key names the encoding it should have been.
    #[test]
    fn a_signing_key_that_does_not_parse_says_what_it_should_be() {
        let error =
            Jwt::<Claims>::issuer(JwtConfig::default(), "k", SecretBytes::new(vec![1, 2, 3]))
                .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("PKCS#8"), "{text}");
        assert!(matches!(error, Error::Config(_)));
    }

    /// A verifier cannot issue, and the error names the constructor that can.
    #[test]
    fn a_verifier_cannot_issue() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let verifier: Jwt<Claims> = Jwt::verifier(
            JwtConfig::default(),
            vec![("k".to_owned(), public_of(&jwt))],
        )
        .unwrap();
        let error = verifier
            .issue(&Claims::new("usr_1"), Duration::from_secs(60))
            .unwrap_err();
        assert!(error.to_string().contains("Jwt::issuer"), "{error}");
    }

    /// Rotation is two-phase: the old key keeps verifying while the new one
    /// signs. Without `also_verifying`, rotating a key is an outage.
    #[test]
    fn a_rotated_key_still_verifies_tokens_in_flight() {
        let old = issuer(JwtAlgorithm::EdDSA);
        let old_token = old
            .issue(&Claims::new("usr_1"), Duration::from_secs(900))
            .unwrap();

        let new: Jwt<Claims> = Jwt::issuer(JwtConfig::default(), "2026-08", ed25519_key())
            .unwrap()
            .also_verifying("test-key", &public_of(&old))
            .unwrap();

        assert!(new.verify(&old_token).is_ok(), "the old token still works");
        let new_token = new
            .issue(&Claims::new("usr_1"), Duration::from_secs(900))
            .unwrap();
        assert!(new.verify(&new_token).is_ok());
        assert!(
            old.verify(&new_token).is_err(),
            "the old deployment does not yet know the new key"
        );
        assert_eq!(new.jwks()["keys"].as_array().unwrap().len(), 2);
    }

    /// Registering the same `kid` twice is refused rather than silently
    /// shadowing.
    #[test]
    fn also_verifying_refuses_a_duplicate_kid() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let public = public_of(&jwt);
        let error = jwt.also_verifying("test-key", &public).unwrap_err();
        assert!(error.to_string().contains("already registered"), "{error}");
    }

    /// The published JWKS is what a consumer needs, in the shape it expects,
    /// and nothing more.
    #[test]
    fn the_jwks_publishes_every_verifying_key() {
        let jwt = issuer(JwtAlgorithm::ES256);
        let document = jwt.jwks();
        let keys = document["keys"].as_array().expect("an array");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0]["kty"], "EC");
        assert_eq!(keys[0]["crv"], "P-256");
        assert_eq!(keys[0]["kid"], "test-key");
        assert_eq!(keys[0]["use"], "sig");
        assert_eq!(keys[0]["alg"], "ES256");
        assert!(keys[0]["x"].is_string() && keys[0]["y"].is_string());
        assert!(
            keys[0].get("d").is_none(),
            "a JWKS must never carry a private component"
        );
    }

    /// An RSA JWKS carries `n` and `e`, and 65537 is `AQAB` everywhere.
    #[test]
    fn an_rsa_jwks_publishes_the_modulus_and_exponent() {
        let jwt = issuer(JwtAlgorithm::RS256);
        let document = jwt.jwks();
        assert_eq!(document["keys"][0]["kty"], "RSA");
        assert_eq!(document["keys"][0]["e"], "AQAB");
        let modulus = unb64u(document["keys"][0]["n"].as_str().unwrap()).unwrap();
        assert_eq!(modulus.len(), 256, "2048 bits");
    }

    /// A symmetric configuration has nothing to publish, which is the argument
    /// against it written as an assertion.
    #[test]
    fn a_symmetric_jwks_is_empty() {
        let jwt = issuer(JwtAlgorithm::HS256);
        assert_eq!(jwt.jwks()["keys"].as_array().unwrap().len(), 0);
    }

    /// A published key really is the verifying key: round-trip it through a
    /// JWKS and verify a token with what came back.
    #[test]
    fn a_published_key_verifies_what_the_issuer_signed() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let token = jwt
            .issue(&Claims::new("usr_1"), Duration::from_secs(300))
            .unwrap();
        let consumer: Jwt<Claims> = Jwt::verifier(
            JwtConfig::default(),
            vec![("test-key".to_owned(), public_of(&jwt))],
        )
        .unwrap();
        assert_eq!(consumer.verify(&token).unwrap().subject(), "usr_1");
    }

    /// An application's own claims type round-trips, and the registered checks
    /// still apply to it.
    #[test]
    fn a_custom_claims_type_round_trips() {
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct Service {
            /// Which service.
            sub: String,
            /// What it may do.
            scope: Vec<String>,
            /// Filled by `issue`.
            #[serde(default)]
            exp: i64,
        }

        let jwt: Jwt<Service> = Jwt::issuer(JwtConfig::default(), "k", ed25519_key()).unwrap();
        let claims = Service {
            sub: "svc_billing".to_owned(),
            scope: vec!["invoices:read".to_owned()],
            exp: 0,
        };
        let token = jwt.issue(&claims, Duration::from_secs(60)).unwrap();
        let back = jwt.verify(&token).unwrap();
        assert_eq!(back.sub, "svc_billing");
        assert_eq!(back.scope, vec!["invoices:read".to_owned()]);
        assert!(back.exp > 0, "the issuer stamped it");
    }

    /// Claims that are not an object are not a JWT payload.
    #[test]
    fn a_non_object_payload_is_refused() {
        let jwt: Jwt<Vec<String>> = Jwt::issuer(JwtConfig::default(), "k", ed25519_key()).unwrap();
        let error = jwt
            .issue(
                &vec!["not".to_owned(), "an object".to_owned()],
                Duration::from_secs(60),
            )
            .unwrap_err();
        assert!(error.to_string().contains("JSON object"), "{error}");
    }

    /// Garbage in every place a token can be garbage.
    #[test]
    fn malformed_tokens_are_rejected_without_panicking() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        for token in [
            "",
            ".",
            "..",
            "a.b.c",
            "a.b.c.d.e",
            "not-base64!.eyJ9.sig",
            "eyJhbGciOiJFZERTQSJ9",
            "eyJhbGciOiJFZERTQSJ9.eyJzdWIiOiJ4In0",
            &"x".repeat(10_000),
        ] {
            assert!(jwt.verify(token).is_err(), "accepted {token:?}");
        }
    }

    /// `Claims::claim` reads what `with_claim` wrote, and says so when the type
    /// is wrong rather than silently returning `None`.
    #[test]
    fn application_claims_survive_the_round_trip() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let claims = Claims::new("usr_1")
            .with_claim("tenant", serde_json::json!("acme"))
            .with_claim("seats", serde_json::json!(3));
        let token = jwt.issue(&claims, Duration::from_secs(60)).unwrap();
        let back = jwt.verify(&token).unwrap();
        assert_eq!(
            back.claim::<String>("tenant").unwrap().as_deref(),
            Some("acme")
        );
        assert_eq!(back.claim::<u8>("seats").unwrap(), Some(3));
        assert!(back.claim::<u8>("tenant").is_err());
        assert_eq!(back.claim::<u8>("nothing").unwrap(), None);
    }

    /// Fifteen minutes is the documented access-token lifetime, and `issue`
    /// honours the `ttl` it is given rather than whatever `exp` was already on
    /// the claims.
    #[test]
    fn the_ttl_wins_over_a_preset_expiry() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let mut claims = Claims::new("usr_1");
        claims.exp = Utc::now().timestamp() + 10 * 365 * 24 * 3600;
        let token = jwt.issue(&claims, Duration::from_secs(900)).unwrap();
        let back = jwt.verify(&token).unwrap();
        let lifetime = back.exp - Utc::now().timestamp();
        assert!(
            (890..=900).contains(&lifetime),
            "expected ~900 seconds, got {lifetime}"
        );
    }

    /// The `Debug` impl must not be a way to read a key.
    #[test]
    fn the_debug_impl_leaks_nothing() {
        let jwt = issuer(JwtAlgorithm::HS256);
        let rendered = format!("{jwt:?}");
        assert!(rendered.contains("HS256"), "{rendered}");
        assert!(rendered.contains("keys: 1"), "{rendered}");
        assert!(rendered.contains("can_issue: true"), "{rendered}");
    }

    // -----------------------------------------------------------------------
    // Refresh tokens
    // -----------------------------------------------------------------------

    /// A store over a real issuer.
    fn refresh_store() -> MemoryRefreshStore {
        MemoryRefreshStore::new(Arc::new(issuer(JwtAlgorithm::EdDSA)))
    }

    /// The happy path: issue, exchange, get a new pair, and the new access
    /// token verifies against the store's own issuer and nobody else's.
    #[tokio::test]
    async fn a_refresh_token_rotates() {
        let store = refresh_store();
        let first = store
            .issue("usr_1", Duration::from_secs(3600))
            .await
            .unwrap();
        let outcome = store.exchange(first.expose()).await.unwrap();
        let RefreshOutcome::Rotated { access, refresh } = outcome else {
            panic!("expected a rotation, got {outcome:?}");
        };
        assert_eq!(refresh.family(), first.family(), "same family");
        assert_ne!(refresh.expose(), first.expose(), "a new token");
        assert!(store.jwt.verify(&access).is_ok());
        assert_eq!(store.jwt.verify(&access).unwrap().subject(), "usr_1");
        assert!(
            issuer(JwtAlgorithm::EdDSA).verify(&access).is_err(),
            "a different issuer must not verify it"
        );
    }

    /// The heart of it: a replayed token burns the family, and every descendant
    /// dies with it.
    #[tokio::test]
    async fn reuse_revokes_the_whole_family() {
        let store = refresh_store();
        let first = store
            .issue("usr_1", Duration::from_secs(3600))
            .await
            .unwrap();
        let RefreshOutcome::Rotated {
            refresh: second, ..
        } = store.exchange(first.expose()).await.unwrap()
        else {
            panic!("expected a rotation");
        };

        // The attacker replays the token the legitimate client already used.
        let outcome = store.exchange(first.expose()).await.unwrap();
        let RefreshOutcome::ReuseDetected { family } = outcome else {
            panic!("expected reuse detection, got {outcome:?}");
        };
        assert_eq!(family, first.family());

        // And the legitimate client's *current* token is dead too. That is the
        // trade: one logout beats an attacker with a self-renewing credential.
        assert!(matches!(
            store.exchange(second.expose()).await.unwrap(),
            RefreshOutcome::Invalid
        ));
        assert!(store.is_empty(), "the family's rows are gone");
    }

    /// Two concurrent exchanges of one token produce exactly one rotation. This
    /// is the race the trait documentation calls out, run for real.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_exchanges_produce_one_rotation() {
        let store = Arc::new(refresh_store());
        let token = store
            .issue("usr_1", Duration::from_secs(3600))
            .await
            .unwrap();
        let value = token.expose().to_owned();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let store = Arc::clone(&store);
            let value = value.clone();
            handles.push(tokio::spawn(async move {
                store.exchange(&value).await.unwrap()
            }));
        }
        let mut rotations = 0;
        let mut reuses = 0;
        for handle in handles {
            match handle.await.unwrap() {
                RefreshOutcome::Rotated { .. } => rotations += 1,
                RefreshOutcome::ReuseDetected { .. } => reuses += 1,
                RefreshOutcome::Invalid => {}
            }
        }
        assert_eq!(rotations, 1, "exactly one exchange may win");
        assert!(reuses >= 1, "the losers must be seen as reuse");
    }

    /// An unknown token is `Invalid` and revokes nothing — there is no family
    /// to burn, and burning one on a guess would be a denial of service.
    #[tokio::test]
    async fn an_unknown_token_revokes_nothing() {
        let store = refresh_store();
        let live = store
            .issue("usr_1", Duration::from_secs(3600))
            .await
            .unwrap();
        assert!(matches!(
            store.exchange("not-a-token").await.unwrap(),
            RefreshOutcome::Invalid
        ));
        assert!(matches!(
            store.exchange(live.expose()).await.unwrap(),
            RefreshOutcome::Rotated { .. }
        ));
    }

    /// An expired token is `Invalid`, not reuse: it was never exchanged.
    #[tokio::test]
    async fn an_expired_token_is_invalid_not_reuse() {
        let store = refresh_store();
        let token = store.issue("usr_1", Duration::ZERO).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(matches!(
            store.exchange(token.expose()).await.unwrap(),
            RefreshOutcome::Invalid
        ));
    }

    /// "Log out everywhere" for token clients: every family for the subject
    /// dies at once, and nobody else's does.
    #[tokio::test]
    async fn revoking_a_subject_kills_every_family() {
        let store = refresh_store();
        let one = store
            .issue("usr_1", Duration::from_secs(3600))
            .await
            .unwrap();
        let two = store
            .issue("usr_1", Duration::from_secs(3600))
            .await
            .unwrap();
        let other = store
            .issue("usr_2", Duration::from_secs(3600))
            .await
            .unwrap();
        assert_ne!(one.family(), two.family(), "two logins, two families");

        assert_eq!(store.revoke_subject("usr_1").await.unwrap(), 2);
        assert!(matches!(
            store.exchange(one.expose()).await.unwrap(),
            RefreshOutcome::Invalid
        ));
        assert!(matches!(
            store.exchange(two.expose()).await.unwrap(),
            RefreshOutcome::Invalid
        ));
        assert!(matches!(
            store.exchange(other.expose()).await.unwrap(),
            RefreshOutcome::Rotated { .. }
        ));
    }

    /// Revoking one family leaves the others alone.
    #[tokio::test]
    async fn revoking_a_family_is_surgical() {
        let store = refresh_store();
        let one = store
            .issue("usr_1", Duration::from_secs(3600))
            .await
            .unwrap();
        let two = store
            .issue("usr_1", Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(store.revoke_family(one.family()).await.unwrap(), 1);
        assert!(matches!(
            store.exchange(one.expose()).await.unwrap(),
            RefreshOutcome::Invalid
        ));
        assert!(matches!(
            store.exchange(two.expose()).await.unwrap(),
            RefreshOutcome::Rotated { .. }
        ));
    }

    /// Rotation chains: a token can be exchanged repeatedly, and only the
    /// newest one works.
    #[tokio::test]
    async fn a_family_rotates_repeatedly() {
        let store = refresh_store();
        let mut current = store
            .issue("usr_1", Duration::from_secs(3600))
            .await
            .unwrap();
        let family = current.family().to_owned();
        for _ in 0..5 {
            let RefreshOutcome::Rotated { refresh, .. } =
                store.exchange(current.expose()).await.unwrap()
            else {
                panic!("expected a rotation");
            };
            assert_eq!(refresh.family(), family, "the family is stable");
            current = refresh;
        }
        assert!(matches!(
            store.exchange(current.expose()).await.unwrap(),
            RefreshOutcome::Rotated { .. }
        ));
    }

    /// Only the hash is ever stored; a database dump must not be a set of live
    /// tokens.
    #[test]
    fn only_the_hash_is_storable() {
        let token = RefreshToken::mint("usr_1", "fam", Duration::from_secs(60)).unwrap();
        assert_eq!(token.hash(), RefreshToken::hash_of(token.expose()));
        assert_eq!(token.hash().len(), 64);
        assert!(!token.hash().contains(token.expose()));
        assert!(token.is_live(Utc::now()));
        assert!(!token.is_live(Utc::now() + chrono::TimeDelta::try_seconds(3600).unwrap()));
    }

    /// Two tokens are never the same. 256 bits from the system CSPRNG.
    #[test]
    fn tokens_and_families_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            let token = RefreshToken::mint("u", "f", Duration::from_secs(60)).unwrap();
            assert!(seen.insert(token.expose().to_owned()), "a token repeated");
            assert!(
                seen.insert(RefreshToken::new_family().unwrap()),
                "a family repeated"
            );
        }
    }

    /// The token survives serialisation, because delivering it is the point —
    /// but a `Debug` still must not print it.
    #[test]
    fn a_refresh_token_serialises_but_does_not_debug_print() {
        let token = RefreshToken::mint("usr_1", "fam", Duration::from_secs(60)).unwrap();
        let json = serde_json::to_string(&token).expect("serialises");
        assert!(json.contains(token.expose()));
        let rendered = format!("{token:?}");
        assert!(
            !rendered.contains(token.expose()),
            "Debug must not print the token: {rendered}"
        );
    }

    // -----------------------------------------------------------------------
    // RemoteJwks
    // -----------------------------------------------------------------------

    /// A JWKS server that counts how many times it was asked.
    struct CountingJwks {
        /// Where it listens.
        address: std::net::SocketAddr,
        /// How many requests it has served.
        hits: Arc<std::sync::atomic::AtomicUsize>,
    }

    /// Serve `body` for every request, forever, counting hits.
    async fn serve_jwks(body: String) -> CountingJwks {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;

            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Read the request before answering: closing with it unread
                // makes the kernel reset rather than close, which is a property
                // of this test server and not of the client.
                {
                    use tokio::io::AsyncReadExt as _;

                    let mut seen = Vec::new();
                    let mut buffer = [0u8; 1024];
                    while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut buffer).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => seen.extend_from_slice(&buffer[..read]),
                        }
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        CountingJwks { address, hits }
    }

    /// A token verified against a key fetched from a real socket.
    #[tokio::test]
    async fn a_remote_key_set_verifies_a_token() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let server = serve_jwks(jwt.jwks().to_string()).await;
        let remote = RemoteJwks::new(
            format!("http://{}/jwks", server.address),
            JwtConfig::default(),
        );
        let token = jwt
            .issue(&Claims::new("usr_1"), Duration::from_secs(300))
            .unwrap();
        assert_eq!(remote.verify(&token).await.unwrap().subject(), "usr_1");
        assert_eq!(remote.cached_key_count().await, 1);
    }

    /// The cache is the reason this type exists: a hundred verifications must
    /// not be a hundred requests.
    #[tokio::test]
    async fn the_key_set_is_fetched_once_and_cached() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let server = serve_jwks(jwt.jwks().to_string()).await;
        let remote = RemoteJwks::new(
            format!("http://{}/jwks", server.address),
            JwtConfig::default(),
        );
        let token = jwt
            .issue(&Claims::new("usr_1"), Duration::from_secs(300))
            .unwrap();
        for _ in 0..25 {
            remote.verify(&token).await.unwrap();
        }
        assert_eq!(
            server.hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "25 verifications, one fetch"
        );
    }

    /// A made-up `kid` must not become a request. This is the amplification
    /// guard, measured rather than asserted by inspection.
    #[tokio::test]
    async fn an_unknown_kid_does_not_hammer_the_issuer() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let server = serve_jwks(jwt.jwks().to_string()).await;
        let remote = RemoteJwks::new(
            format!("http://{}/jwks", server.address),
            JwtConfig::default(),
        )
        .refetch_at_most_every(Duration::from_secs(300));
        remote.refresh().await.unwrap();
        let before = server.hits.load(std::sync::atomic::Ordering::SeqCst);

        for index in 0..50 {
            let forged = sign_raw(
                &jwt,
                &serde_json::json!({ "alg": "EdDSA", "kid": format!("made-up-{index}") }),
                &good_payload(),
            );
            assert!(remote.verify(&forged).await.is_err());
        }
        assert_eq!(
            server.hits.load(std::sync::atomic::Ordering::SeqCst),
            before,
            "50 unknown kids must produce zero extra requests"
        );
    }

    /// Concurrency: many misses, one fetch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_misses_make_one_request() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let server = serve_jwks(jwt.jwks().to_string()).await;
        let remote = Arc::new(RemoteJwks::new(
            format!("http://{}/jwks", server.address),
            JwtConfig::default(),
        ));
        let token = jwt
            .issue(&Claims::new("usr_1"), Duration::from_secs(300))
            .unwrap();

        let mut handles = Vec::new();
        for _ in 0..16 {
            let remote = Arc::clone(&remote);
            let token = token.clone();
            handles.push(tokio::spawn(async move {
                remote.verify(&token).await.map(|claims| claims.sub)
            }));
        }
        for handle in handles {
            assert_eq!(handle.await.unwrap().unwrap(), "usr_1");
        }
        assert_eq!(server.hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// An endpoint that is down before anything is cached is an outage, not a
    /// rejection: degrading it to "not logged in" hides the failure.
    #[tokio::test]
    async fn an_unreachable_issuer_is_unavailable_not_unauthenticated() {
        let jwt = issuer(JwtAlgorithm::EdDSA);
        let remote = RemoteJwks::new("http://127.0.0.1:1/jwks", JwtConfig::default());
        let token = jwt
            .issue(&Claims::new("usr_1"), Duration::from_secs(300))
            .unwrap();
        let error = remote.verify(&token).await.unwrap_err();
        assert!(matches!(error, Error::Unavailable { .. }), "{error}");
    }

    /// A key set with nothing this verifier can use is an error, not an empty
    /// cache that rejects everything for an hour.
    #[tokio::test]
    async fn a_key_set_with_no_usable_key_is_an_error() {
        let server =
            serve_jwks(r#"{"keys":[{"kty":"RSA","kid":"r","n":"AA","e":"AQAB"}]}"#.to_owned())
                .await;
        let remote = RemoteJwks::new(
            format!("http://{}/jwks", server.address),
            JwtConfig::default(),
        );
        let error = remote.refresh().await.unwrap_err();
        assert!(
            error.to_string().contains("none usable for EdDSA"),
            "{error}"
        );
    }

    /// The defaults are the documented ones.
    #[test]
    fn the_remote_defaults_are_an_hour_and_five_minutes() {
        let remote = RemoteJwks::new("https://idp.example.com/jwks", JwtConfig::default());
        assert_eq!(remote.cache_ttl(), Duration::from_secs(3600));
        assert_eq!(remote.min_refetch(), Duration::from_secs(300));
        assert_eq!(remote.url(), "https://idp.example.com/jwks");
    }
}
