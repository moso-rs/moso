# ADR-0019 - The default `/docs` is the real Swagger UI, vendored and self-hosted

Status: Accepted
Date: 2026-08-19
Deciders: Alessandro Zucchiatti

## Context

`moso-openapi/src/ui.rs` documented a firm decision and enforced it with two unit tests
(`template_loads_nothing_from_the_network`, `rendered_document_is_balanced`): Moso ships **one**
documentation renderer it controls - a single self-contained HTML page with inlined CSS and vanilla
JavaScript - and the `scalar`, `redoc` and `swagger-ui` cargo features only select which *route* that
one page answers on. The rejected alternative was named explicitly: *"Vendoring the real Scalar, ReDoc
or Swagger UI bundles would add megabytes of third-party JavaScript to every Moso binary; shipping one
good renderer we control is the better trade, and it is the only version of the promise 'works
air-gapped' that we can actually keep."*

Two things pushed against that. First, the feature names lie: a developer who enables `swagger-ui`
expecting FastAPI's `/docs` gets a Moso-built page that looks nothing like Swagger UI - a genuine
surprise reported in use. Second, the familiar tool has real value: Swagger UI is what the target
persona already knows, and matching it lowers the cost of adopting Moso. The owner decided the
default documentation experience should be the genuine Swagger UI.

The one part of the original rationale worth keeping is **air-gapped operation**. FastAPI's own
default loads Swagger UI from a CDN (`jsdelivr`), which fails in exactly the environments that most
need working docs: air-gapped deployments, TLS-intercepting proxies, and CI. That downside is
avoidable by self-hosting the assets rather than fetching them.

## Decision

1. **The default `/docs` serves the real Swagger UI.** The `swagger-ui-dist` 5.17.14 bundle
   (`swagger-ui.css` + `swagger-ui-bundle.js`, Apache-2.0) is vendored under
   `crates/moso-openapi/vendor/swagger-ui/`, `include_bytes!`-embedded by the new
   `moso_openapi::swagger_ui` module, and served on same-origin sub-paths of the docs route
   (`/docs/swagger-ui.css`, `/docs/swagger-ui-bundle.js`). **No CDN**: the rendered page names no
   absolute URL, so the air-gapped promise holds. A new test,
   `the_page_names_no_external_url`, keeps that line, exactly as the compact renderer's tests did.

2. **The compact renderer is retained behind `lean-docs`.** `moso_openapi::ui` is unchanged and still
   correct; the new off-by-default `lean-docs` feature puts it back at `/docs` for builds that prefer
   a smaller binary with no third-party JavaScript. The `redoc` and `swagger-ui` routes continue to
   mount the compact renderer. Both renderers are network-free; the axis is binary size versus
   familiarity, not offline-capability.

3. **The Swagger UI page carries a slightly looser CSP.** Swagger UI sets element styles from
   JavaScript at runtime, which `style-src` can only admit with `'unsafe-inline'` - a nonce cannot
   cover styles a script injects. The page therefore ships
   `script-src 'self' 'nonce-<n>'; style-src 'self' 'unsafe-inline'`, where the compact renderer kept
   `script-src 'nonce-<n>'; style-src 'nonce-<n>'`. This is confined to `dev` and `test`: the
   documentation page is never served in the production profile (`http.expose_docs`). The compact
   renderer keeps its strict, nonce-only policy under `lean-docs`.

## Alternatives considered

- **Keep the in-house renderer, only fix the misleading feature names.** Rejected: it does not give
  the user the actual tool they asked for; the surprise is the point, not the naming.
- **Load Swagger UI from a CDN, like FastAPI's default.** Rejected: it breaks the one principle worth
  keeping. Self-hosting the same assets yields the identical UI with none of the offline fragility.
- **Vendor all three real UIs (Swagger UI, ReDoc, Scalar) now.** Deferred, not rejected: Swagger UI
  is the requested one and the default. Real ReDoc at `/redoc` is a reasonable follow-up; until then
  the `redoc`/`swagger-ui` routes render the compact renderer, and `scalar` is a compatibility no-op.

## Consequences

- **Binary size, not crate count.** The embedded assets add ~1.6 MB to a build that mounts `/docs`
  (opt out with `lean-docs`). They add **zero** crates, so the `xtask check-deps` rule-6 budget is
  unaffected - the size axis this touches is the linked binary, not the dependency graph.
- **A documented security posture is relaxed on the dev-only docs page** (the CSP above). It never
  reaches production. Recorded here so the change is a decision on the record, not a drift.
- **The feature names remain imperfect.** Real Swagger UI is now the *default* `/docs`; the
  `swagger-ui` feature still mounts the *compact* renderer at `/swagger`. A future ADR may rename or
  retire `scalar`/`redoc`/`swagger-ui` once real ReDoc lands. This ADR does not change their routes,
  to keep the diff small and their existing tests green.
