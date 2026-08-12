//! [`CursorCodec`] — what makes a [`Page`](crate::response::Page) cursor
//! tamper-proof.
//!
//! [`moso_schema::Cursor`] is only the carrier: base64url in, base64url out,
//! with no authentication. Signing needs the application secret, so it lives
//! here.
//!
//! ```
//! use moso::prelude::*;
//! use moso::response::cursor::CursorCodec;
//! /// A post, as the API returns one.
//! #[derive(Schema)]
//! pub struct PostOut {
//!     /// URL-safe identifier.
//!     pub slug: Slug,
//! }
//!
//! /// The query string this listing accepts.
//! #[derive(Schema)]
//! pub struct ListQuery {
//!     /// Where to resume from.
//!     pub cursor: Option<Cursor>,
//! }
//!
//! /// List posts, page by page.
//! #[endpoint]
//! async fn list(
//!     Inject(cursors): Inject<CursorCodec>,
//!     Query(q): Query<ListQuery>,
//! ) -> Result<Page<PostOut>> {
//!     let after: Option<u64> = q
//!         .cursor
//!         .map(|c| cursors.verify_value("posts", &c))
//!         .transpose()?;
//!     let _ = after;
//!     Ok(Page::new(Vec::new()).with_next(cursors.sign_value("posts", &42_u64)?))
//! }
//! # fn main() {
//! let codec = CursorCodec::new("a-32-byte-or-longer-signing-secret");
//! let cursor = codec.sign_value("posts", &42_u64).unwrap();
//! assert_eq!(codec.verify_value::<u64>("posts", &cursor).unwrap(), 42);
//! # }
//! ```
//!
//! # Why a cursor has to be signed
//!
//! An unsigned cursor is a query parameter the client can edit. Since it
//! encodes a sort key that goes straight into a `WHERE` clause, editing it is
//! at best a way to page past a filter the endpoint thought it had applied, and
//! at worst a way to make the server deserialise a shape it did not expect. A
//! MAC turns "opaque by politeness" into "opaque by arithmetic".
//!
//! # The wire format
//!
//! ```text
//! ┌─────────┬───────────────────────┬──────────────────┐
//! │ version │        payload        │   tag (16 B)     │
//! │  1 byte │      n bytes          │ truncated HMAC   │
//! └─────────┴───────────────────────┴──────────────────┘
//! ```
//!
//! The tag covers the version byte, the *scope* and the payload. The scope is
//! never transmitted: a cursor minted for `"posts"` therefore fails to verify
//! against `"comments"` without either endpoint having to check anything, which
//! is the "foreign cursor" case.
//!
//! A truncated 128-bit tag is the usual trade for a token that travels in a URL:
//! forging one needs 2^128 work, while the full 256-bit tag would add 21
//! characters to every cursor.
//!
//! # Not a general-purpose crypto module
//!
//! The SHA-256 and HMAC below are here because a pagination MAC is the only
//! thing in `moso-core` that needs one, and a dependency for it would pull a
//! trait ecosystem into every build. They are the textbook constructions,
//! checked against the FIPS 180-4 and RFC 4231 vectors in this file's tests.
//! Anything that needs agility, key rotation or a password hash should use a
//! real crypto crate.

use std::sync::Arc;

use moso_schema::Cursor;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};

/// Signs and verifies pagination cursors with the application secret.
///
/// Register one provider at boot and read it with `Inject<CursorCodec>`:
///
/// ```
/// use moso::response::cursor::CursorCodec;
///
/// let codec = CursorCodec::new("a-32-byte-or-longer-signing-secret");
/// let cursor = codec.sign_value("posts", &7_u64).unwrap();
///
/// assert_eq!(codec.verify_value::<u64>("posts", &cursor).unwrap(), 7);
/// // A cursor minted for one listing cannot be replayed against another.
/// assert!(codec.verify_value::<u64>("comments", &cursor).is_err());
/// ```
///
/// Register one at boot — `App::new(cfg).provide(CursorCodec::new(secret))` —
/// and reach it from a handler with `Inject<CursorCodec>`.
///
/// Cloning is cheap — the key is behind an [`Arc`] — so it can be handed to a
/// service layer without ceremony.
#[derive(Clone)]
pub struct CursorCodec {
    key: Arc<[u8; BLOCK_LEN]>,
}

