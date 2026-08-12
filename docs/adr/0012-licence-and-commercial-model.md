# ADR-0012 — Apache-2.0 OR MIT, permissive forever, first-party commercial products

Status: **Superseded by [ADR-0014](0014-agpl-relicence.md)** (2026-08-05)
Date: 2026-07-29

> Moso is licensed `AGPL-3.0-only`. The "permanent" permissive commitment recorded below was
> withdrawn while the project was unpublished and single-authored — the only window in which it
> could be. This record is kept unedited because the trail is the value; read
> [ADR-0014](0014-agpl-relicence.md) for what replaced it and what it costs.

## Context

Moso's scope is 2–3 engineer-years to 0.1 plus continuing maintenance. Unfunded projects of this
size stall — Rocket's five-year gap between 0.4 and 0.5 is the ecosystem's reference case, and
Actix's 2020 crisis showed what an unsupported sole maintainer under pressure looks like.

The frameworks Moso emulates all pair open source with a funding model: Laravel self-funded through
first-party commercial products (Forge, Vapor, Nova, Cloud) for over a decade before raising a
$57M Series A led by Accel in September 2024; FastAPI's creator was funded by the inaugural Sequoia
Open Source Fellowship and then founded FastAPI Labs; NestJS is sustained by consulting, an official
course, and sponsorship. Pavex is the Rust-native experiment in charging for the framework itself —
explicitly not open source, with planned usage pricing — and its reception is an important data
point we should watch rather than pre-empt.

## Decision

1. **Licence: Apache-2.0 OR MIT**, the Rust ecosystem standard. Permissive, and **permanent**.
2. **Contribution: DCO only.** No copyright-assignment CLA. A CLA is what makes a future relicence
   possible, and we are foreclosing that deliberately.
3. **Commercial model: first-party products alongside the OSS**, not open-core. Candidates in
   order: a managed deployment platform, hosted observability built on what the framework already
   emits, enterprise support with LTS, and training.
4. **A published promise in the README:**
   - No feature is ever removed from open source to create a paid tier.
   - Core functionality — ORM, auth, authorization, admin, jobs — is open source permanently.
   - No licence change, including on acquisition or wind-down.
   - Forks may exist under a different name (standard trademark policy, stated up front).

## Alternatives considered

- **Open-core.** Reliable revenue, but it requires deciding which users to disappoint, and the line
  moves under commercial pressure. Adoption of a framework depends on trust in exactly this.
- **Closed source / usage pricing (the Pavex model).** Poor fit for our positioning: our thesis is
  broad adoption by developers wary of lock-in, and the community reaction to Pavex has been
  intrigued but wary.
- **Sponsorship only.** Reliably funds about 0.2 FTE at this project's visibility. Insufficient.
- **Foundation.** Foundations govern well and fund early work poorly. Premature.

## Consequences

- Revenue arrives later than open-core would allow, so the runway to M4 must be secured another way
  (self-funding or grants). This is a real constraint on the roadmap and is stated in
  `05-delivery/50`.
- The promise constrains future product design, permanently and on purpose.
- If adoption metrics are not met by M5, we do **not** build the commercial product — a paid product
  on top of an unadopted framework is two failures instead of one. The alternative is the
  specialisation off-ramp in `05-delivery/52`.

## Reversal criteria

**The licence and the promise must not be reversed.** That is the point of writing them down. The
*products* may change freely; the licence and the open-source scope may not.
