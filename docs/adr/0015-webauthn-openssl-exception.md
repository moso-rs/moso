# ADR-0015 - A scoped OpenSSL and MPL exception for the WebAuthn attestation path

Status: Accepted
Date: 2026-08-12
Deciders: Alessandro Zucchiatti

> **Note (2026-08-12):** Moso is now licensed `MIT` ([ADR-0018](0018-mit-relicence.md), superseding
> ADR-0014). This exception still stands and is trivially compatible under MIT - MPL-2.0 and
> CDLA-Permissive-2.0 dependencies combine cleanly with an MIT project. The AGPL/ADR-0014 references
> below are preserved as the licence context at the time of writing.

## Context

`docs/03-batteries/30-auth.md` specifies passkeys "via `webauthn-rs`", and `moso-auth`
implements the WebAuthn registration and authentication ceremonies on top of it. That crate is the
mature relying-party (server-side) WebAuthn library for Rust; the pure-Rust alternatives
(`passkey-rs`) are authenticator/client-side and are not a drop-in replacement for the server role a
web framework needs.

`webauthn-rs` collides with two policies this project set for itself, and the collision is not
avoidable by configuration:

- **The OpenSSL ban.** `45-security.md` and `deny.toml` forbid `openssl`/`openssl-sys`/`native-tls`
  in favour of rustls. `webauthn-rs-core` depends on `webauthn-attestation-ca`, which links OpenSSL
  to parse and verify attestation certificates. `webauthn-rs-core` exposes only a `default` feature,
  so **there is no feature knob that removes the OpenSSL edge** - it arrives unconditionally with the
  crate.
- **The permissive-only licence allowlist.** ADR-0014 keeps `deny.toml`'s allowlist permissive-only
  and refuses copyleft *dependencies*. Every `webauthn-rs` crate is MPL-2.0
  (`webauthn-rs`, `-core`, `-proto`, `webauthn-attestation-ca`, `webauthn-authenticator-rs`,
  `base64urlsafedata`), and the `webpki-roots` its HTTP path shares with rustls is
  CDLA-Permissive-2.0.

`cargo deny check` failed on both counts, and it was the only gate failing on the licence/ban front.
The choice was put to the copyright holder as: keep passkeys and scope exceptions; drop passkeys and
keep the policy pristine; or gamble on a pre-release `webauthn-rs` 0.6 that might drop the edge.
**Keep passkeys and scope the exceptions was chosen.**

## Decision

1. **Relax the OpenSSL ban for exactly one path, using `wrappers`.** The `deny.toml` bans on
   `openssl` and `openssl-sys` now list the WebAuthn crates that are permitted to pull them
   (`webauthn-attestation-ca`, `webauthn-authenticator-rs`, `webauthn-rs-core`, and `openssl` itself
   for `openssl-sys`). Pulled in by **any other** crate, OpenSSL still fails the build. `native-tls`
   stays banned outright - nothing needs it. **Moso's own TLS remains rustls**; this exception is
   scoped to attestation-certificate parsing, not to a transport.
2. **Allow the MPL-2.0 and CDLA-Permissive-2.0 licences as per-crate exceptions**, not allowlist
   entries. `deny.toml`'s `[licenses].exceptions` names each of the seven crates individually, so the
   allowance is recorded against the crates that triggered it and cannot widen the policy for the
   graph.
3. **Passkeys are an off-by-default `passkeys` cargo feature** on `moso-auth`, re-exposed as an
   off-by-default `passkeys` feature on the `moso` facade. A default build of Moso therefore pulls no
   OpenSSL and needs no C toolchain; the exception's cost is paid only by a project that turns
   passkeys on. `cargo deny` judges `all-features`, so the exceptions above are still required for the
   gate - the feature gate is about the *build*, not the *audit*.

## Alternatives considered

**Drop passkeys entirely.** Keeps `deny.toml` pristine and removes the largest C-linked CVE surface
from the graph. Rejected because passwordless authentication is a headline feature of the auth
battery and the design document promises it; removing it to satisfy a transitive licence would be the
tail wagging the dog. Everything else in `moso-auth` (password, JWT, OAuth2/OIDC, TOTP, API keys)
would have stayed regardless.

**Wait for `webauthn-rs` 0.6.** The cache holds a `0.6.1-dev` prerelease. Depending on a `-dev`
version is unauditable and non-reproducible, it may still link OpenSSL, and it would churn the API.
Rejected as a gamble dressed up as diligence.

**Rewrite the ceremonies on a pure-Rust stack.** The RustCrypto-based passkey crates are
authenticator-side; there is no mature, audited relying-party library that avoids OpenSSL. Writing
attestation-certificate and COSE verification by hand would violate "never implement a cryptographic
primitive" - the exact rule the OpenSSL ban serves. Rejected.

## Consequences

- **The single-static-binary story now has an asterisk**, but only for opt-in passkeys:
  `cargo install`-ing or building a Moso app with `--features passkeys` needs OpenSSL headers and a C
  toolchain. Without the feature, nothing changes.
- **`45-security.md`'s "rustls only" is no longer literally true**, and it now reads "rustls only,
  with one wrapper-scoped exception recorded in ADR-0015". The `deny.toml` bans carry the same note
  inline, so a reader of either lands on this record.
- **The MPL-2.0 allowance is narrow and defensible.** MPL-2.0 is *file-level* copyleft with a
  §3.3 secondary-licence clause that makes combination under AGPL-3.0 explicit; it imposes no terms on
  Moso's own source. It is an exception rather than an allowlist entry precisely so a future
  MPL dependency is a fresh decision, not an inherited one.
- **`xtask check-deps` rule 6 (the third-party crate budget) benefits** from the feature gate: the
  WebAuthn subtree leaves the default-feature build.

## Reversal criteria

Reverse - by dropping passkeys and restoring the unqualified ban - if any of these appear:

- `webauthn-rs` ships a rustls/RustCrypto build (a released 0.6+ that drops the OpenSSL edge), at
  which point the OpenSSL exception is deleted and only the MPL licence exceptions remain.
- A security review flags the OpenSSL attestation path as an unacceptable CVE surface for the value
  passkeys deliver.
- A downstream adopter's licence policy rejects the MPL-2.0 transitive dependencies and the passkey
  feature is the only thing pulling them - in which case off-by-default already contains the blast
  radius, and dropping the feature is a one-line change to the facade.
