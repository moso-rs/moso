# ADR-0017 - Four seams where `moso-auth` stops and the application continues

Status: Accepted
Date: 2026-08-12
Deciders: Alessandro Zucchiatti

## Context

A battery is defined as much by what it refuses to do as by what it does. Four places in `moso-auth`
hand a concern back to the application rather than absorbing it, and each was questioned as a possible
gap. None is: each is a boundary drawn by a framework rule (the extraction invariants, the
middleware/extractor split, the downward-only dependency graph), and drawing it anywhere else would
cost more than the seam. This record states all four together because they share one thesis - *the
abstraction stops at the point where absorbing the next step would violate a framework invariant, and
exposes a seam there* - and because none is large enough to earn a page of its own.

The four are individually RFC-shaped: moving any of them is a change to a public boundary, not a bug
fix. Recording them keeps each from being "fixed" by a later contributor who reads the seam as an
oversight.

## Decision

### 1. `LoginThrottle` is a service-layer check, not a `Guard` (item 10.11)

A `Guard` sees only the request *parts*: `Guard::check(&self, parts: &Parts, ctx)`
(`moso-core/src/middleware/mod.rs`). The throttle's per-identity tier keys on a field of the request
*body* - the identity being logged in as - so it cannot run from a guard without a body-reading guard,
which the extraction invariants forbid (a guard that reads the body would race the one body extractor
and break "at most one body extractor, last"). The throttle therefore runs as the **first `await`
inside the handler**, before any password hash is computed. That ordering is load-bearing: it is what
stops a refused attempt from paying for an argon2 hash. The cost is that the 429 is declared per route
group by hand rather than derived from a guard's `describe` - which is exactly the kind of hand-written
group-level documentation `moso-auth`'s mounted routes already carry (see ADR-0016).

**Decision:** the throttle stays a service-layer call. No `Guard`, no body-reading guard variant.

### 2. `SessionLayer` reads the cookie as a `CustomLayer`, not through the extractor cookie jar (item 16.5)

`SessionLayer` fills `Slot::Session`, a `CustomLayer` position that is `Route -> Route`
(`session.rs`, `middleware/mod.rs`). It parses the request cookie, builds the lazy `Session`, and
writes `Set-Cookie` on the way out, all at the tower layer - *outside* the `RouteHandler` and the
`RequestCtx` the extractor-level `CookieJar` lives in. It cannot use that `CookieJar` without becoming
a `Dependency` or `Guard`, because that jar is resolved per request inside extraction, one layer in
from where the session cookie must already have been read to make the `Session` available to
extractors. There is **no behavioural conflict today**: the session owns exactly one cookie name and
reads/writes it directly, and the extractor `CookieJar` an application uses for its own cookies does
not touch it. The open question is purely architectural - whether a future reworking should let the
session participate in a single request-scoped cookie-jar abstraction so both agree about outgoing
`Set-Cookie` headers.

**Decision:** the session stays a `CustomLayer`. Unifying it with the extractor cookie jar is
RFC-shaped and deferred; it changes a public boundary and buys nothing until a second concern needs to
coordinate `Set-Cookie` with the session.

### 3. No bundled i18n; `MessageProvider` is the seam (item 16.6)

Validation messages render through `MessageProvider`; `DefaultMessages` is the bundled English
terminal provider, and `ChainedMessages` lets an application override a handful of codes and fall
through to it (`moso-schema/src/message.rs`). There is no bundled Fluent, ICU, or other i18n stack:
`Locale` is a deliberately thin BCP 47 wrapper, not a CLDR implementation, and the message renderer is
hard-coded English. An application that wants translated or reworded messages registers its own
`MessageProvider` with `.provide_dyn::<dyn MessageProvider>(…)`.

**Decision:** do **not** bundle Fluent or any i18n runtime. The seam already exists and is
dyn-compatible; a heavy localisation dependency would be paid for by every application to serve the
subset that translates. `DefaultMessages`' documentation points at `MessageProvider` as the place an
application supplies its own.

### 4. Security- and change-notification mail is produced as intent and sent by the application (item 10.12)

