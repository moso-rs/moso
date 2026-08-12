//! Passkeys and WebAuthn: registration and authentication ceremonies, the
//! credential storage schema, clone detection, and the discoverable
//! (usernameless) flow.
//!
//! The ceremonies themselves are [`webauthn-rs`]'s — re-implementing CBOR
//! attestation parsing and COSE signature verification would be a liability,
//! not a feature. What this module owns is everything around them, and that is
//! where the mistakes actually live:
//!
//! 1. **The challenge has a server-side lifetime.** WebAuthn's `timeout` is a
//!    hint to the *browser*; nothing in the protocol stops a challenge captured
//!    an hour ago from being replayed. [`WebAuthnChallenge::expires_at`] is
//!    checked before the signature is, on every finish.
//! 2. **The ceremony state is tagged.** A registration state handed to
//!    `finish_authentication`, or a discoverable state finished as a
//!    non-discoverable one, is a type confusion with a signature attached. The
//!    tag makes it a refusal instead.
//! 3. **A counter regression is reported distinctly, and quarantines the row.**
//!    It is the one authentication failure that means "this credential exists
//!    twice", and the answer is not "try again" — it is
//!    [`PasskeyStore::disable`], so that neither copy works until a person has
//!    looked. [`is_clone_detected`] is how a route handler tells the two apart
//!    and [`CLONE_EVENT`] is what it logs; the mounted
//!    `POST /auth/passkeys/login/finish` already does both, and a hand-written
//!    handler has to.
//! 4. **User verification is a real switch.** A passkey is a two-factor
//!    credential precisely because the authenticator demanded a PIN or a
//!    biometric. [`WebAuthn::require_user_verification`] chooses between the
//!    passkey ceremony (verification required) and the security-key ceremony
//!    (verification preferred, and the credential is one factor of two). It
//!    does not silently do nothing.
//!
//! ```no_run
//! use moso_auth::{PasskeyCredential, WebAuthn};
//!
//! # fn f() -> moso_auth::Result<()> {
//! let rp = WebAuthn::new("example.com", "https://example.com", "Example");
//!
//! // 1. start, and put the challenge in the session
//! let challenge = rp.start_registration("usr_1", "ada@example.com", "Ada", &[])?;
//!
//! // 2. the browser signs `challenge.options` and posts the result back
//! # let response = serde_json::json!({});
//! let credential: PasskeyCredential = rp.finish_registration(&challenge, &response)?;
//! # let _ = credential;
//! # Ok(()) }
//! ```
//!
//! [`webauthn-rs`]: https://docs.rs/webauthn-rs

use std::borrow::Cow;
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, TimeDelta, Utc};
use moso_core::{BoxFuture, config::SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;
use webauthn_rs::prelude::{
    AttestationMetadata, AuthenticationResult, COSEKey, COSEKeyType, Credential, CredentialID,
    DiscoverableAuthentication, DiscoverableKey, Passkey, PasskeyAuthentication,
    PasskeyRegistration, PublicKeyCredential, RegisterPublicKeyCredential, SecurityKey,
    SecurityKeyAuthentication, SecurityKeyRegistration,
};
use webauthn_rs::{Webauthn as RelyingParty, WebauthnBuilder};

use crate::{Error, Result};

/// URL-safe base64 without padding — WebAuthn's encoding for every binary
/// value that crosses the wire.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// How long a challenge lives by default.
///
/// Sixty seconds is the WebAuthn recommendation for a *visible* prompt. It is
/// long enough for a user to find their security key and short enough that a
/// captured challenge is worthless by the time it is replayed.
///
/// ```
/// assert_eq!(moso_auth::webauthn::DEFAULT_TIMEOUT.as_secs(), 60);
/// ```
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// The reason a counter regression is reported with.
///
/// Compared by [`is_clone_detected`]; it is a constant rather than a free
/// string so that the comparison cannot drift away from the message.
///
/// ```
/// assert!(moso_auth::webauthn::CLONE_DETECTED.contains("clone"));
/// ```
pub const CLONE_DETECTED: &str = "the authenticator's signature counter went backwards, which means the credential has been \
     cloned";

/// The `event` field the log line for a detected clone carries.
///
/// A counter regression is the one authentication failure that is not "try
/// again", and what a *client* sees is deliberately the same 401 every other
/// credential failure gets — telling the browser "that key is a copy" would
/// answer a question only the attacker is asking. The log is therefore the
/// channel, and this is the string to alert on: it is stable, it is not
/// localisable, and the message beside it is both.
///
/// ```
/// assert_eq!(moso_auth::webauthn::CLONE_EVENT, "passkey.clone_detected");
/// ```
pub const CLONE_EVENT: &str = "passkey.clone_detected";

/// Whether a failure was the counter regression that means "this credential
/// exists twice".
///
/// Every other ceremony failure is "try again". This one is not: the credential
/// must be disabled and the user notified, because somebody is holding a copy
/// of a private key that was supposed to be unextractable.
///
/// ```
/// use moso_auth::webauthn::{is_clone_detected, CLONE_DETECTED};
/// use moso_auth::Error;
///
/// let cloned = Error::Ceremony { ceremony: "webauthn", reason: CLONE_DETECTED.into() };
/// assert!(is_clone_detected(&cloned));
///
/// let ordinary = Error::Ceremony { ceremony: "webauthn", reason: "signature invalid".into() };
/// assert!(!is_clone_detected(&ordinary));
/// ```
#[must_use]
pub fn is_clone_detected(error: &Error) -> bool {
    matches!(
        error,
        Error::Ceremony {
            ceremony: "webauthn",
            reason,
        } if reason == CLONE_DETECTED
    )
}

/// A ceremony failure, with the reason the *server* logs.
fn ceremony(reason: impl Into<Cow<'static, str>>) -> Error {
    Error::Ceremony {
        ceremony: "webauthn",
        reason: reason.into(),
    }
}

// ---------------------------------------------------------------------------
// Stored credentials
// ---------------------------------------------------------------------------

/// A stored passkey.
///
/// This is the storage schema: one row per credential, with the columns an
/// operator needs to answer "whose is this, what kind of device is it, and when
/// was it last used" without deserialising anything.
///
/// [`record`](Self::record) is the authoritative ceremony state and the only
/// field the verifier reads — with one exception. [`sign_count`](Self::sign_count)
/// overrides the counter inside `record` when the credential is rebuilt, so a
/// store that updates the counter column through
/// [`PasskeyStore::update_counter`] and leaves the record alone is still
/// correct. There is exactly one authoritative counter and it is the column.
///
/// ```no_run
/// use moso_auth::PasskeyCredential;
///
/// # fn f(c: &PasskeyCredential) {
/// let _ = c.sign_count;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PasskeyCredential {
    /// The authenticator's identifier for this credential, base64url.
    pub credential_id: String,
    /// Whose it is.
    pub user_id: String,
    /// The public key, in COSE form.
    ///
    /// Canonical CBOR, exactly as `COSE_Key` is defined by RFC 8152 and used in
    /// the authenticator data — so an auditor, or a verifier that is not this
    /// process, can read it with any COSE library.
    pub public_key: Vec<u8>,
    /// The authenticator's signature counter.
    ///
    /// Clone detection: a counter that goes *backwards* means two devices are
    /// presenting the same credential, which means one of them is a copy. The
    /// credential is disabled and the user is notified. Authenticators that
    /// always report zero are exempt, per the specification.
    pub sign_count: u32,
    /// The authenticator's AAGUID, so an operator can tell a platform key from
    /// a security key.
    ///
    /// `None` unless attestation was conveyed — which, for the passkey
    /// ceremony, it deliberately is not, because asking for attestation on a
    /// consumer sign-in is a privacy problem and a support burden.
    pub aaguid: Option<String>,
    /// Whether this credential can be found without the user naming an account
    /// — what makes the usernameless flow possible.
    ///
    /// Reported by the browser through the `credProps` extension, which is
    /// **unsigned**. Treat it as a hint for the sign-in UI, never as a security
    /// property.
    pub discoverable: bool,
    /// A human label: `"MacBook Touch ID"`.
    pub label: Option<String>,
    /// When it was registered.
    pub created_at: DateTime<Utc>,
    /// When it was last used.
    pub last_used_at: Option<DateTime<Utc>>,
    /// The WebAuthn user handle this credential was registered against,
    /// base64url.
    ///
    /// Sixteen bytes, derived from [`user_id`](Self::user_id) — see
    /// [`WebAuthn::user_handle_for`]. The discoverable flow returns it instead
    /// of an account name, which is why it is stored rather than recomputed:
    /// the derivation may change, the stored handle may not.
    pub user_handle: String,
    /// Whether the ceremony that minted this credential required a
    /// user-verifying gesture.
    ///
    /// `true` means a passkey: one credential, two factors. `false` means a
    /// security key: one factor, to be paired with another.
    pub user_verified: bool,
    /// Whether the private key may exist on more than one device — a synced
    /// passkey rather than one sealed in hardware.
    pub backup_eligible: bool,
    /// Whether it is currently backed up or shared between devices.
    pub backup_state: bool,
    /// The COSE signature algorithm: `-7` is ES256, `-8` EdDSA, `-257` RS256.
    pub algorithm: i64,
    /// The transports the authenticator claims: `"usb"`, `"nfc"`, `"ble"`,
    /// `"internal"`, `"hybrid"`. A hint for the browser's prompt, not a
    /// security property.
    pub transports: Vec<String>,
    /// Set when clone detection fired. A disabled credential is refused before
    /// its signature is even checked.
    pub disabled: bool,
    /// The full ceremony record.
    ///
    /// Opaque, versioned by the ceremony engine, and stored as `jsonb`. Every
    /// other field on this struct is derived from it and exists so that a query
    /// does not have to open it.
    pub record: serde_json::Value,
}

