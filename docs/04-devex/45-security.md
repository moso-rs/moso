# 45 - Security Model & Hardening

> 🟡 **Status: the defaults are implemented; parts of the process are not.** Built: security
> headers, sensitive-header redaction, `expose_internal_errors` off in every profile with a boot
> warning when it is forced on, request limits enforced before allocation, `SecretString`/
> `SecretBytes` with `zeroize` and a redacting `Debug`, signed and private cookies,
> `trusted_proxies` empty by default so an unconfigured deployment never believes
> `X-Forwarded-For`, the `Csrf` guard, `LoginThrottle` and `AuthConfig::validate` in `moso-auth`,
> the canary-secret grep, `cargo-deny` and `cargo-audit` on every pull request and again nightly
> against a fresh database, and a release pipeline that cuts only from a signed tag and attests
> SLSA provenance for every binary.
> ⛔ Not built: `Slot::RateLimit` (a reserved, empty position - the limiter is a `moso-kv` route
> guard instead), fuzz targets (the nightly job probes for `fuzz/`, does not find it, and says so),
> the SBOM, the admin panel the "admin" rows below assume, TLS termination, and the external
> security review. Each is marked ⛔ in place. `moso check` and `moso deploy checklist` **have**
> landed since the ⛔ rows below were written; where a row still says otherwise it is corrected in
> place.

## Position

A batteries-included framework carries more security responsibility than a router, because users
will trust its defaults. Moso's rule: **the secure configuration is the default, and the insecure
one requires an explicit, logged opt-out.**

## Threat model

**In scope.** Untrusted HTTP clients; malicious authenticated users; multi-tenant data leakage;
credential attacks; supply-chain risk in our dependency tree; accidental exposure by the
application developer (leaking a password hash, forgetting an authorization check).

**Out of scope.** A compromised host or database server; malicious first-party code; physical
access; a compromised CI of the deploying organisation. Documented so users know what they own.

**Assumed operator responsibilities**, stated in the deployment docs: TLS termination - ⛔ Moso does
**not** terminate TLS in this release, and `ServerConfig::tls` is a reserved shape whose presence is
a boot error rather than a silent plaintext listener - network isolation of the database, secret
management, OS patching, and DDoS protection at the edge.

## Secure defaults (the table users should be able to point at)

| Area | Default |
| --- | --- |
| Body size limit | 2 MiB; 413 with a problem document |
| Request timeout | 30 s; 504 |
| Header count/size | 100 / 16 KiB |
| JSON nesting depth | 64 (billion-laughs / deep-nesting defence) |
| Query nesting depth | 8 |
| Multipart | streamed, per-file and total caps. ⛔ The extractor does no type sniffing; magic-byte sniffing and EXIF stripping live in `moso-storage`'s upload pipeline |
| CORS | **off**; must be configured explicitly. No `*` with credentials, ever - refused at build |
| Security headers | `X-Content-Type-Options: nosniff`, `Referrer-Policy: strict-origin-when-cross-origin`, `X-Frame-Options: DENY`, HSTS (omitted in `dev`), CSP `frame-ancestors 'none'` on **every** response. ⛔ No `Permissions-Policy` by default - `DENY_ALL_PERMISSIONS_POLICY` is one constant away - and no HTML-specific policy |
| Cookies | **Session cookie** (`moso-auth`): `HttpOnly`, `Secure` (relaxed in `dev` only, and only behind `auth.allow_insecure_cookies`), `SameSite=Lax`, `__Host-` prefix where possible, signed. **Any cookie written through `Cookies`** (`moso-core`): the same `HttpOnly` / `SameSite=Lax` / `Path=/`, and `Secure` in every profile but `dev` - `CookieDefaults` fills in only what the caller left *unset*, so an explicit `.secure(false)` is the escape hatch and stays visible in the diff. ⛔ The two disagree on one point: `moso-core` has no `http.allow_insecure_cookies`, so the `dev` relaxation there is not behind a second opt-in |
| CSRF | double-submit token on cookie-authenticated unsafe methods; exempt for `Authorization`-header requests. Built as the `Csrf` guard, so it documents its 403 and its header parameter |
| Passwords | argon2id, calibrated params, breach check, bounded blocking pool. ⛔ The corpus is an embedded seed list expanded into a Bloom filter, not a published breach list; `password::EMBEDDED_CORPUS_NOTE` says so |
| Sessions | ID cycled on login, epoch invalidation, idle + absolute timeouts. `SessionLayer` implements `CustomLayer`, so `MiddlewareStack::replace_custom(Slot::Session, ..)` installs it into its slot; ⛔ nothing installs it *for* you, and `moso new` generates no session wiring at all |
| JWT | EdDSA default; `alg` never trusted from the header - the verifier's algorithm is fixed at construction and the header's `alg` is only compared with it; HS256 opt-in through `allow_symmetric`, refused at boot without it |
| SQL | always parameterised; `sql!` binds, never concatenates; no string-formatted queries in any API |
| Errors | 5xx detail suppressed; problem+json; no stack traces in production |
| OpenAPI docs | disabled in the production profile by default |
| Admin | ⛔ there is no `moso-admin` crate; nothing to disable, gate or re-authenticate |
| Metrics | ⛔ no separate port. Cardinality is bounded instead, by `MetricsConfig::max_routes` and by labelling with the matched pattern |
| Rate limiting | ⛔ not on by default anywhere. `Slot::RateLimit` is reserved and empty, and `moso_auth::LoginThrottle` runs only on the routes whose `AuthState` was given one |
| Redirects | `next`/`redirect_uri` validated against an allowlist by `routes::validate_next`, which also re-checks the percent-decoded form; the allowlist itself is validated at boot |
| File uploads | EXIF stripped from JPEG and PNG, a scriptable SVG refused (and deleted, on the presigned path), served with an RFC 6266 `Content-Disposition` and `Content-Security-Policy: sandbox` |
| Unsafe code | `#![forbid(unsafe_code)]` in every crate |