/// The format byte, so a future change of layout is a clean rejection rather
/// than a confusing decode failure.
const VERSION: u8 = 1;

/// Bytes of MAC kept. 128 bits is the standard truncation for a URL token.
const TAG_LEN: usize = 16;

impl CursorCodec {
    /// Bytes of MAC appended to every cursor.
    pub const TAG_LEN: usize = TAG_LEN;

    /// The largest payload [`CursorCodec::sign`] will accept.
    ///
    /// Chosen so a signed cursor stays inside
    /// [`Cursor::MAX_ENCODED_LENGTH`](moso_schema::Cursor::MAX_ENCODED_LENGTH)
    /// after base64: a cursor that cannot be decoded by the type that carries it
    /// is worse than one that was refused at mint time.
    pub const MAX_PAYLOAD: usize = Cursor::MAX_ENCODED_LENGTH / 4 * 3 - 1 - TAG_LEN;

    /// Derive a codec from the application secret.
    ///
    /// Any length is accepted — the HMAC construction normalises it — but a
    /// secret shorter than 32 bytes gives the tag less strength than its length
    /// suggests, and `moso doctor` says so.
    #[must_use]
    pub fn new(secret: impl AsRef<[u8]>) -> Self {
        Self {
            key: Arc::new(hmac_key(secret.as_ref())),
        }
    }

    /// Sign `payload` under `scope`.
    ///
    /// `scope` is a short, stable label for the listing the cursor belongs to —
    /// the route name is the obvious choice. It is mixed into the MAC and never
    /// sent, so a cursor issued by one listing cannot be replayed against
    /// another.
    ///
    /// # Errors
    /// A 500 if `payload` exceeds [`CursorCodec::MAX_PAYLOAD`], because a cursor
    /// that large is a bug in the caller rather than something a client did.
    pub fn sign(&self, scope: &str, payload: &[u8]) -> Result<Cursor> {
        if payload.len() > Self::MAX_PAYLOAD {
            return Err(Error::internal_msg(format!(
                "a pagination cursor payload may be at most {} bytes, got {}",
                Self::MAX_PAYLOAD,
                payload.len()
            )));
        }
        let mut out = Vec::with_capacity(1 + payload.len() + TAG_LEN);
        out.push(VERSION);
        out.extend_from_slice(payload);
        let tag = self.tag(scope, &out);
        out.extend_from_slice(&tag);
        Ok(Cursor::from_bytes(out))
    }

    /// Sign `value`, serialised as JSON, under `scope`.
    ///
    /// # Errors
    /// A 500 if `value` cannot be serialised or the result is too large.
    pub fn sign_value<T: Serialize + ?Sized>(&self, scope: &str, value: &T) -> Result<Cursor> {
        let payload = serde_json::to_vec(value).map_err(|error| {
            Error::internal(error).with_detail("a pagination cursor could not be serialised")
        })?;
        self.sign(scope, &payload)
    }

    /// Recover the payload of a cursor issued by this codec under `scope`.
    ///
    /// # Errors
    /// [`Error::bad_request`] for a cursor that was truncated, edited, issued
    /// under a different scope, or issued by a server with a different secret.
    /// The four cases are deliberately indistinguishable to the client: telling
    /// an attacker *which* part of a token failed is how a forgery oracle
    /// starts.
    pub fn verify(&self, scope: &str, cursor: &Cursor) -> Result<Vec<u8>> {
        let bytes = cursor.as_bytes();
        if bytes.len() < 1 + TAG_LEN || bytes[0] != VERSION {
            return Err(rejected());
        }
        let (signed, tag) = bytes.split_at(bytes.len() - TAG_LEN);
        if !constant_time_eq(&self.tag(scope, signed), tag) {
            return Err(rejected());
        }
        Ok(signed[1..].to_vec())
    }

    /// Verify a cursor and deserialise its payload as JSON.
    ///
    /// # Errors
    /// As [`CursorCodec::verify`], plus a 400 if the authenticated payload does
    /// not deserialise into `T` — which happens when the cursor is genuine but
    /// was issued by an older deployment whose key tuple had a different shape.
    pub fn verify_value<T: DeserializeOwned>(&self, scope: &str, cursor: &Cursor) -> Result<T> {
        let payload = self.verify(scope, cursor)?;
        serde_json::from_slice(&payload).map_err(|_| rejected())
    }