impl PasskeyCredential {
    /// Whether this credential may still be used.
    ///
    /// ```no_run
    /// # use moso_auth::PasskeyCredential;
    /// # fn f(c: &PasskeyCredential) { assert!(c.is_active()); }
    /// ```
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.disabled
    }

    /// Refuse this credential from now on.
    ///
    /// What a route handler calls when [`is_clone_detected`] says the counter
    /// went backwards.
    ///
    /// ```no_run
    /// # use moso_auth::PasskeyCredential;
    /// # fn f(c: &mut PasskeyCredential) { c.disable(); assert!(!c.is_active()); }
    /// ```
    pub fn disable(&mut self) {
        self.disabled = true;
    }

    /// The credential id, decoded.
    ///
    /// # Errors
    ///
    /// [`Error::Ceremony`] when the stored id is not base64url — which means
    /// the row was written by something other than this crate.
    ///
    /// ```no_run
    /// # use moso_auth::PasskeyCredential;
    /// # fn f(c: &PasskeyCredential) -> moso_auth::Result<Vec<u8>> { c.credential_id_bytes() }
    /// ```
    pub fn credential_id_bytes(&self) -> Result<Vec<u8>> {
        B64.decode(&self.credential_id)
            .map_err(|_| ceremony("the stored credential id is not base64url"))
    }

    /// Rebuild the ceremony credential, with the counter column winning.
    fn to_credential(&self) -> Result<Credential> {
        let mut credential: Credential = serde_json::from_value(self.record.clone())
            .map_err(|e| ceremony(format!("the stored credential record does not parse: {e}")))?;
        credential.counter = self.sign_count;
        Ok(credential)
    }

    /// Rebuild as a passkey (user verification required).
    fn to_passkey(&self) -> Result<Passkey> {
        self.to_credential().map(Passkey::from)
    }

    /// Rebuild as a security key (user verification preferred).
    fn to_security_key(&self) -> Result<SecurityKey> {
        self.to_credential().map(SecurityKey::from)
    }

    /// Build the stored row from a freshly registered credential.
    fn from_credential(
        credential: Credential,
        user_id: &str,
        user_handle: &Uuid,
        user_verified: bool,
    ) -> Result<Self> {
        let public_key = cose_key_bytes(&credential.cred)?;
        let algorithm = algorithm_id(&credential.cred)?;
        let aaguid = match &credential.attestation.metadata {
            AttestationMetadata::Packed { aaguid } | AttestationMetadata::Tpm { aaguid, .. } => {
                Some(aaguid.to_string())
            }
            _ => None,
        };
        let discoverable = serde_json::to_value(&credential.extensions)
            .ok()
            .and_then(|v| v.get("cred_props").cloned())
            .and_then(|v| v.get("Set").cloned())
            .and_then(|v| v.get("rk").and_then(serde_json::Value::as_bool))
            .unwrap_or(false);
        let transports = credential
            .transports
            .as_ref()
            .map(|list| {
                list.iter()
                    .filter_map(|t| {
                        serde_json::to_value(t)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            credential_id: B64.encode(credential.cred_id.as_slice()),
            user_id: user_id.to_owned(),
            public_key,
            sign_count: credential.counter,
            aaguid,
            discoverable,
            label: None,
            created_at: Utc::now(),
            last_used_at: None,
            user_handle: B64.encode(user_handle.as_bytes()),
            user_verified,
            backup_eligible: credential.backup_eligible,
            backup_state: credential.backup_state,
            algorithm,
            transports,
            disabled: false,
            record: serde_json::to_value(&credential)
                .map_err(|e| ceremony(format!("the credential does not serialise: {e}")))?,
        })
    }
}

/// The COSE algorithm identifier, as it appears in a `COSE_Key`.
fn algorithm_id(key: &COSEKey) -> Result<i64> {
    use webauthn_rs::prelude::COSEAlgorithm as A;
    Ok(match key.type_ {
        A::ES256 => -7,
        A::ES384 => -35,
        A::ES512 => -36,
        A::EDDSA => -8,
        A::PS256 => -37,
        A::PS384 => -38,
        A::PS512 => -39,
        A::RS256 => -257,
        A::RS384 => -258,
        A::RS512 => -259,
        A::INSECURE_RS1 => {
            return Err(ceremony(
                "the authenticator offered an RSA SHA-1 key, which is not a signature algorithm \
                 this relying party accepts",
            ));
        }
        A::PinUvProtocol => {
            return Err(ceremony(
                "the authenticator reported a PIN protocol identifier where a signature algorithm \
                 was expected",
            ));
        }
    })
}

/// Encode a public key as canonical CBOR `COSE_Key`, per RFC 8152 §7 and the
/// CTAP2 canonical encoding rules: keys sorted by type then value, definite
/// lengths, shortest-form integers, negative labels last.
fn cose_key_bytes(key: &COSEKey) -> Result<Vec<u8>> {
    let alg = algorithm_id(key)?;
    let mut out = Vec::with_capacity(96);

    match &key.key {
        COSEKeyType::EC_EC2(ec2) => {
            // {1: 2 (EC2), 3: alg, -1: crv, -2: x, -3: y}
            cbor_map_header(&mut out, 5);
            cbor_int(&mut out, 1);
            cbor_int(&mut out, 2);
            cbor_int(&mut out, 3);
            cbor_int(&mut out, alg);
            cbor_int(&mut out, -1);
            cbor_int(&mut out, ecdsa_curve_id(&ec2.curve));
            cbor_int(&mut out, -2);
            cbor_bytes(&mut out, ec2.x.as_slice());
            cbor_int(&mut out, -3);
            cbor_bytes(&mut out, ec2.y.as_slice());
        }
        COSEKeyType::EC_OKP(okp) => {
            // {1: 1 (OKP), 3: alg, -1: crv, -2: x}
            cbor_map_header(&mut out, 4);
            cbor_int(&mut out, 1);
            cbor_int(&mut out, 1);
            cbor_int(&mut out, 3);
            cbor_int(&mut out, alg);
            cbor_int(&mut out, -1);
            cbor_int(&mut out, eddsa_curve_id(&okp.curve));
            cbor_int(&mut out, -2);
            cbor_bytes(&mut out, okp.x.as_slice());
        }
        COSEKeyType::RSA(rsa) => {
            // {1: 3 (RSA), 3: alg, -1: n, -2: e}
            cbor_map_header(&mut out, 4);
            cbor_int(&mut out, 1);
            cbor_int(&mut out, 3);
            cbor_int(&mut out, 3);
            cbor_int(&mut out, alg);
            cbor_int(&mut out, -1);
            cbor_bytes(&mut out, rsa.n.as_slice());
            cbor_int(&mut out, -2);
            cbor_bytes(&mut out, &rsa.e);
        }
    }

    Ok(out)
}

/// The COSE curve identifier of an elliptic-curve key (RFC 8152 §13.1).
///
/// Written out rather than cast from the discriminant so that a reordering
/// upstream cannot silently change what is stored in a database column.
fn ecdsa_curve_id(curve: &webauthn_rs::prelude::ECDSACurve) -> i64 {
    use webauthn_rs::prelude::ECDSACurve as C;
    match curve {
        C::SECP256R1 => 1,
        C::SECP384R1 => 2,
        C::SECP521R1 => 3,
    }
}

/// The COSE curve identifier of an octet key pair.
fn eddsa_curve_id(curve: &webauthn_rs::prelude::EDDSACurve) -> i64 {
    use webauthn_rs::prelude::EDDSACurve as C;
    match curve {
        C::ED25519 => 6,
        C::ED448 => 7,
    }
}

/// A definite-length CBOR map header (major type 5).
fn cbor_map_header(out: &mut Vec<u8>, entries: u64) {
    cbor_head(out, 5, entries);
}

/// A CBOR integer, signed (major type 0 or 1).
fn cbor_int(out: &mut Vec<u8>, value: i64) {
    if value >= 0 {
        cbor_head(out, 0, value.unsigned_abs());
    } else {
        // Negative integers are encoded as -1 - n, so n = -1 - value.
        cbor_head(out, 1, (-1 - value).unsigned_abs());
    }
}

/// A CBOR byte string (major type 2).
fn cbor_bytes(out: &mut Vec<u8>, value: &[u8]) {
    cbor_head(out, 2, value.len() as u64);
    out.extend_from_slice(value);
}

/// A CBOR head: the major type in the top three bits, then the shortest
/// encoding of `value`.
fn cbor_head(out: &mut Vec<u8>, major: u8, value: u64) {
    let major = major << 5;
    if value < 24 {
        out.push(major | u8::try_from(value).unwrap_or(23));
    } else if value <= u64::from(u8::MAX) {
        out.push(major | 24);
        out.push(value as u8);
    } else if value <= u64::from(u16::MAX) {
        out.push(major | 25);
        out.extend_from_slice(&(value as u16).to_be_bytes());
    } else if value <= u64::from(u32::MAX) {
        out.push(major | 26);
        out.extend_from_slice(&(value as u32).to_be_bytes());
    } else {
        out.push(major | 27);
        out.extend_from_slice(&value.to_be_bytes());
    }
}

// ---------------------------------------------------------------------------
// Challenges
// ---------------------------------------------------------------------------

/// Which ceremony a challenge belongs to.
///
/// Stored alongside the state so that a registration state cannot be finished
/// as an authentication, and a discoverable state cannot be finished as a
/// non-discoverable one. Both confusions are otherwise silent, and both hand a
/// verified signature to the wrong check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Ceremony {
    /// `start_registration` with user verification required.
    PasskeyRegistration,
    /// `start_registration` with user verification not required.
    SecurityKeyRegistration,
    /// `start_authentication` with a non-empty allow list, verification required.
    PasskeyAuthentication,
    /// `start_authentication` with a non-empty allow list, verification preferred.
    SecurityKeyAuthentication,
    /// `start_authentication` with an empty allow list.
    DiscoverableAuthentication,
}