## Injection defences

- **SQL.** There is no API in `moso-orm` that accepts a runtime string as SQL structure. `sql!` is a
  macro producing bind parameters; a dynamic-identifier need goes through `Ident::validate()`
  which allows only `[A-Za-z_][A-Za-z0-9_]*` against a known set. Raw execution via `db.pool()` is
  the user's responsibility and the docs say so at that call site.
- **Template.** ⛔ Nothing in the request path renders a template. `minijinja` is a `moso-mail`
  dependency only, and there is no template response type in the facade. `moso check` exists, but
  its ten lints are `layering`, `blocking_in_async`, `n_plus_one`, `stale_layer`,
  `unhandled_error_variant`, `undocumented_endpoint`, `route_not_in_document`, `env_example_drift`,
  `missing_authz` and `unknown_permission` - none of them is a `|safe` lint. So neither the
  autoescaping promise nor the `|safe` lint is a promise this framework currently makes. An
  application that renders HTML owns its own escaping.
- **Header.** Header values are constructed through `HeaderValue::from_str` with validation; CRLF
  injection is structurally impossible.
- **Log.** Log fields are structured, not formatted, so log injection cannot forge a line.
- **Path.** `StorageKey` rejects `..` and `.` as segments, absolute paths, empty segments and
  control characters. ⛔ Tested with unit tests, not with a fuzz corpus: there is no `fuzz/`.

## Multi-tenancy safety

The `#[entity(tenant)]` scope requirement (`02-data/24`) exists specifically because cross-tenant
leakage is the highest-severity bug a SaaS can ship. ⛔ It is enforced at query-build time rather
than by the type system: `Select::check_tenant` returns `Error::TenantMissing` for an entity that
`requires_tenant` when neither a tenant nor `across_tenants` was named. `ScopedPolicy` filters at
the query level and is proved against a real database to agree with `Policy`. ⛔ Postgres RLS is a
flag on `EntityDescriptor` that no migration emits, and there is no admin to scope.

## Supply chain

- `cargo deny check advisories bans licenses sources` runs on every pull request (G11).
- `cargo audit --deny warnings` runs on every pull request **and** nightly against a freshly fetched
  database, because advisories appear after merge.
- **Dependency budget** (`00-foundations/03`): ≤ 90 crates default, ≤ 260 full. Every new
  dependency requires justification in the PR; this is as much a security control as a build-time
  one. ⛔ The default budget is exceeded - `xtask check-deps` rule 6 counts 155 third-party crates
  against 90 - and is deliberately not being closed by raising the number.
- All Moso crates published with `cargo publish --locked`. A release runs only from a `v*.*.*` tag
  push, and every binary ships a SHA-256 checksum and a SLSA provenance attestation signed with the
  workflow's OIDC identity. ⛔ *Signing* the tag is a documented rule the pipeline does not verify:
  `gh release create --verify-tag` checks that the tag exists, not that it carries a signature.
- ⛔ No SBOM. Nothing generates one per release, and `moso deploy checklist` reports risks rather
  than emitting an SBOM for a user's application.
- `#![forbid(unsafe_code)]` everywhere, with any exception requiring an ADR, a benchmark
  justification, and a Miri test. The Actix 2020 unsafe controversy is the cautionary tale we
  design against.

## Vulnerability handling

Report privately through **GitHub Security Advisories** on the repository. There is no committed
`SECURITY.md` - it was removed with the MIT relicence ([ADR-0018](../adr/0018-mit-relicence.md)) - and
no `security@moso.rs` alias is registered yet:

- Private reporting via GitHub Security Advisories; a monitored `security@` alias.
- **Acknowledge within 48 h, triage within 5 days, fix target 30 days** for high severity.
- Coordinated disclosure, CVE assignment, RUSTSEC advisory filed.
- A security mailing list and a GitHub Discussions security category for announcements.
- Patch releases for the current minor and the previous minor.

This process existing *before* launch is non-negotiable, given that the ecosystem's formative trauma
was a security disclosure handled badly under pressure.

## Cryptography policy

- We do not implement primitives. RustCrypto and `ring`/`aws-lc-rs` only.
- Key material is `SecretString`/`Zeroizing`; keys are never logged, never in `Debug`.
- Signed cookies and cursors use HMAC-SHA256 with domain separation (each use gets a distinct
  derived key via HKDF with a context label, so a cookie signature cannot be replayed as a cursor).