`moso-auth` must not depend on `moso-mail` (`xtask/allow/dep-edges.toml` declares `auth -> [orm, kv]`
and no edge to mail), so it cannot send email. Instead it produces the *intent* and hands it to a
sink the application wires:

- Token deliveries (verification, reset, magic link, email-change confirmation) are `Delivery`
  values carrying a typed `DeliveryPurpose`, handed to `AuthState::deliver`, which calls the
  registered `TokenSink` or, with none registered, logs a `WARN` naming `AuthState::token_sink`
  (`routes.rs`). The routes are wired to it: `password`, `magic_link`, and the email-change flow all
  call `state.deliver(..)`.
- An email change carries `EmailChange::notify_previous`, the **old** address that should be told a
  change was requested - the breach/change-notification intent, produced beside the confirmation
  token (`lifecycle.rs`).
- A credential-stuffing signal is a `SecurityNotice`, assembled by `LoginThrottle::notice` at the same
  moment it claims the notify-once marker, and handed to a `NoticeSink` (the same shape as
  `TokenSink`). A `SecurityNotice` carries no token and has no `expose()`, so an alert sink can never
  be handed a live credential - the same structural separation `DeliveryPurpose` keeps from
  `TokenPurpose`.

The intent is **reachable, not stranded**: the routes emit it and the sinks deliver it; only the
transport is the application's, because the transport is what would pull the dependency the graph
forbids.

**Decision:** mail stays app-side by design. `moso-auth` produces `Delivery`, `EmailChange::notify_previous`
and `SecurityNotice`; the application sends them through its own Outbox/Mailer via the `TokenSink` and
`NoticeSink` hand-offs. This is documented as the design, not owed as a gap.

## Alternatives considered

- **A body-reading guard for the throttle.** Rejected: it breaks the single-body-extractor invariant
  and would run before the handler could read the body it needs, reintroducing the very race the
  invariant deletes.
- **Make the session a `Dependency`/`Guard` so it uses the extractor cookie jar.** Rejected for now:
  no behavioural problem exists, and the session must read its cookie before extraction runs, which is
  where a dependency-resolved jar lives. A rework is RFC-shaped and unmotivated today.
- **Bundle Fluent** so translations are turnkey. Rejected: a heavy, always-compiled dependency to
  serve the minority that localises, when the dyn-compatible seam already lets them plug in exactly
  the stack they want.
- **Add an `auth -> mail` edge** so the battery sends its own mail. Rejected: it decides for every
  user who wants auth that they also compile a mailer, which is exactly the trade
  `xtask/allow/dep-edges.toml` exists to force into the open; the sink shape keeps the choice with the
  application.

## Consequences

- The throttle's 429 is documented by hand per route group, consistent with ADR-0016's treatment of
  the mounted routes.
- An application's own cookies and the session cookie are managed by two different mechanisms (the
  extractor `CookieJar` and `SessionLayer`); they do not conflict because they touch different cookie
  names, and a contributor who wants to unify them must write an RFC first.
- Untranslated deployments get correct English messages with zero configuration; translated ones
  register a `MessageProvider`. No application pays for an i18n runtime it does not use.
- An application that wants verification email, reset email, or security alerts to actually be sent
  must register a `TokenSink` and a `NoticeSink`; with none registered, the intent is dropped with a
  `WARN` that names the builder call, never silently.

## Reversal criteria

- **Throttle (1):** reverse if the extraction model ever grows a first-class "body-aware guard" for
  reasons unrelated to auth; the throttle should then move onto it so its 429 is derived rather than
  hand-written.
- **Session (2):** reverse if a second concern needs to coordinate outgoing `Set-Cookie` with the
  session, or if the session must expose itself to the OpenAPI document as a security requirement - at
  which point unify it with a request-scoped cookie-jar abstraction under an RFC.
- **i18n (3):** reverse if a bundled default-locale set beyond English becomes a common enough request
  that shipping one (still behind the `MessageProvider` seam, still off the hot path) beats every
  application wiring its own. The seam does not change; only whether Moso ships more than one terminal
  provider.
- **Mail (4):** reverse only if the downward-dependency rule itself is revisited (it will not be
  lightly); the sink hand-off is the correct shape for as long as `auth` must not reach `mail`.