    /// Decode and verify a cursor still in its base64url form.
    ///
    /// The shape a query-string parameter arrives in.
    ///
    /// # Errors
    /// As [`CursorCodec::verify`], and the same 400 for input that is not
    /// base64url at all.
    pub fn verify_str(&self, scope: &str, encoded: &str) -> Result<Vec<u8>> {
        let cursor = Cursor::decode(encoded).map_err(|_| rejected())?;
        self.verify(scope, &cursor)
    }

    /// The truncated MAC over the scope and `signed`.
    ///
    /// The scope is length-prefixed so that `("ab", "c")` and `("a", "bc")`
    /// cannot produce the same input — the classic concatenation ambiguity.
    fn tag(&self, scope: &str, signed: &[u8]) -> [u8; TAG_LEN] {
        let mut mac = Hmac::new(&self.key);
        mac.update(&(scope.len() as u64).to_be_bytes());
        mac.update(scope.as_bytes());
        mac.update(signed);
        let full = mac.finish();
        let mut tag = [0u8; TAG_LEN];
        tag.copy_from_slice(&full[..TAG_LEN]);
        tag
    }
}

impl core::fmt::Debug for CursorCodec {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("CursorCodec(***)")
    }
}

/// The one rejection every cursor failure produces.
fn rejected() -> Error {
    Error::bad_request(
        "the `cursor` parameter was not issued by this API for this listing; \
         drop it and start from the first page",
    )
}

/// Compare two tags without leaking where they first differ.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

// ---------------------------------------------------------------------------
// HMAC-SHA256
// ---------------------------------------------------------------------------

/// SHA-256's block size, which is also HMAC's.
const BLOCK_LEN: usize = 64;

/// SHA-256's digest size.
const DIGEST_LEN: usize = 32;

/// Normalise a secret of any length into one HMAC block, per RFC 2104: a key
/// longer than the block is hashed first, a shorter one is zero-padded.
fn hmac_key(secret: &[u8]) -> [u8; BLOCK_LEN] {
    let mut key = [0u8; BLOCK_LEN];
    if secret.len() > BLOCK_LEN {
        key[..DIGEST_LEN].copy_from_slice(&sha256(secret));
    } else {
        key[..secret.len()].copy_from_slice(secret);
    }
    key
}

/// HMAC-SHA256 over an incrementally supplied message.
struct Hmac {
    inner: Sha256,
    outer_key: [u8; BLOCK_LEN],
}

impl Hmac {
    /// Start a MAC under an already-normalised key.
    fn new(key: &[u8; BLOCK_LEN]) -> Self {
        let mut inner_key = [0u8; BLOCK_LEN];
        let mut outer_key = [0u8; BLOCK_LEN];
        for i in 0..BLOCK_LEN {
            inner_key[i] = key[i] ^ 0x36;
            outer_key[i] = key[i] ^ 0x5c;
        }
        let mut inner = Sha256::new();
        inner.update(&inner_key);
        Self { inner, outer_key }
    }

    /// Feed more message bytes.
    fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    /// Finish, producing the 32-byte tag.
    fn finish(self) -> [u8; DIGEST_LEN] {
        let inner = self.inner.finish();
        let mut outer = Sha256::new();
        outer.update(&self.outer_key);
        outer.update(&inner);
        outer.finish()
    }
}

/// One-shot SHA-256.
fn sha256(data: &[u8]) -> [u8; DIGEST_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finish()
}

/// The SHA-256 round constants: the first 32 bits of the fractional parts of
/// the cube roots of the first 64 primes (FIPS 180-4 §4.2.2).
#[rustfmt::skip]
const ROUND_CONSTANTS: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// The initial hash value: the first 32 bits of the fractional parts of the
/// square roots of the first eight primes (FIPS 180-4 §5.3.3).
const INITIAL_STATE: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// A streaming SHA-256.
struct Sha256 {
    state: [u32; 8],
    buffer: [u8; BLOCK_LEN],
    buffered: usize,
    length: u64,
}