- Key rotation is designed in: config holds `secret_key` plus `previous_keys`, verification tries
  all, signing uses the current one.
- Randomness from the OS CSPRNG only. No `rand::thread_rng` for security-relevant values.
  ⛔ There is no `moso::crypto` module and no `moso::crypto::random()`: the sanctioned source is
  `ring::rand::SystemRandom`, reached through `moso-auth`'s crate-private `random_bytes`, which
  returns `Error::Unavailable` rather than panicking when the operating system refuses.

## Denial of service

| Vector | Mitigation |
| --- | --- |
| Slowloris | ⛔ the 30 s request timeout and hyper's own defaults only. There is no header-read timeout and no connection cap in `ServerConfig`; this is the edge's job |
| Large bodies | streamed limits before allocation |
| Compression bomb (request) | ⛔ no cap, because Moso does not decompress request bodies at all. `Content-Encoding` is a response concern here |
| Password-hash flood | bounded blocking pool; `LoginThrottle` where it is configured |
| Regex catastrophic backtracking | `regex` crate, linear-time by construction, so backtracking is structurally impossible. ⛔ Nothing additionally rejects a "pathological" user-supplied pattern, because with that engine there is nothing to reject |
| Unbounded queries | default `LIMIT` on a `Select` (`DEFAULT_ROW_LIMIT`, 10 000), with `.unlimited()` as the explicit opt-out |
| Cache stampede | single-flight, in `moso-kv`'s `cached!` macro (a macro, not a `#[cached]` attribute) |
| Connection exhaustion | pool caps + `database.acquire_timeout` → 503 |
| Metric cardinality | the `route` label is the matched pattern, and `MetricsConfig::max_routes` folds everything past the cap into one series |
| Job queue flooding | per-queue depth limit that pauses low-priority pulls, and priority classes |

The `fetch_all` default limit deserves a note: it is a deliberate paternalism. An unbounded
`SELECT *` that works in dev and OOMs in production is a rite of passage we are choosing to prevent.
It logs a warning naming the query when the cap is hit, and `.unlimited()` opts out explicitly.

## `moso deploy checklist`

Built. `moso deploy checklist` resolves the configuration the application would resolve under the
production profile (`--profile` overrides it), reads the project on disk, and exits non-zero on any
failed check. It deploys nothing and writes nothing. `moso config --generate-secret` is real too:
32 bytes from the OS CSPRNG, base64, printed to standard output and nowhere else.

⛔ One line of the illustrative output below is still fiction: there is no `moso-admin` crate, so no
check reports on an admin path.

```
$ moso deploy checklist
  ✗ openapi.expose = true in production          → set false, or gate behind auth
  ✗ secret_key is the development default        → moso config --generate-secret
  ⚠ http.expose_internal_errors = true           → leaks internals in 500 responses
  ⚠ database.max_connections=50 × 20 replicas    → 1000 > typical Postgres max (100)
  ⚠ no CSP configured for HTML responses
  ✓ TLS terminated upstream (X-Forwarded-Proto trusted from 10.0.0.0/8)
  ✓ cookies Secure + HttpOnly + SameSite
  ✓ 0 known advisories in 214 dependencies
```

## Security testing in CI

- ⛔ The adversarial admin test: there is no admin.
- ⛔ Traversal fuzzing of path handling, and fuzzing of the query-string, cursor and multipart
  parsers. The nightly job that would run them warns that `fuzz/` is absent (G20).
- A canary-secret grep over the entire test-suite log and error output. CI exports
  `MOSO_CANARY_SECRET` and fails the run if it appears anywhere in the output.
- A test asserting every security header is present on a representative response.
- CSRF, session-fixation and open-redirect regression tests, in `moso-auth`.
- `cargo deny` + `cargo audit` gates, per pull request and nightly.

## Acceptance criteria (WP-28)

1. Every default in the defaults table is asserted by a test - for every default that exists; the
   rows marked ⛔ above have nothing to assert.
2. ⛔ There is no committed `SECURITY.md` (removed with the MIT relicence); private reporting is
   through GitHub Security Advisories, and the `security@` alias is not registered yet.
3. ✅ `#![forbid(unsafe_code)]` is present in every crate; the `hygiene` job fails if removed.
4. 🟡 `moso deploy checklist` detects each listed condition in a purpose-built fixture project. The
   command ships with 25 unit tests and is driven end to end against a freshly generated project in
   `crates/moso-cli/tests/new_builds.rs`; the fixture that exercises *every* condition does not
   exist yet.
5. ⛔ Fuzz targets run in CI nightly with no crashes over 10 M iterations. The job runs; there are
   no targets, and the budget it would run them under is five minutes each, not 10 M iterations.
6. ✅ The canary-secret grep passes over the full test suite.
7. ✅ A permissive CORS config combined with credentials fails at build with an explanation.
8. 🟡 Signed artefacts are produced by the release pipeline - a signed tag, a SHA-256 checksum and a
   SLSA provenance attestation per binary. ⛔ No SBOM.
