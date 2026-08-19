//! Acceptance criterion 2: **a key containing `:` cannot forge another
//! namespace.**
//!
//! `docs/02-data/25-kv-cache.md` asks for a fuzz test, and this is it. The
//! property being fuzzed is stronger than the one asked for, and easier to
//! check:
//!
//! > The map from `(application, namespace, version, key parts)` to the key
//! > string is **injective**.
//!
//! Forging a namespace is the special case where two *different* triples
//! produce the same string. Proving injectivity over a corpus that is full of
//! the characters an attacker would reach for — `:`, `\`, `#`, control bytes,
//! and the escape sequences the encoder itself emits — proves the special case
//! and several others besides.
//!
//! # Why a deterministic generator and not `arbitrary`
//!
//! A fuzz failure that cannot be reproduced is a bug report nobody can act on.
//! The generator here is an xorshift with a fixed seed, so a failure names the
//! exact inputs and reproduces on every machine. The corpus is also seeded with
//! the hand-written adversarial cases first, because those are the ones that
//! would actually be tried.

use std::collections::HashMap;

use moso_kv::key::{Key, KeyBuf, MAX_KEY_LEN, is_valid_name};

/// A reproducible pseudo-random source.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        usize::try_from(self.next() % bound as u64).unwrap_or(0)
    }

    fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[self.below(from.len())]
    }
}

/// The characters a key part is built out of.
///
/// Deliberately dense in the ones that matter: the separator, the escape
/// character, the byte marker, the sequences the encoder emits (`\c`, `\h`,
/// `\x00`), control bytes, and a couple of multi-byte characters so that the
/// encoder cannot be assumed to be byte-oriented.
const ALPHABET: &[&str] = &[
    ":", "\\", "#", "\\c", "\\h", "\\x00", "\\\\", "a", "b", "0", "1", "-", "_", ".", "*", "%",
    "\0", "\n", "\x7f", "é", "☃", "", "v1", "moso", "shop", "profile",
];

/// The namespaces the corpus is built over.
const NAMESPACES: &[&str] = &["profile", "profile2", "p", "login-code", "login_code", "a"];

/// The applications the corpus is built over.
const APPS: &[&str] = &["shop", "shop2", "s", "blog"];

/// The versions the corpus is built over.
const VERSIONS: &[u16] = &[1, 2, 11, 111];

/// One randomly assembled key part.
fn part(rng: &mut Rng) -> String {
    let pieces = 1 + rng.below(5);
    let mut out = String::new();
    for _ in 0..pieces {
        // The turbofish pins `T = &str`; under the 1.94 MSRV, inference otherwise
        // picks `T = str` and rejects the call. Deref coercion then hands
        // `push_str` the `&str` it wants.
        out.push_str(rng.pick::<&str>(ALPHABET));
    }
    out
}

/// The key for one `(app, namespace, version, part)`.
fn build(app: &str, namespace: &str, version: u16, value: &str) -> Key {
    let mut buf = KeyBuf::new(app, namespace, version).expect("the names are valid");
    buf.segment_str(value);
    buf.finish().expect("short enough")
}

