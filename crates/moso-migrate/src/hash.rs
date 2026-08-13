//! Checksums.
//!
//! A migration's checksum is the mechanism that turns "please do not edit an
//! applied migration" from a convention into an enforced rule
//! (`docs/02-data/23-migrations.md` § safety policy, point 5), so it has to be
//! a real hash rather than a length and a mood.
//!
//! It is SHA-256, implemented here rather than pulled in. `xtask check-deps`
//! rule 6 counts third-party crates against a budget the workspace is already
//! over, and this is sixty lines of arithmetic with published test vectors —
//! the three NIST vectors are asserted below, so a mistake in it cannot ship
//! silently. There is no security claim attached: nothing here defends against
//! an adversary who can write to `migrations/`, because such an adversary can
//! write a new migration instead.

use std::fmt;

/// A SHA-256 digest.
///
/// ```
/// use moso_migrate::Checksum;
///
/// let checksum = Checksum::of(b"abc");
/// assert_eq!(checksum.short(), "ba7816bf");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Checksum([u8; 32]);

impl Checksum {
    /// Hashes a byte string.
    ///
    /// ```
    /// use moso_migrate::Checksum;
    ///
    /// assert_eq!(
    ///     Checksum::of(b"").to_string(),
    ///     "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    /// );
    /// ```
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(sha256(bytes))
    }

    /// Hashes a migration body, ignoring the things a reformat changes.
    ///
    /// A checksum that fires because someone re-indented a file is a checksum
    /// people learn to bypass, and a bypassed check is worse than none. Line
    /// endings, trailing whitespace and blank lines are normalised away;
    /// everything else — including comments, because a comment is where a
    /// destructive statement hides — is hashed.
    ///
    /// ```
    /// use moso_migrate::Checksum;
    ///
    /// let unix = Checksum::of_migration("SELECT 1;\nSELECT 2;\n");
    /// let windows = Checksum::of_migration("SELECT 1;  \r\n\r\nSELECT 2;\r\n");
    /// assert_eq!(unix, windows);
    /// ```
    #[must_use]
    pub fn of_migration(body: &str) -> Self {
        let normalised: Vec<&str> = body
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .collect();
        Self::of(normalised.join("\n").as_bytes())
    }

    /// The raw digest.
    ///
    /// ```
    /// assert_eq!(moso_migrate::Checksum::of(b"abc").as_bytes().len(), 32);
    /// ```
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The first eight hex characters, which is what a generated file's
    /// `moso:generated-from` header carries.
    ///
    /// ```
    /// assert_eq!(moso_migrate::Checksum::of(b"abc").short().len(), 8);
    /// ```
    #[must_use]
    pub fn short(&self) -> String {
        self.to_string()[..8].to_owned()
    }

    /// Parses a 64-character hex digest, as read back from the ledger.
    ///
    /// ```
    /// use moso_migrate::Checksum;
    ///
    /// let checksum = Checksum::of(b"abc");
    /// assert_eq!(Checksum::parse(&checksum.to_string()), Some(checksum));
    /// assert_eq!(Checksum::parse("nope"), None);
    /// ```
    #[must_use]
    pub fn parse(hex: &str) -> Option<Self> {
        if hex.len() != 64 {
            return None;
        }
        let mut out = [0_u8; 32];
        for (index, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
        }
        Some(Self(out))
    }
}

impl fmt::Display for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Checksum({self})")
    }
}

/// FNV-1a, 64-bit. Used for advisory-lock keys, where the requirement is
/// "spread out" rather than "unforgeable".
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// SHA-256 (FIPS 180-4).
fn sha256(message: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    let bit_len = (message.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(message.len() + 72);
    padded.extend_from_slice(message);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for block in padded.chunks_exact(64) {
        let mut w = [0_u32; 64];
        for (index, word) in w.iter_mut().take(16).enumerate() {
            let at = index * 4;
            *word = u32::from_be_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut out = [0_u8; 32];
    for (index, word) in state.iter().enumerate() {
        out[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three vectors every SHA-256 implementation is checked against:
    /// the empty string, the one-block `"abc"`, and the two-block message
    /// from FIPS 180-4's appendix.
    #[test]
    fn nist_vectors() {
        assert_eq!(
            Checksum::of(b"").to_string(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            Checksum::of(b"abc").to_string(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            Checksum::of(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq").to_string(),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn a_million_a_characters() {
        let message = vec![b'a'; 1_000_000];
        assert_eq!(
            Checksum::of(&message).to_string(),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn block_boundaries_are_handled() {
        // 55, 56, 63, 64 and 65 bytes exercise every padding path.
        for length in [55_usize, 56, 63, 64, 65, 119, 120] {
            let message = vec![b'x'; length];
            let digest = Checksum::of(&message);
            assert_eq!(digest.to_string().len(), 64, "length {length}");
            assert_eq!(Checksum::parse(&digest.to_string()), Some(digest));
        }
    }

    #[test]
    fn migration_checksums_ignore_formatting_only() {
        let a = Checksum::of_migration("-- +migrate up\nSELECT 1;\n");
        let b = Checksum::of_migration("-- +migrate up\r\n\r\nSELECT 1;   \r\n");
        assert_eq!(a, b);

        let c = Checksum::of_migration("-- +migrate up\nSELECT 2;\n");
        assert_ne!(a, c);

        // A commented-out destructive statement is part of the file's meaning.
        let live = Checksum::of_migration("DROP TABLE t;");
        let commented = Checksum::of_migration("-- DROP TABLE t;");
        assert_ne!(live, commented);
    }

    #[test]
    fn parse_rejects_nonsense() {
        assert_eq!(Checksum::parse(""), None);
        assert_eq!(Checksum::parse(&"z".repeat(64)), None);
    }

    #[test]
    fn fnv_spreads() {
        assert_ne!(fnv1a(b"a"), fnv1a(b"b"));
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
    }
}