impl Ceremony {
    /// What the log calls it.
    fn as_str(self) -> &'static str {
        match self {
            Self::PasskeyRegistration => "passkey registration",
            Self::SecurityKeyRegistration => "security-key registration",
            Self::PasskeyAuthentication => "passkey authentication",
            Self::SecurityKeyAuthentication => "security-key authentication",
            Self::DiscoverableAuthentication => "discoverable authentication",
        }
    }
}

/// The tagged, serialised ceremony state that lives in the session.
#[derive(Serialize, Deserialize)]
struct TaggedState {
    /// Which ceremony produced it.
    kind: Ceremony,
    /// Whose ceremony it is, for a registration.
    ///
    /// Recorded when the ceremony *starts*, so that finishing it cannot file
    /// the credential under an account the request names. A registration
    /// response carries no user handle; without this the caller would have to
    /// pass the user id back in, and a caller that passes the wrong one gets a
    /// credential silently attached to somebody else.
    #[serde(default)]
    subject: String,
    /// The engine's own state, opaque here.
    state: serde_json::Value,
}

/// A challenge the browser must sign. Held in the session, never in a cookie of
/// its own.
///
/// The JSON is opaque here and is passed to the browser's WebAuthn API as-is.
/// Modelling it would mean re-encoding the specification into Rust types that
/// then have to track it, for no benefit — the browser is the consumer.
///
/// ```no_run
/// use moso_auth::WebAuthnChallenge;
///
/// # fn f(c: &WebAuthnChallenge) {
/// let _ = &c.options;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WebAuthnChallenge {
    /// What the browser is given.
    pub options: serde_json::Value,
    /// The state the finish step needs, kept server-side.
    pub state: SecretString,
    /// When the challenge stops being accepted.
    pub expires_at: DateTime<Utc>,
}

impl WebAuthnChallenge {
    /// Whether this challenge is past its lifetime.
    ///
    /// Checked by every finish before the signature is, because a challenge
    /// that outlives its window is a replay window: WebAuthn's own `timeout` is
    /// advice to the browser and binds nothing on the server.
    ///
    /// ```no_run
    /// # use moso_auth::WebAuthnChallenge;
    /// # fn f(c: &WebAuthnChallenge) { assert!(!c.has_expired()); }
    /// ```
    #[must_use]
    pub fn has_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }

    /// The options as a JSON string, ready to write into a response body.
    ///
    /// ```no_run
    /// # use moso_auth::WebAuthnChallenge;
    /// # fn f(c: &WebAuthnChallenge) -> String { c.options_json() }
    /// ```
    #[must_use]
    pub fn options_json(&self) -> String {
        self.options.to_string()
    }

    /// Build from an engine state, tagging it with its ceremony.
    fn new<S: Serialize>(
        options: &impl Serialize,
        kind: Ceremony,
        subject: &str,
        state: &S,
        timeout: Duration,
    ) -> Result<Self> {
        let tagged = TaggedState {
            kind,
            subject: subject.to_owned(),
            state: serde_json::to_value(state)
                .map_err(|e| ceremony(format!("the ceremony state does not serialise: {e}")))?,
        };
        Ok(Self {
            options: serde_json::to_value(options)
                .map_err(|e| ceremony(format!("the challenge does not serialise: {e}")))?,
            state: SecretString::new(
                serde_json::to_string(&tagged)
                    .map_err(|e| ceremony(format!("the ceremony state does not serialise: {e}")))?,
            ),
            expires_at: Utc::now()
                + TimeDelta::from_std(timeout).unwrap_or_else(|_| TimeDelta::seconds(60)),
        })
    }

    /// Read the state back, refusing a challenge of the wrong kind or one that
    /// has expired.
    fn open<S: serde::de::DeserializeOwned>(&self, expected: Ceremony) -> Result<S> {
        if self.has_expired() {
            return Err(ceremony("the challenge has expired"));
        }
        let tagged = self.tagged()?;
        if tagged.kind != expected {
            return Err(ceremony(format!(
                "the session holds a {} challenge, which cannot finish this ceremony",
                tagged.kind.as_str()
            )));
        }
        serde_json::from_value(tagged.state)
            .map_err(|_| ceremony("the stored ceremony state does not parse"))
    }

    /// The whole tagged state.
    fn tagged(&self) -> Result<TaggedState> {
        serde_json::from_str::<TaggedState>(self.state.expose())
            .map_err(|_| ceremony("the stored ceremony state does not parse"))
    }

    /// Which ceremony this challenge belongs to.
    fn kind(&self) -> Result<Ceremony> {
        self.tagged().map(|t| t.kind)
    }
}

/// What a completed authentication established.
///
/// [`WebAuthn::finish_authentication`] returns the new counter, which is what a
/// store has to persist. This is the same ceremony with everything else it
/// learned — the backup state of a synced passkey changes over its life, and an
/// application that shows "this key is synced to iCloud" needs to see it move.
///
/// ```no_run
/// use moso_auth::webauthn::PasskeyAssertion;
///
/// # fn f(a: &PasskeyAssertion) {
/// let _ = a.sign_count;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PasskeyAssertion {
    /// Which credential signed, base64url.
    pub credential_id: String,
    /// The counter to store.
    pub sign_count: u32,
    /// Whether the authenticator verified the user this time.
    pub user_verified: bool,
    /// Whether the private key may exist on more than one device.
    pub backup_eligible: bool,
    /// Whether it is currently backed up.
    pub backup_state: bool,
    /// Whether anything about the stored credential changed and should be
    /// written back.
    pub needs_update: bool,
}

/// Who a discoverable credential says it belongs to.
///
/// Returned by [`WebAuthn::identify_discoverable`], which runs *before* the
/// signature is verified: the point of the usernameless flow is that the server
/// does not know which account it is authenticating until the browser says so.
/// Look the credential up by [`credential_id`](Self::credential_id) — that is
/// what [`PasskeyStore::find`] is for — and then finish the ceremony.
///
/// ```no_run
/// use moso_auth::webauthn::DiscoveredCredential;
///
/// # fn f(d: &DiscoveredCredential) {
/// let _ = &d.credential_id;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DiscoveredCredential {
    /// The credential the browser chose, base64url.
    pub credential_id: String,
    /// The user handle it carries, base64url. **Unverified** at this point.
    pub user_handle: String,
}

// ---------------------------------------------------------------------------
// The relying party
// ---------------------------------------------------------------------------

/// The WebAuthn relying party.
///
/// ```no_run
/// use moso_auth::WebAuthn;
///
/// let _ = WebAuthn::new("example.com", "https://app.example.com", "Example");
/// ```
#[derive(Clone, Debug)]
pub struct WebAuthn {
    /// The relying-party id: a registrable domain. Credentials are scoped to
    /// it, so getting it wrong at launch means re-registering every passkey.
    rp_id: String,
    /// The origin the browser must report. Compared exactly.
    origin: String,
    /// The name shown in the authenticator's prompt.
    rp_name: String,
    /// How long a challenge lives.
    timeout: Duration,
    /// Whether to require a user-verifying gesture — a PIN or a biometric —
    /// which is what makes a passkey a *two*-factor credential on its own.
    require_user_verification: bool,
}

impl WebAuthn {
    /// A relying party.
    ///
    /// The ceremony engine is built per call rather than held here, so that a
    /// misconfigured origin is an [`Error::Config`] from a fallible method
    /// instead of a panic from a constructor that cannot return one.
    ///
    /// ```no_run
    /// use moso_auth::WebAuthn;
    ///
    /// let _ = WebAuthn::new("example.com", "https://app.example.com", "Example");
    /// ```
    #[must_use]
    pub fn new(
        rp_id: impl Into<String>,
        origin: impl Into<String>,
        rp_name: impl Into<String>,
    ) -> Self {
        Self {
            rp_id: rp_id.into(),
            origin: origin.into(),
            rp_name: rp_name.into(),
            timeout: DEFAULT_TIMEOUT,
            require_user_verification: true,
        }
    }

    /// Whether to require a user-verifying gesture. On by default.
    ///
    /// This is not a cosmetic flag. It chooses the ceremony:
    ///
    /// | Value | Ceremony | What the credential is |
    /// | --- | --- | --- |
    /// | `true` (default) | passkey | a PIN or a biometric was checked; one credential, two factors |
    /// | `false` | security key | possession only; pair it with a password |
    ///
    /// A credential remembers which ceremony minted it
    /// ([`PasskeyCredential::user_verified`]) and is authenticated with the
    /// matching one, so flipping this later does not invalidate what is already
    /// registered.
    ///
    /// ```no_run
    /// # use moso_auth::WebAuthn;
    /// # fn f(w: WebAuthn) { let _ = w.require_user_verification(false); }
    /// ```
    #[must_use]
    pub fn require_user_verification(mut self, required: bool) -> Self {
        self.require_user_verification = required;
        self
    }

    /// How long a challenge lives. Sixty seconds by default.
    ///
    /// ```no_run
    /// # use moso_auth::WebAuthn;
    /// # use std::time::Duration;
    /// # fn f(w: WebAuthn) { let _ = w.timeout(Duration::from_secs(120)); }
    /// ```
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The relying-party id credentials are scoped to.
    ///
    /// ```
    /// use moso_auth::WebAuthn;
    ///
    /// let rp = WebAuthn::new("example.com", "https://example.com", "Example");
    /// assert_eq!(rp.rp_id(), "example.com");
    /// ```
    #[must_use]
    pub fn rp_id(&self) -> &str {
        &self.rp_id
    }