/// One `(application, namespace, version, key part)`.
type Input = (&'static str, &'static str, u16, &'static str);

#[test]
fn the_hand_written_forgeries_all_fail() {
    // Each pair is "what an attacker sends" against "what they are trying to
    // impersonate". None of them may collide.
    let attempts: &[(Input, Input)] = &[
        // A colon in a key part, trying to become another namespace.
        (
            ("shop", "profile", 1, "x:other:1:y"),
            ("shop", "other", 1, "y"),
        ),
        // An escaped escape, trying to make the next colon "escape itself".
        (
            ("shop", "profile", 1, "a\\:b"),
            ("shop", "profile", 1, "a\\"),
        ),
        // The encoder's own output, fed back in.
        (
            ("shop", "profile", 1, "a\\cb"),
            ("shop", "profile", 1, "a:b"),
        ),
        (
            ("shop", "profile", 1, "\\h00"),
            ("shop", "profile", 1, "#00"),
        ),
        // A version that is a prefix of another version.
        (("shop", "profile", 1, "1:x"), ("shop", "profile", 11, "x")),
        // An application name that is a prefix of another.
        (("shop", "profile", 1, "a"), ("shop2", "profile", 1, "a")),
        // A namespace that is a prefix of another.
        (("shop", "p", 1, "rofile:1:a"), ("shop", "profile", 1, "a")),
        // The whole key layout, as a key part.
        (
            ("shop", "profile", 1, "moso:v1:shop:profile:1:x"),
            ("shop", "profile", 1, "x"),
        ),
    ];

    for ((app_a, ns_a, v_a, part_a), (app_b, ns_b, v_b, part_b)) in attempts {
        let forged = build(app_a, ns_a, *v_a, part_a);
        let target = build(app_b, ns_b, *v_b, part_b);
        assert_ne!(
            forged, target,
            "`{part_a}` in {app_a}/{ns_a}/v{v_a} forged {app_b}/{ns_b}/v{v_b}"
        );
        assert!(
            forged
                .as_str()
                .starts_with(&format!("moso:v1:{app_a}:{ns_a}:{v_a}:")),
            "`{part_a}` escaped its own namespace: {forged}"
        );
    }
}

#[test]
fn the_key_mapping_is_injective_over_a_fuzzed_corpus() {
    let mut rng = Rng::new(0x5EED_0000_1234_ABCD);
    let mut seen: HashMap<String, (String, String, u16, String)> = HashMap::new();

    for _ in 0..40_000 {
        let app = *rng.pick(APPS);
        let namespace = *rng.pick(NAMESPACES);
        let version = *rng.pick(VERSIONS);
        let value = part(&mut rng);

        let key = build(app, namespace, version, &value);
        let input = (app.to_owned(), namespace.to_owned(), version, value.clone());

        if let Some(previous) = seen.insert(key.as_str().to_owned(), input.clone()) {
            assert_eq!(
                previous, input,
                "two different inputs produced `{key}`: {previous:?} and {input:?}"
            );
        }
    }

    assert!(seen.len() > 1_000, "the corpus was too small to mean much");
}

#[test]
fn no_fuzzed_key_ever_leaves_its_namespace() {
    let mut rng = Rng::new(0xC0FF_EE00_0000_0001);

    for _ in 0..20_000 {
        let app = *rng.pick(APPS);
        let namespace = *rng.pick(NAMESPACES);
        let version = *rng.pick(VERSIONS);
        let value = part(&mut rng);

        let key = build(app, namespace, version, &value);
        let expected = format!("moso:v1:{app}:{namespace}:{version}");

        assert_eq!(
            key.namespace_prefix(),
            expected,
            "`{value}` produced `{key}`"
        );
        assert!(
            !key.parts().contains(':'),
            "`{value}` left an unescaped separator in `{key}`"
        );
        assert!(
            !key.as_bytes().contains(&0),
            "`{value}` left a NUL in `{key}`"
        );
        assert!(key.len() <= MAX_KEY_LEN);
    }
}

#[test]
fn a_fuzzed_prefix_never_matches_another_namespace() {
    let mut rng = Rng::new(0xDEAD_BEEF_0000_0007);

    // Every namespace's prefix, built once.
    let prefixes: Vec<(String, String, u16, Key)> = APPS
        .iter()
        .flat_map(|app| {
            NAMESPACES.iter().flat_map(move |namespace| {
                VERSIONS.iter().map(move |version| {
                    let key = KeyBuf::new(app, namespace, *version)
                        .expect("valid")
                        .finish_prefix()
                        .expect("short");
                    ((*app).to_owned(), (*namespace).to_owned(), *version, key)
                })
            })
        })
        .collect();

    for _ in 0..5_000 {
        let app = *rng.pick(APPS);
        let namespace = *rng.pick(NAMESPACES);
        let version = *rng.pick(VERSIONS);
        let key = build(app, namespace, version, &part(&mut rng));

        for (prefix_app, prefix_ns, prefix_version, prefix) in &prefixes {
            let mine = prefix_app == app && prefix_ns == namespace && *prefix_version == version;
            assert_eq!(
                key.starts_with(prefix),
                mine,
                "`{key}` matched {prefix_app}/{prefix_ns}/v{prefix_version} \
                 but belongs to {app}/{namespace}/v{version}"
            );
        }
    }
}

#[test]
fn a_namespace_prefix_is_never_forgeable_because_names_are_validated() {
    // The layout's safety rests on the first five segments being fixed. That
    // holds because a name cannot contain a separator in the first place.
    for candidate in [
        "a:b",
        "a b",
        "A",
        "",
        "moso:v1",
        "profile\\",
        "profile#",
        "profile.v2",
        "prófile",
    ] {
        assert!(
            !is_valid_name(candidate),
            "`{candidate}` was accepted as a name"
        );
        assert!(KeyBuf::new("shop", candidate, 1).is_err());
        assert!(KeyBuf::new(candidate, "profile", 1).is_err());
    }
}

#[test]
fn every_alphabet_character_survives_a_round_trip_through_one_segment() {
    // Not a round trip in the decoding sense — keys are write-only — but a
    // proof that no character is silently dropped, which would collapse two
    // distinct key parts into one.
    let mut seen = HashMap::new();
    for piece in ALPHABET {
        let key = build("shop", "profile", 1, piece);
        if let Some(previous) = seen.insert(key.parts().to_owned(), *piece) {
            assert_eq!(
                previous, *piece,
                "`{piece}` and `{previous}` encode to the same segment"
            );
        }
    }
}