impl Sha256 {
    /// A fresh hasher.
    fn new() -> Self {
        Self {
            state: INITIAL_STATE,
            buffer: [0u8; BLOCK_LEN],
            buffered: 0,
            length: 0,
        }
    }

    /// Absorb `data`.
    fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);

        if self.buffered > 0 {
            let take = (BLOCK_LEN - self.buffered).min(data.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered < BLOCK_LEN {
                // Still short of a block, and `data` is exhausted. Returning
                // here rather than falling through is load-bearing: the tail
                // below would reset `buffered` to the (empty) remainder and
                // silently drop everything buffered so far.
                return;
            }
            let block = self.buffer;
            self.compress(&block);
            self.buffered = 0;
        }

        let mut chunks = data.chunks_exact(BLOCK_LEN);
        for chunk in &mut chunks {
            let mut block = [0u8; BLOCK_LEN];
            block.copy_from_slice(chunk);
            self.compress(&block);
        }

        let rest = chunks.remainder();
        self.buffer[..rest.len()].copy_from_slice(rest);
        self.buffered = rest.len();
    }

    /// Pad and produce the digest.
    fn finish(mut self) -> [u8; DIGEST_LEN] {
        let bit_length = self.length.wrapping_mul(8);

        // 0x80, then zeroes, then the 64-bit big-endian bit length.
        self.buffer[self.buffered] = 0x80;
        self.buffered += 1;
        if self.buffered > BLOCK_LEN - 8 {
            self.buffer[self.buffered..].fill(0);
            let block = self.buffer;
            self.compress(&block);
            self.buffered = 0;
        }
        self.buffer[self.buffered..BLOCK_LEN - 8].fill(0);
        self.buffer[BLOCK_LEN - 8..].copy_from_slice(&bit_length.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);

        let mut digest = [0u8; DIGEST_LEN];
        for (out, word) in digest.chunks_exact_mut(4).zip(self.state) {
            out.copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    /// One block of the compression function (FIPS 180-4 §6.2.2).
    fn compress(&mut self, block: &[u8; BLOCK_LEN]) {
        let mut w = [0u32; 64];
        for (word, chunk) in w.iter_mut().zip(block.chunks_exact(4)) {
            *word = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(ROUND_CONSTANTS[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// FIPS 180-4 §B.1–B.2 and the empty-input vector.
    #[test]
    fn sha256_matches_the_published_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // One million 'a', which exercises the multi-block path.
        let mut hasher = Sha256::new();
        for _ in 0..1_000 {
            hasher.update(&[b'a'; 1_000]);
        }
        assert_eq!(
            hex(&hasher.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// A digest must not depend on how the message was chopped up.
    #[test]
    fn sha256_is_independent_of_chunking() {
        let message: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        let one_shot = sha256(&message);
        for chunk in [1usize, 7, 63, 64, 65, 128] {
            let mut hasher = Sha256::new();
            for piece in message.chunks(chunk) {
                hasher.update(piece);
            }
            assert_eq!(hasher.finish(), one_shot, "chunked by {chunk}");
        }
    }

    /// RFC 4231 test cases 1, 2 and 6.
    #[test]
    fn hmac_matches_rfc_4231() {
        fn mac(key: &[u8], message: &[u8]) -> String {
            let mut hmac = Hmac::new(&hmac_key(key));
            hmac.update(message);
            hex(&hmac.finish())
        }

        assert_eq!(
            mac(&[0x0b; 20], b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            mac(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // A key longer than the block, which must be hashed first.
        assert_eq!(
            mac(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            ),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    fn codec() -> CursorCodec {
        CursorCodec::new("the application secret, which is long enough")
    }

    #[test]
    fn a_signed_cursor_round_trips() {
        let codec = codec();
        let cursor = codec.sign("posts", b"id:42").expect("signs");
        assert_eq!(codec.verify("posts", &cursor).unwrap(), b"id:42");
        // And through the encoded form a client would send back.
        assert_eq!(
            codec.verify_str("posts", &cursor.encode()).unwrap(),
            b"id:42"
        );
    }

    #[test]
    fn an_empty_payload_is_legal() {
        let codec = codec();
        let cursor = codec.sign("posts", b"").expect("signs");
        assert!(codec.verify("posts", &cursor).unwrap().is_empty());
    }

    #[test]
    fn a_json_payload_round_trips() {
        let codec = codec();
        let cursor = codec
            .sign_value("posts", &(1_700_000_000u64, "abc"))
            .expect("signs");
        let (at, id): (u64, String) = codec.verify_value("posts", &cursor).unwrap();
        assert_eq!((at, id.as_str()), (1_700_000_000, "abc"));
    }

    #[test]
    fn a_tampered_cursor_is_rejected() {
        let codec = codec();
        let cursor = codec.sign("posts", b"id:42").expect("signs");

        // Every single-byte edit, in the payload and in the tag alike.
        for index in 0..cursor.len() {
            let mut bytes = cursor.as_bytes().to_vec();
            bytes[index] ^= 0x01;
            match codec.verify("posts", &Cursor::from_bytes(bytes)) {
                Ok(payload) => panic!("byte {index} was edited but accepted as {payload:?}"),
                Err(error) => assert_eq!(error.status(), http::StatusCode::BAD_REQUEST),
            }
        }
    }

    #[test]
    fn a_truncated_or_extended_cursor_is_rejected() {
        let codec = codec();
        let cursor = codec.sign("posts", b"id:42").expect("signs");

        for length in 0..cursor.len() {
            let bytes = cursor.as_bytes()[..length].to_vec();
            assert!(codec.verify("posts", &Cursor::from_bytes(bytes)).is_err());
        }
        let mut longer = cursor.as_bytes().to_vec();
        longer.push(0);
        assert!(codec.verify("posts", &Cursor::from_bytes(longer)).is_err());
    }

    #[test]
    fn a_cursor_from_another_listing_is_rejected() {
        let codec = codec();
        let cursor = codec.sign("posts", b"id:42").expect("signs");
        assert!(codec.verify("comments", &cursor).is_err());
        // And the scope is length-prefixed, so no two scopes collide by
        // concatenation.
        let a = codec.sign("ab", b"c").unwrap();
        assert!(codec.verify("a", &a).is_err());
    }

    #[test]
    fn a_cursor_from_another_server_is_rejected() {
        let mine = codec();
        let theirs = CursorCodec::new("a completely different application secret");
        let cursor = theirs.sign("posts", b"id:42").expect("signs");
        assert!(mine.verify("posts", &cursor).is_err());
    }

    #[test]
    fn a_cursor_with_the_wrong_version_byte_is_rejected() {
        let codec = codec();
        let cursor = codec.sign("posts", b"id:42").expect("signs");
        let mut bytes = cursor.as_bytes().to_vec();
        bytes[0] = VERSION.wrapping_add(1);
        assert!(codec.verify("posts", &Cursor::from_bytes(bytes)).is_err());
    }

    #[test]
    fn a_genuine_cursor_whose_payload_no_longer_parses_is_a_400() {
        let codec = codec();
        let cursor = codec.sign("posts", b"not json").expect("signs");
        let error = codec
            .verify_value::<(u64, String)>("posts", &cursor)
            .expect_err("must not deserialise");
        assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn input_that_is_not_base64_is_rejected_the_same_way() {
        let codec = codec();
        let error = codec
            .verify_str("posts", "not base64!")
            .expect_err("rejects");
        assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn an_over_long_payload_is_refused_at_mint_time() {
        let codec = codec();
        let payload = vec![0u8; CursorCodec::MAX_PAYLOAD + 1];
        let error = codec.sign("posts", &payload).expect_err("refuses");
        assert_eq!(
            error.status(),
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "an over-long cursor is the server's bug, not the client's"
        );
        // And the largest accepted payload still fits the carrier.
        let largest = codec
            .sign("posts", &vec![0u8; CursorCodec::MAX_PAYLOAD])
            .expect("signs");
        assert!(largest.encode().len() <= Cursor::MAX_ENCODED_LENGTH);
        assert!(Cursor::decode(&largest.encode()).is_ok());
    }

    #[test]
    fn constant_time_eq_is_still_an_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn the_key_never_reaches_a_log() {
        assert_eq!(format!("{:?}", codec()), "CursorCodec(***)");
    }
}