    /// The origin the browser must report.
    ///
    /// ```
    /// use moso_auth::WebAuthn;
    ///
    /// let rp = WebAuthn::new("example.com", "https://example.com", "Example");
    /// assert_eq!(rp.origin(), "https://example.com");
    /// ```
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The sixteen-byte WebAuthn user handle for an application user id.
    ///
    /// WebAuthn's handle is a fixed-width opaque blob, and an application's user
    /// id is a string of whatever shape it likes. The mapping is: a handle that
    /// already *is* a UUID is used unchanged; anything else becomes the first
    /// sixteen bytes of its SHA-256, tagged as a version-8 (custom) UUID.
    ///
    /// It is deterministic, so re-registering a device for the same account
    /// produces the same handle, and it is one-way, so the handle is not a
    /// user identifier leaking out of the authenticator.
    ///
    /// ```
    /// use moso_auth::WebAuthn;
    ///
    /// let a = WebAuthn::user_handle_for("usr_1");
    /// let b = WebAuthn::user_handle_for("usr_1");
    /// assert_eq!(a, b);
    /// assert_ne!(a, WebAuthn::user_handle_for("usr_2"));
    /// ```
    #[must_use]
    pub fn user_handle_for(user_id: &str) -> Uuid {
        if let Ok(parsed) = Uuid::parse_str(user_id) {
            return parsed;
        }
        let digest = Sha256::digest(user_id.as_bytes());
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        Uuid::from_bytes(
            uuid::Builder::from_custom_bytes(bytes)
                .into_uuid()
                .into_bytes(),
        )
    }

    /// Build the ceremony engine.
    fn engine(&self) -> Result<RelyingParty> {
        let origin = moso_schema::Url::parse_with_schemes(&self.origin, &["https", "http"])
            .map_err(|e| {
                Error::Config(Cow::Owned(format!(
                    "the WebAuthn origin `{}` is not a URL: {e}",
                    self.origin
                )))
            })?
            .into_url();

        WebauthnBuilder::new(&self.rp_id, &origin)
            .and_then(|b| b.rp_name(&self.rp_name).timeout(self.timeout).build())
            .map_err(|e| {
                Error::Config(Cow::Owned(format!(
                    "the WebAuthn relying party is misconfigured: rp_id `{}` and origin `{}` do \
                     not agree ({e:?}); the rp_id must be a registrable suffix of the origin's \
                     host, e.g. rp_id `example.com` for origin `https://app.example.com`",
                    self.rp_id, self.origin
                )))
            })
    }

    /// Begin registering a passkey for a user.
    ///
    /// `existing` becomes the exclude list, so an authenticator that already
    /// holds a credential for this account says so instead of silently making a
    /// second one.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the relying party is misconfigured, and
    /// [`Error::Ceremony`] when the challenge cannot be generated.
    ///
    /// ```no_run
    /// # use moso_auth::{PasskeyCredential, WebAuthn, WebAuthnChallenge};
    /// # fn f(w: &WebAuthn, existing: &[PasskeyCredential]) -> moso_auth::Result<WebAuthnChallenge> {
    /// w.start_registration("usr_1", "ada@example.com", "Ada", existing)
    /// # }
    /// ```
    pub fn start_registration(
        &self,
        user_id: &str,
        user_name: &str,
        display_name: &str,
        existing: &[PasskeyCredential],
    ) -> Result<WebAuthnChallenge> {
        let engine = self.engine()?;
        let handle = Self::user_handle_for(user_id);
        let exclude = exclude_list(existing)?;

        if self.require_user_verification {
            let (options, state) = engine
                .start_passkey_registration(handle, user_name, display_name, exclude)
                .map_err(|e| ceremony(format!("could not start registration: {e:?}")))?;
            WebAuthnChallenge::new(
                &options,
                Ceremony::PasskeyRegistration,
                user_id,
                &state,
                self.timeout,
            )
        } else {
            let (options, state) = engine
                .start_securitykey_registration(
                    handle,
                    user_name,
                    display_name,
                    exclude,
                    None,
                    None,
                )
                .map_err(|e| ceremony(format!("could not start registration: {e:?}")))?;
            WebAuthnChallenge::new(
                &options,
                Ceremony::SecurityKeyRegistration,
                user_id,
                &state,
                self.timeout,
            )
        }
    }

    /// Finish registering, producing the credential to store.
    ///
    /// The credential is attributed to the user named when the ceremony
    /// *started*, read back out of the challenge. A registration response
    /// carries no account of its own, so the alternative would be to trust one
    /// the request supplies — and a request that supplies the wrong one attaches
    /// a working credential to somebody else's account.
    ///
    /// # Errors
    ///
    /// [`Error::Ceremony`] when the attestation does not verify, the origin does
    /// not match, the challenge has expired, or the session is holding a
    /// challenge from a different ceremony.
    ///
    /// ```no_run
    /// # use moso_auth::{PasskeyCredential, WebAuthn, WebAuthnChallenge};
    /// # fn f(w: &WebAuthn, c: &WebAuthnChallenge, r: serde_json::Value)
    /// #     -> moso_auth::Result<PasskeyCredential> {
    /// w.finish_registration(c, &r)
    /// # }
    /// ```
    pub fn finish_registration(
        &self,
        challenge: &WebAuthnChallenge,
        response: &serde_json::Value,
    ) -> Result<PasskeyCredential> {
        let engine = self.engine()?;
        let registration: RegisterPublicKeyCredential = serde_json::from_value(response.clone())
            .map_err(|e| ceremony(format!("the registration response does not parse: {e}")))?;

        let tagged = challenge.tagged()?;
        let credential: Credential = match tagged.kind {
            Ceremony::PasskeyRegistration => {
                let state: PasskeyRegistration = challenge.open(Ceremony::PasskeyRegistration)?;
                engine
                    .finish_passkey_registration(&registration, &state)
                    .map_err(register_failure)?
                    .into()
            }
            Ceremony::SecurityKeyRegistration => {
                let state: SecurityKeyRegistration =
                    challenge.open(Ceremony::SecurityKeyRegistration)?;
                engine
                    .finish_securitykey_registration(&registration, &state)
                    .map_err(register_failure)?
                    .into()
            }
            other => {
                return Err(ceremony(format!(
                    "the session holds a {} challenge, which is not a registration",
                    other.as_str()
                )));
            }
        };

        // The engine enforced the policy the ceremony asked for; this records
        // which one it was, so authentication picks the same ceremony.
        let user_verified = tagged.kind == Ceremony::PasskeyRegistration;
        PasskeyCredential::from_credential(
            credential,
            &tagged.subject,
            &Self::user_handle_for(&tagged.subject),
            user_verified,
        )
    }

    /// Finish registering, asserting whose credential it is.
    ///
    /// The same work as [`finish_registration`](Self::finish_registration) with
    /// the account the caller *believes* it is registering for checked against
    /// the one the ceremony recorded. Worth spelling out in a handler that
    /// reads the session and the challenge from two different places, because
    /// the failure it catches — the two disagreeing — is otherwise invisible.
    ///
    /// # Errors
    ///
    /// [`Error::Ceremony`], as [`finish_registration`](Self::finish_registration),
    /// plus a subject mismatch.
    ///
    /// ```no_run
    /// # use moso_auth::{PasskeyCredential, WebAuthn, WebAuthnChallenge};
    /// # fn f(w: &WebAuthn, c: &WebAuthnChallenge, r: serde_json::Value)
    /// #     -> moso_auth::Result<PasskeyCredential> {
    /// w.finish_registration_for("usr_1", c, &r)
    /// # }
    /// ```
    pub fn finish_registration_for(
        &self,
        user_id: &str,
        challenge: &WebAuthnChallenge,
        response: &serde_json::Value,
    ) -> Result<PasskeyCredential> {
        let recorded = challenge.tagged()?.subject;
        if recorded != user_id {
            return Err(ceremony(
                "the challenge in the session was issued for a different account",
            ));
        }
        self.finish_registration(challenge, response)
    }

    /// Begin authenticating.
    ///
    /// `allow` empty means the discoverable-credential flow: the browser offers
    /// whatever it has for this relying party and the user never types an
    /// identifier. That flow is the reason passkeys feel different from every
    /// other second factor, and it is why it is the default when no credentials
    /// are named.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the relying party is misconfigured and
    /// [`Error::Ceremony`] when the challenge cannot be generated or every
    /// credential offered is disabled.
    ///
    /// ```no_run
    /// # use moso_auth::{PasskeyCredential, WebAuthn, WebAuthnChallenge};
    /// # fn f(w: &WebAuthn, a: &[PasskeyCredential]) -> moso_auth::Result<WebAuthnChallenge> {
    /// w.start_authentication(a)
    /// # }
    /// ```
    pub fn start_authentication(&self, allow: &[PasskeyCredential]) -> Result<WebAuthnChallenge> {
        let engine = self.engine()?;

        if allow.is_empty() {
            let (mut options, state) = engine
                .start_discoverable_authentication()
                .map_err(|e| ceremony(format!("could not start authentication: {e:?}")))?;
            // `start_discoverable_authentication` forces conditional mediation,
            // which is the autofill flow. An empty allow list is also how a
            // "sign in with a passkey" *button* works, and that one must not
            // ask for conditional mediation or the browser shows nothing.
            // `start_conditional_authentication` is the autofill spelling.
            options.mediation = None;
            return WebAuthnChallenge::new(
                &options,
                Ceremony::DiscoverableAuthentication,
                "",
                &state,
                self.timeout,
            );
        }

        let usable: Vec<&PasskeyCredential> = allow.iter().filter(|c| c.is_active()).collect();
        if usable.is_empty() {
            return Err(ceremony(
                "every credential offered for this account is disabled",
            ));
        }

        // A credential remembers the ceremony that minted it. Mixing the two in
        // one challenge would mean one policy for both, so the stricter set
        // wins and the caller is told when that dropped something.
        let verified: Vec<&PasskeyCredential> =
            usable.iter().copied().filter(|c| c.user_verified).collect();

        if verified.is_empty() {
            let keys = usable
                .iter()
                .map(|c| c.to_security_key())
                .collect::<Result<Vec<_>>>()?;
            let (options, state) = engine
                .start_securitykey_authentication(&keys)
                .map_err(|e| ceremony(format!("could not start authentication: {e:?}")))?;
            WebAuthnChallenge::new(
                &options,
                Ceremony::SecurityKeyAuthentication,
                "",
                &state,
                self.timeout,
            )
        } else {
            let keys = verified
                .iter()
                .map(|c| c.to_passkey())
                .collect::<Result<Vec<_>>>()?;
            let (options, state) = engine
                .start_passkey_authentication(&keys)
                .map_err(|e| ceremony(format!("could not start authentication: {e:?}")))?;
            WebAuthnChallenge::new(
                &options,
                Ceremony::PasskeyAuthentication,
                "",
                &state,
                self.timeout,
            )
        }
    }

    /// Begin a conditional-UI ("autofill") authentication.
    ///
    /// The same discoverable ceremony as
    /// [`start_authentication`](Self::start_authentication) with an empty allow
    /// list, but with `mediation: "conditional"` set, which is what makes a
    /// browser offer passkeys from the username field instead of from a button.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] and [`Error::Ceremony`], as
    /// [`start_authentication`](Self::start_authentication).
    ///
    /// ```no_run
    /// # use moso_auth::{WebAuthn, WebAuthnChallenge};
    /// # fn f(w: &WebAuthn) -> moso_auth::Result<WebAuthnChallenge> {
    /// w.start_conditional_authentication()
    /// # }
    /// ```
    pub fn start_conditional_authentication(&self) -> Result<WebAuthnChallenge> {
        let engine = self.engine()?;
        let (options, state) = engine
            .start_discoverable_authentication()
            .map_err(|e| ceremony(format!("could not start authentication: {e:?}")))?;
        WebAuthnChallenge::new(
            &options,
            Ceremony::DiscoverableAuthentication,
            "",
            &state,
            self.timeout,
        )
    }

    /// Which credential a discoverable response used, before verifying it.
    ///
    /// The usernameless flow's missing step: the server has a signature and no
    /// idea whose account it belongs to. Look the credential up with
    /// [`PasskeyStore::find`], then call
    /// [`finish_authentication`](Self::finish_authentication) with it.
    ///
    /// **Nothing here is verified.** The credential id and user handle are
    /// unauthenticated client input until the ceremony finishes.
    ///
    /// # Errors
    ///
    /// [`Error::Ceremony`] when the response does not parse or carries no user
    /// handle — a credential without one is not discoverable, and finishing it
    /// as though it were would skip the handle check.
    ///
    /// ```no_run
    /// # use moso_auth::WebAuthn;
    /// # use moso_auth::webauthn::DiscoveredCredential;
    /// # fn f(w: &WebAuthn, r: serde_json::Value) -> moso_auth::Result<DiscoveredCredential> {
    /// w.identify_discoverable(&r)
    /// # }
    /// ```
    pub fn identify_discoverable(
        &self,
        response: &serde_json::Value,
    ) -> Result<DiscoveredCredential> {
        let assertion: PublicKeyCredential = serde_json::from_value(response.clone())
            .map_err(|e| ceremony(format!("the authentication response does not parse: {e}")))?;

        let user_handle = assertion.get_user_unique_id().ok_or_else(|| {
            ceremony(
                "the authentication response carries no user handle, so the credential is not \
                 discoverable",
            )
        })?;

        Ok(DiscoveredCredential {
            credential_id: B64.encode(assertion.get_credential_id()),
            user_handle: B64.encode(user_handle),
        })
    }

    /// Finish authenticating, returning the credential's new counter.
    ///
    /// # Errors
    ///
    /// [`Error::Ceremony`] when the signature does not verify, the origin does
    /// not match, the challenge has expired, or the **counter went backwards** —
    /// which means the credential has been cloned, and is reported distinctly
    /// (see [`is_clone_detected`]) so the caller can disable it and notify.
    ///
    /// ```no_run
    /// # use moso_auth::{PasskeyCredential, WebAuthn, WebAuthnChallenge};
    /// # fn f(w: &WebAuthn, c: &WebAuthnChallenge, r: serde_json::Value,
    /// #      k: &PasskeyCredential) -> moso_auth::Result<u32> {
    /// w.finish_authentication(c, &r, k)
    /// # }
    /// ```
    pub fn finish_authentication(
        &self,
        challenge: &WebAuthnChallenge,
        response: &serde_json::Value,
        credential: &PasskeyCredential,
    ) -> Result<u32> {
        self.assert(challenge, response, credential)
            .map(|a| a.sign_count)
    }

    /// Finish authenticating, with everything the ceremony established.
    ///
    /// [`finish_authentication`](Self::finish_authentication) narrowed to the
    /// counter; this is the same work with the backup state and the
    /// "write this credential back" flag the store needs.
    ///
    /// # Errors
    ///
    /// As [`finish_authentication`](Self::finish_authentication).
    ///
    /// ```no_run
    /// # use moso_auth::{PasskeyCredential, WebAuthn, WebAuthnChallenge};
    /// # use moso_auth::webauthn::PasskeyAssertion;
    /// # fn f(w: &WebAuthn, c: &WebAuthnChallenge, r: serde_json::Value,
    /// #      k: &PasskeyCredential) -> moso_auth::Result<PasskeyAssertion> {
    /// w.assert(c, &r, k)
    /// # }
    /// ```
    pub fn assert(
        &self,
        challenge: &WebAuthnChallenge,
        response: &serde_json::Value,
        credential: &PasskeyCredential,
    ) -> Result<PasskeyAssertion> {
        if credential.disabled {
            return Err(ceremony(
                "this credential is disabled; it was cloned, or an operator revoked it",
            ));
        }

        let engine = self.engine()?;
        let assertion: PublicKeyCredential = serde_json::from_value(response.clone())
            .map_err(|e| ceremony(format!("the authentication response does not parse: {e}")))?;

        let presented = B64.encode(assertion.get_credential_id());
        if presented != credential.credential_id {
            return Err(ceremony(
                "the response was signed by a different credential from the one supplied",
            ));
        }

        let result: AuthenticationResult = match challenge.kind()? {
            Ceremony::PasskeyAuthentication => {
                let state: PasskeyAuthentication =
                    challenge.open(Ceremony::PasskeyAuthentication)?;
                engine
                    .finish_passkey_authentication(&assertion, &state)
                    .map_err(authenticate_failure)?
            }
            Ceremony::SecurityKeyAuthentication => {
                let state: SecurityKeyAuthentication =
                    challenge.open(Ceremony::SecurityKeyAuthentication)?;
                engine
                    .finish_securitykey_authentication(&assertion, &state)
                    .map_err(authenticate_failure)?
            }
            Ceremony::DiscoverableAuthentication => {
                let state: DiscoverableAuthentication =
                    challenge.open(Ceremony::DiscoverableAuthentication)?;
                let handle = assertion
                    .get_user_unique_id()
                    .ok_or_else(|| ceremony("the discoverable response carries no user handle"))?;
                if B64.encode(handle) != credential.user_handle {
                    return Err(ceremony(
                        "the user handle in the response does not belong to the credential it \
                         names",
                    ));
                }
                let key: DiscoverableKey = credential.to_passkey()?.into();
                engine
                    .finish_discoverable_authentication(&assertion, state, &[key])
                    .map_err(authenticate_failure)?
            }
            other => {
                return Err(ceremony(format!(
                    "the session holds a {} challenge, which is not an authentication",
                    other.as_str()
                )));
            }
        };

        Ok(PasskeyAssertion {
            credential_id: B64.encode(result.cred_id().as_slice()),
            sign_count: result.counter(),
            user_verified: result.user_verified(),
            backup_eligible: result.backup_eligible(),
            backup_state: result.backup_state(),
            needs_update: result.needs_update(),
        })
    }
}

/// The exclude list for a registration.
fn exclude_list(existing: &[PasskeyCredential]) -> Result<Option<Vec<CredentialID>>> {
    if existing.is_empty() {
        return Ok(None);
    }
    existing
        .iter()
        .map(|c| c.credential_id_bytes().map(CredentialID::from))
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

/// Registration failures, named for the log.
fn register_failure(error: webauthn_rs::prelude::WebauthnError) -> Error {
    use webauthn_rs::prelude::WebauthnError as W;
    ceremony(match error {
        W::UserNotVerified => Cow::Borrowed(
            "the authenticator did not verify the user, and this relying party requires a PIN or \
             a biometric",
        ),
        W::CredentialAlreadyExists => {
            Cow::Borrowed("this authenticator already holds a credential for this account")
        }
        W::InvalidRPOrigin => Cow::Borrowed("the browser reported a different origin"),
        W::ChallengeNotFound | W::MismatchedChallenge => {
            Cow::Borrowed("the challenge does not match the one this session issued")
        }
        other => Cow::Owned(format!("the attestation did not verify: {other:?}")),
    })
}

/// Authentication failures, with the clone case reported distinctly.
fn authenticate_failure(error: webauthn_rs::prelude::WebauthnError) -> Error {
    use webauthn_rs::prelude::WebauthnError as W;
    match error {
        W::CredentialPossibleCompromise => ceremony(CLONE_DETECTED),
        W::UserNotVerified => ceremony(
            "the authenticator did not verify the user, and this credential was registered as a \
             verifying one",
        ),
        W::InvalidRPOrigin => ceremony("the browser reported a different origin"),
        W::ChallengeNotFound | W::MismatchedChallenge => {
            ceremony("the challenge does not match the one this session issued")
        }
        other => ceremony(format!("the signature did not verify: {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Where passkeys live.
///
/// Six operations, and each one is a decision rather than a convenience:
///
/// | Operation | Why it is on the trait |
/// | --- | --- |
/// | [`insert`](Self::insert) | the write a registration makes, refusing a credential id already on record |
/// | [`find`](Self::find) | by credential id **alone**, which is what the usernameless flow rests on |
/// | [`list_for_user`](Self::list_for_user) | the exclude list, and the "your keys" page |
/// | [`update_counter`](Self::update_counter) | the counter column is authoritative, so it is written on its own |
/// | [`disable`](Self::disable) | clone detection has to be able to quarantine a row |
/// | [`delete`](Self::delete) | a user removing a key they no longer own |
///
/// [`MemoryPasskeyStore`](crate::store::MemoryPasskeyStore) and
/// [`TablePasskeyStore`](crate::store::TablePasskeyStore) are the shipped
/// implementations, and `store::conformance` runs one suite against both, so the
/// shape a third one has to reproduce is written down as a test rather than as
/// prose.
///
/// ```no_run
/// use moso_auth::{PasskeyCredential, PasskeyStore};
///
/// async fn find(store: &dyn PasskeyStore, id: &str)
///     -> moso_auth::Result<Option<PasskeyCredential>>
/// {
///     store.find(id).await
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot store passkeys",
    label = "not a passkey store",
    note = "a passkey store implements `insert`, `find`, `list_for_user`, `update_counter`, \
            `disable` and `delete`",
    note = "help: `find` is by credential id and must work without a user id — that is what \
            makes the usernameless flow possible",
    note = "help: `moso_auth::store::{{MemoryPasskeyStore, TablePasskeyStore}}` are shipped; \
            pass `store::descriptors()` to `moso db make-migration` for the table"
)]
pub trait PasskeyStore: Send + Sync + 'static {
    /// Store a newly registered credential.
    ///
    /// The credential id is a primary key: a second insert of one already on
    /// record is refused rather than written over, because overwriting would
    /// move a credential somebody else holds onto a new account.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn insert<'a>(&'a self, credential: &'a PasskeyCredential) -> BoxFuture<'a, Result<()>>;

    /// Find a credential by its identifier, without knowing the user.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn find<'a>(
        &'a self,
        credential_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<PasskeyCredential>>>;

    /// Every credential a user has.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn list_for_user<'a>(
        &'a self,
        user_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<PasskeyCredential>>>;

    /// Record a new signature counter after a successful authentication.
    ///
    /// The counter column is the authoritative one, so this is the write a
    /// successful assertion makes and it is deliberately narrower than
    /// [`insert`](Self::insert): it touches one credential's counter and leaves
    /// the opaque ceremony record alone.
    ///
    /// It writes what it is told, in both directions. A counter that went
    /// *backwards* never reaches here from a verified ceremony, because
    /// [`WebAuthn::assert`] refuses it first, so a store that clamped would be
    /// hiding a caller that skipped the verifier rather than protecting
    /// anybody. A credential that is not on record is not an error either: the
    /// same `UPDATE … WHERE credential_id = $1` a table would run affects no
    /// rows and says nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn update_counter<'a>(
        &'a self,
        credential_id: &'a str,
        sign_count: u32,
    ) -> BoxFuture<'a, Result<()>>;

    /// Take a credential out of service. `Ok(false)` when there was nothing to
    /// disable, or it was already disabled.
    ///
    /// What clone detection calls: a signature counter that went backwards means
    /// two devices are presenting one private key, and the correct response is to
    /// refuse *both* until a person has looked, not to let the next attempt with
    /// a plausible counter through. It is a compare-and-set, so two requests
    /// quarantining one credential produce one `true` and one `false` rather
    /// than two alerts for one event.
    ///
    /// The row is **kept**, not deleted, for the same reason a revoked API key
    /// is: an audit that cannot resolve a credential id is not an audit, and the
    /// user has to be told which of their keys stopped working and why.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn disable<'a>(&'a self, credential_id: &'a str) -> BoxFuture<'a, Result<bool>>;

    /// Remove a credential. `Ok(false)` when there was nothing to remove.
    ///
    /// For a user deleting a key they no longer have. A *cloned* key is
    /// [`disable`](Self::disable)d instead, because deleting it destroys the
    /// evidence and leaves the user with no explanation for a key that stopped
    /// working.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    fn delete<'a>(&'a self, credential_id: &'a str) -> BoxFuture<'a, Result<bool>>;
}

/// The virtual authenticator the crate's own tests drive, and the relying party
/// it is paired with.
///
/// It sits outside `mod tests` because two modules need it: this file's
/// ceremony tests, and `routes::passkeys`'s round trip through the mounted
/// routes. A second copy would be free to drift, and the copy that drifted
/// would be the one proving that the route wiring works.
#[cfg(test)]
pub(crate) mod testing {
    use webauthn_authenticator_rs::WebauthnAuthenticator;
    use webauthn_authenticator_rs::softpasskey::SoftPasskey;
    use webauthn_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse, Url as RpUrl};

    use super::{PasskeyCredential, WebAuthn, WebAuthnChallenge};

    /// The relying-party id every test registers against.
    pub(crate) const RP_ID: &str = "example.com";

    /// The origin the virtual browser reports.
    pub(crate) const ORIGIN: &str = "https://example.com";

    /// The relying party under test.
    pub(crate) fn relying_party() -> WebAuthn {
        WebAuthn::new(RP_ID, ORIGIN, "Example")
    }

    /// [`ORIGIN`], as the authenticator wants it.
    fn origin() -> RpUrl {
        RpUrl::parse(ORIGIN).expect("the test origin parses")
    }

    /// Name one credential in a request's allow list.
    ///
    /// `AllowCredentials` is not re-exported by `webauthn-rs`, and the JSON is
    /// the interface the browser sees anyway, so the edit is made there.
    pub(crate) fn with_allow_list(
        challenge: &WebAuthnChallenge,
        credential: &PasskeyCredential,
    ) -> WebAuthnChallenge {
        let mut options = challenge.options.clone();
        options["publicKey"]["allowCredentials"] = serde_json::json!([{
            "type": "public-key",
            "id": credential.credential_id,
        }]);
        WebAuthnChallenge {
            options,
            state: challenge.state.clone(),
            expires_at: challenge.expires_at,
        }
    }

    /// A browser: takes the challenge JSON, drives a virtual authenticator,
    /// hands back the response JSON. Exactly what the client does, minus the
    /// user interface.
    pub(crate) struct VirtualBrowser {
        /// The soft authenticator holding this browser's key material.
        authenticator: WebauthnAuthenticator<SoftPasskey>,
    }

    impl VirtualBrowser {
        /// `falsify_uv` is the soft authenticator's stand-in for a PIN prompt:
        /// there is no user to verify, so it asserts that it did.
        pub(crate) fn new() -> Self {
            Self {
                authenticator: WebauthnAuthenticator::new(SoftPasskey::new(true)),
            }
        }

        /// Answer a creation challenge.
        pub(crate) fn register(&mut self, challenge: &WebAuthnChallenge) -> serde_json::Value {
            let options: CreationChallengeResponse =
                serde_json::from_value(challenge.options.clone())
                    .expect("the creation options round-trip");
            let response = self
                .authenticator
                .do_registration(origin(), options)
                .expect("the virtual authenticator registers");
            serde_json::to_value(response).expect("the registration response serialises")
        }

        /// Answer a request challenge.
        pub(crate) fn authenticate(&mut self, challenge: &WebAuthnChallenge) -> serde_json::Value {
            let options: RequestChallengeResponse =
                serde_json::from_value(challenge.options.clone())
                    .expect("the request options round-trip");
            let response = self
                .authenticator
                .do_authentication(origin(), options)
                .expect("the virtual authenticator signs");
            serde_json::to_value(response).expect("the assertion serialises")
        }
    }
}

#[cfg(test)]
mod tests {
    use webauthn_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse};

    use super::testing::{VirtualBrowser, relying_party, with_allow_list};
    use super::*;
    use crate::store::MemoryPasskeyStore;

    /// The relying party under test, spelled the way every case here reads.
    fn rp() -> WebAuthn {
        relying_party()
    }

    // -----------------------------------------------------------------------
    // The round trip
    // -----------------------------------------------------------------------

    /// The acceptance criterion from `docs/03-batteries/30-auth.md`: a passkey
    /// registration and authentication round trip against a virtual
    /// authenticator.
    #[test]
    fn a_passkey_registers_and_then_authenticates() {
        let rp = rp();
        let mut browser = VirtualBrowser::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&challenge);
        let credential = rp
            .finish_registration_for("usr_1", &challenge, &response)
            .expect("registration finishes");

        assert_eq!(credential.user_id, "usr_1");
        assert!(
            credential.user_verified,
            "the default ceremony is the verifying one"
        );
        assert!(!credential.credential_id.is_empty());
        assert!(
            !credential.public_key.is_empty(),
            "the COSE key must be stored"
        );
        assert_eq!(
            credential.algorithm, -7,
            "the soft authenticator signs with ES256"
        );
        assert!(credential.is_active());

        let challenge = rp
            .start_authentication(std::slice::from_ref(&credential))
            .expect("authentication starts");
        let response = browser.authenticate(&challenge);
        let counter = rp
            .finish_authentication(&challenge, &response, &credential)
            .expect("authentication finishes");

        assert!(
            counter > credential.sign_count,
            "the authenticator's counter must advance: {counter} <= {}",
            credential.sign_count
        );
    }

    /// The same round trip through the richer return, because that is what a
    /// store actually persists.
    #[test]
    fn an_assertion_reports_what_the_store_has_to_write_back() {
        let rp = rp();
        let mut browser = VirtualBrowser::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&challenge);
        let credential = rp
            .finish_registration_for("usr_1", &challenge, &response)
            .expect("registration finishes");

        let challenge = rp
            .start_authentication(std::slice::from_ref(&credential))
            .expect("authentication starts");
        let response = browser.authenticate(&challenge);
        let assertion = rp
            .assert(&challenge, &response, &credential)
            .expect("authentication finishes");

        assert_eq!(assertion.credential_id, credential.credential_id);
        assert!(assertion.user_verified);
        assert!(assertion.needs_update, "the counter advanced");
    }

    /// A second registration on the same authenticator is refused, because the
    /// first credential is in the exclude list. Without it a user quietly
    /// accumulates duplicate keys they cannot tell apart.
    #[test]
    fn an_already_registered_authenticator_is_excluded() {
        let rp = rp();
        let mut browser = VirtualBrowser::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&challenge);
        let first = rp
            .finish_registration_for("usr_1", &challenge, &response)
            .expect("registration finishes");

        let challenge = rp
            .start_registration(
                "usr_1",
                "ada@example.com",
                "Ada",
                std::slice::from_ref(&first),
            )
            .expect("the second registration starts");
        let options: CreationChallengeResponse =
            serde_json::from_value(challenge.options.clone()).expect("options round-trip");
        assert_eq!(
            options
                .public_key
                .exclude_credentials
                .as_ref()
                .map(Vec::len)
                .unwrap_or(0),
            1,
            "the credential already registered must be excluded"
        );
    }

    // -----------------------------------------------------------------------
    // Clone detection
    // -----------------------------------------------------------------------

    /// The counter going backwards is the signal that two devices hold the same
    /// private key. It must be reported *distinctly*, because the response is
    /// not "try again" — it is "disable this credential".
    #[test]
    fn a_counter_regression_is_reported_as_a_clone() {
        let rp = rp();
        let mut browser = VirtualBrowser::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&challenge);
        let mut credential = rp
            .finish_registration_for("usr_1", &challenge, &response)
            .expect("registration finishes");

        // Two successful authentications, so the stored counter is behind the
        // authenticator's by one.
        for _ in 0..2 {
            let challenge = rp
                .start_authentication(std::slice::from_ref(&credential))
                .expect("authentication starts");
            let response = browser.authenticate(&challenge);
            credential.sign_count = rp
                .finish_authentication(&challenge, &response, &credential)
                .expect("authentication finishes");
        }

        // Now pretend the store is *ahead* — which is exactly what a clone
        // looks like: the other copy already used a higher counter.
        credential.sign_count += 5;

        let challenge = rp
            .start_authentication(std::slice::from_ref(&credential))
            .expect("authentication starts");
        let response = browser.authenticate(&challenge);
        let error = rp
            .finish_authentication(&challenge, &response, &credential)
            .expect_err("a counter regression must not authenticate");

        assert!(
            is_clone_detected(&error),
            "a counter regression must be distinguishable: {error}"
        );
    }

    /// A credential that clone detection disabled is refused before its
    /// signature is even looked at.
    #[test]
    fn a_disabled_credential_never_authenticates() {
        let rp = rp();
        let mut browser = VirtualBrowser::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&challenge);
        let mut credential = rp
            .finish_registration_for("usr_1", &challenge, &response)
            .expect("registration finishes");

        let challenge = rp
            .start_authentication(std::slice::from_ref(&credential))
            .expect("authentication starts");
        let response = browser.authenticate(&challenge);

        credential.disable();
        let error = rp
            .finish_authentication(&challenge, &response, &credential)
            .expect_err("a disabled credential must not authenticate");
        assert!(format!("{error}").contains("disabled"), "{error}");

        // …and it cannot even start one.
        let error = rp
            .start_authentication(std::slice::from_ref(&credential))
            .expect_err("a disabled credential offers nothing to sign with");
        assert!(format!("{error}").contains("disabled"), "{error}");
    }

    // -----------------------------------------------------------------------
    // Replay and confusion
    // -----------------------------------------------------------------------

    /// An expired challenge is refused before the signature is verified.
    /// WebAuthn's own `timeout` is advice to the browser; nothing in the
    /// protocol stops a captured challenge from being replayed an hour later.
    #[test]
    fn an_expired_challenge_is_refused() {
        let rp = rp();
        let mut browser = VirtualBrowser::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&challenge);

        let mut stale = challenge.clone();
        stale.expires_at = Utc::now() - TimeDelta::seconds(1);

        let error = rp
            .finish_registration(&stale, &response)
            .expect_err("an expired challenge must not finish");
        assert!(format!("{error}").contains("expired"), "{error}");
    }

    /// A registration state handed to `finish_authentication` is a type
    /// confusion with a real signature attached. The tag makes it a refusal.
    #[test]
    fn a_registration_challenge_cannot_finish_an_authentication() {
        let rp = rp();
        let mut browser = VirtualBrowser::new();

        let registration = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&registration);
        let credential = rp
            .finish_registration_for("usr_1", &registration, &response)
            .expect("registration finishes");

        let authentication = rp
            .start_authentication(std::slice::from_ref(&credential))
            .expect("authentication starts");
        let assertion = browser.authenticate(&authentication);

        let error = rp
            .finish_authentication(&registration, &assertion, &credential)
            .expect_err("a registration challenge is not an authentication");
        assert!(format!("{error}").contains("registration"), "{error}");
    }

    /// A challenge from one ceremony cannot be replayed into another: the
    /// engine's own challenge comparison catches it, and the response is a
    /// refusal rather than a verified signature filed under the wrong session.
    #[test]
    fn an_assertion_for_another_challenge_is_refused() {
        let rp = rp();
        let mut browser = VirtualBrowser::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&challenge);
        let credential = rp
            .finish_registration_for("usr_1", &challenge, &response)
            .expect("registration finishes");

        let first = rp
            .start_authentication(std::slice::from_ref(&credential))
            .expect("authentication starts");
        let second = rp
            .start_authentication(std::slice::from_ref(&credential))
            .expect("a second authentication starts");
        let signed_for_first = browser.authenticate(&first);

        let error = rp
            .finish_authentication(&second, &signed_for_first, &credential)
            .expect_err("a response signed for another challenge must be refused");
        assert!(format!("{error}").contains("challenge"), "{error}");
    }

    /// A response signed by a *different* credential than the one the caller
    /// looked up is refused, so a confused route handler cannot advance the
    /// wrong row's counter.
    #[test]
    fn a_response_from_another_credential_is_refused() {
        let rp = rp();
        let mut ada = VirtualBrowser::new();
        let mut bob = VirtualBrowser::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = ada.register(&challenge);
        let ada_credential = rp
            .finish_registration_for("usr_1", &challenge, &response)
            .expect("registration finishes");

        let challenge = rp
            .start_registration("usr_2", "bob@example.com", "Bob", &[])
            .expect("registration starts");
        let response = bob.register(&challenge);
        let bob_credential = rp
            .finish_registration_for("usr_2", &challenge, &response)
            .expect("registration finishes");

        let challenge = rp
            .start_authentication(std::slice::from_ref(&bob_credential))
            .expect("authentication starts");
        let response = bob.authenticate(&challenge);

        let error = rp
            .finish_authentication(&challenge, &response, &ada_credential)
            .expect_err("Bob's signature must not authenticate Ada's credential");
        assert!(
            format!("{error}").contains("different credential"),
            "{error}"
        );
    }

    // -----------------------------------------------------------------------
    // The discoverable (usernameless) flow
    // -----------------------------------------------------------------------

    /// The usernameless round trip: an empty allow list, a browser that reports
    /// which credential it chose, a store lookup by credential id, and a finish
    /// that checks the user handle belongs to that credential.
    ///
    /// The virtual authenticator does not populate `userHandle` — no soft
    /// authenticator in the crate does — so the handle a compliant browser
    /// returns for a resident credential is filled in here. Everything else,
    /// including the signature, is the authenticator's own.
    #[tokio::test]
    async fn a_discoverable_credential_authenticates_without_a_username() {
        let rp = rp();
        let mut browser = VirtualBrowser::new();
        let store = MemoryPasskeyStore::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&challenge);
        let credential = rp
            .finish_registration_for("usr_1", &challenge, &response)
            .expect("registration finishes");
        store.insert(&credential).await.expect("the store takes it");

        // The user names no account: an empty allow list.
        let challenge = rp
            .start_authentication(&[])
            .expect("a discoverable authentication starts");
        let options: RequestChallengeResponse =
            serde_json::from_value(challenge.options.clone()).expect("options round-trip");
        assert!(
            options.public_key.allow_credentials.is_empty(),
            "a discoverable request names no credential"
        );
        assert!(
            options.mediation.is_none(),
            "the button flow must not ask for conditional mediation"
        );

        // The browser has to be told which credential to use, because
        // `SoftPasskey` has no resident-key store; the assertion itself is real.
        let mut response = browser.authenticate(&with_allow_list(&challenge, &credential));
        response["response"]["userHandle"] =
            serde_json::Value::String(credential.user_handle.clone());

        // …and now the server, which knows nothing yet.
        let discovered = rp
            .identify_discoverable(&response)
            .expect("the response identifies itself");
        assert_eq!(discovered.credential_id, credential.credential_id);
        assert_eq!(discovered.user_handle, credential.user_handle);

        let found = store
            .find(&discovered.credential_id)
            .await
            .expect("the store answers")
            .expect("the store finds it by credential id alone");
        assert_eq!(found.user_id, "usr_1");

        let assertion = rp
            .assert(&challenge, &response, &found)
            .expect("the discoverable ceremony finishes");
        assert_eq!(assertion.credential_id, credential.credential_id);
    }

    /// A discoverable response whose user handle belongs to someone else is
    /// refused. Without this check the usernameless flow authenticates the
    /// account the *attacker* named, with a signature from a credential they
    /// legitimately hold.
    #[test]
    fn a_mismatched_user_handle_is_refused() {
        let rp = rp();
        let mut browser = VirtualBrowser::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&challenge);
        let credential = rp
            .finish_registration_for("usr_1", &challenge, &response)
            .expect("registration finishes");

        let challenge = rp
            .start_authentication(&[])
            .expect("a discoverable authentication starts");
        let mut response = browser.authenticate(&with_allow_list(&challenge, &credential));
        // Somebody else's handle.
        response["response"]["userHandle"] =
            serde_json::Value::String(B64.encode(WebAuthn::user_handle_for("usr_2").as_bytes()));

        let error = rp
            .assert(&challenge, &response, &credential)
            .expect_err("a handle that does not belong to the credential must be refused");
        assert!(format!("{error}").contains("user handle"), "{error}");
    }

    /// A response with no user handle at all is not a discoverable credential,
    /// and saying so is better than guessing.
    #[test]
    fn identify_refuses_a_response_without_a_user_handle() {
        let rp = rp();
        let mut browser = VirtualBrowser::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&challenge);
        let credential = rp
            .finish_registration_for("usr_1", &challenge, &response)
            .expect("registration finishes");

        let challenge = rp
            .start_authentication(std::slice::from_ref(&credential))
            .expect("authentication starts");
        let response = browser.authenticate(&challenge);

        let error = rp
            .identify_discoverable(&response)
            .expect_err("no user handle means not discoverable");
        assert!(format!("{error}").contains("user handle"), "{error}");
    }

    /// Conditional UI is the same ceremony with the mediation hint the browser
    /// needs in order to offer passkeys from an autofill dropdown.
    #[test]
    fn conditional_ui_asks_for_conditional_mediation() {
        let challenge = rp()
            .start_conditional_authentication()
            .expect("a conditional authentication starts");
        let options: RequestChallengeResponse =
            serde_json::from_value(challenge.options).expect("options round-trip");
        assert!(
            options.mediation.is_some(),
            "the autofill flow must ask for conditional mediation"
        );
    }

    // -----------------------------------------------------------------------
    // Security keys — the other half of `require_user_verification`
    // -----------------------------------------------------------------------

    /// `require_user_verification(false)` is not a no-op: it chooses the
    /// security-key ceremony, and the credential records that it did.
    #[test]
    fn user_verification_off_selects_the_security_key_ceremony() {
        let rp = rp().require_user_verification(false);
        let mut browser = VirtualBrowser::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&challenge);
        let credential = rp
            .finish_registration_for("usr_1", &challenge, &response)
            .expect("registration finishes");

        assert!(
            !credential.user_verified,
            "a security key is one factor, not two"
        );

        let challenge = rp
            .start_authentication(std::slice::from_ref(&credential))
            .expect("authentication starts");
        let response = browser.authenticate(&challenge);
        let counter = rp
            .finish_authentication(&challenge, &response, &credential)
            .expect("a security key authenticates");
        assert!(counter > 0);
    }

    /// The verifying and non-verifying ceremonies produce different options, so
    /// the flag is observable from the wire and not only from our own records.
    #[test]
    fn the_two_ceremonies_ask_the_browser_for_different_things() {
        let verifying = rp()
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let permissive = rp()
            .require_user_verification(false)
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");

        assert_ne!(
            verifying.options["publicKey"]["authenticatorSelection"],
            permissive.options["publicKey"]["authenticatorSelection"],
            "the two ceremonies must not ask for the same user-verification policy"
        );
        assert_eq!(
            verifying.options["publicKey"]["authenticatorSelection"]["userVerification"],
            serde_json::json!("required")
        );
    }

    // -----------------------------------------------------------------------
    // The storage schema
    // -----------------------------------------------------------------------

    /// The counter *column* is authoritative: a store that updates it and
    /// leaves the opaque record alone is still correct, because the record's
    /// own counter is overridden on the way back in. Getting this backwards
    /// would silently disable clone detection.
    #[test]
    fn the_counter_column_overrides_the_stored_record() {
        let rp = rp();
        let mut browser = VirtualBrowser::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&challenge);
        let mut credential = rp
            .finish_registration_for("usr_1", &challenge, &response)
            .expect("registration finishes");

        credential.sign_count = 4242;
        let rebuilt = credential.to_credential().expect("the record parses");
        assert_eq!(rebuilt.counter, 4242);
    }

    /// The stored COSE key is a real `COSE_Key`, not a private encoding: an EC2
    /// key is a five-entry map whose first label is `1` and whose value is `2`.
    #[test]
    fn the_stored_public_key_is_canonical_cose() {
        let rp = rp();
        let mut browser = VirtualBrowser::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&challenge);
        let credential = rp
            .finish_registration_for("usr_1", &challenge, &response)
            .expect("registration finishes");

        let key = &credential.public_key;
        assert_eq!(key[0], 0xa5, "a definite-length five-entry CBOR map");
        assert_eq!(key[1], 0x01, "label 1, the key type");
        assert_eq!(key[2], 0x02, "value 2, EC2");
        assert_eq!(key[3], 0x03, "label 3, the algorithm");
        assert_eq!(key[4], 0x26, "value -7, ES256");
    }

    /// The CBOR head encoder is the one piece here nothing else checks, and a
    /// wrong length prefix produces a key that decodes to garbage rather than
    /// failing loudly.
    #[test]
    fn cbor_integers_use_the_shortest_encoding() {
        let mut out = Vec::new();
        cbor_int(&mut out, 0);
        assert_eq!(out, [0x00]);

        out.clear();
        cbor_int(&mut out, 23);
        assert_eq!(out, [0x17]);

        out.clear();
        cbor_int(&mut out, 24);
        assert_eq!(out, [0x18, 0x18]);

        out.clear();
        cbor_int(&mut out, -1);
        assert_eq!(out, [0x20]);

        out.clear();
        cbor_int(&mut out, -7);
        assert_eq!(out, [0x26]);

        out.clear();
        cbor_int(&mut out, -257);
        assert_eq!(out, [0x39, 0x01, 0x00]);

        out.clear();
        cbor_bytes(&mut out, &[1, 2, 3]);
        assert_eq!(out, [0x43, 1, 2, 3]);
    }

    /// The user handle is deterministic, sixteen bytes, and a UUID user id is
    /// passed through unchanged so an application that already has one is not
    /// forced into a second identifier.
    #[test]
    fn the_user_handle_is_deterministic_and_passes_uuids_through() {
        let handle = WebAuthn::user_handle_for("usr_1");
        assert_eq!(handle, WebAuthn::user_handle_for("usr_1"));
        assert_ne!(handle, WebAuthn::user_handle_for("usr_2"));
        assert_eq!(handle.as_bytes().len(), 16);

        let uuid = "3f2b1c8e-9d4a-4f6b-8c1d-2e5a7b9c0d3f";
        assert_eq!(
            WebAuthn::user_handle_for(uuid).to_string(),
            uuid,
            "a user id that already is a UUID is the handle"
        );
    }

    /// A misconfigured relying party is a configuration error with the fix in
    /// it, not a panic from a constructor that cannot return one.
    #[test]
    fn a_mismatched_origin_names_the_fix() {
        let rp = WebAuthn::new("other.example", "https://example.com", "Example");
        let error = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect_err("an rp_id that is not a suffix of the origin cannot work");
        let message = format!("{error}");
        assert!(message.contains("rp_id"), "{message}");
        assert!(
            message.contains("registrable suffix"),
            "the message must say what the rule is: {message}"
        );
    }

    /// `Debug` on a challenge must not print the ceremony state, which is a
    /// bearer value for the finish step: a challenge in a log line is a
    /// challenge in a log aggregator.
    #[test]
    fn a_challenge_does_not_print_its_state() {
        let challenge = rp()
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let printed = format!("{challenge:?}");
        assert!(
            !printed.contains(challenge.state.expose()),
            "the ceremony state must not be printed"
        );
        assert!(printed.contains("***"), "the state must be redacted");
    }

    /// A registration challenge remembers whose it is, and finishing it for a
    /// different account is refused rather than filed under the wrong user.
    #[test]
    fn a_registration_is_attributed_to_the_account_that_started_it() {
        let rp = rp();
        let mut browser = VirtualBrowser::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&challenge);

        let error = rp
            .finish_registration_for("usr_2", &challenge, &response)
            .expect_err("the challenge was not issued for usr_2");
        assert!(format!("{error}").contains("different account"), "{error}");

        let credential = rp
            .finish_registration(&challenge, &response)
            .expect("registration finishes");
        assert_eq!(
            credential.user_id, "usr_1",
            "the account comes from the challenge, not from the request"
        );
        assert_eq!(
            credential.user_handle,
            B64.encode(WebAuthn::user_handle_for("usr_1").as_bytes())
        );
    }

    // -----------------------------------------------------------------------
    // The shipped store, from the ceremony's side
    // -----------------------------------------------------------------------

    /// A registered credential round-trips through a `dyn PasskeyStore` and can
    /// be found again by credential id **alone**, which is the lookup the
    /// usernameless flow rests on and the one property of the trait that the
    /// ceremony code here actually depends on. The stores' own rules are
    /// asserted against both of them in `store::conformance`.
    #[tokio::test]
    async fn a_registered_credential_round_trips_through_the_store() {
        let rp = rp();
        let mut browser = VirtualBrowser::new();

        let challenge = rp
            .start_registration("usr_1", "ada@example.com", "Ada", &[])
            .expect("registration starts");
        let response = browser.register(&challenge);
        let credential = rp
            .finish_registration_for("usr_1", &challenge, &response)
            .expect("registration finishes");

        let store: &dyn PasskeyStore = &MemoryPasskeyStore::new();
        store.insert(&credential).await.expect("insert");

        let found = store
            .find(&credential.credential_id)
            .await
            .expect("find")
            .expect("the credential is there");
        assert_eq!(found.user_id, "usr_1");
        assert_eq!(store.list_for_user("usr_1").await.expect("list").len(), 1);
        assert!(store.list_for_user("usr_2").await.expect("list").is_empty());
    }
}
