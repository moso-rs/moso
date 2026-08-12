# ADR-0018 — MIT, superseding the AGPL relicence

Status: Accepted
Date: 2026-08-12
Deciders: Alessandro Zucchiatti
Supersedes [ADR-0014](0014-agpl-relicence.md).

## Context

[ADR-0014](0014-agpl-relicence.md) moved Moso from `Apache-2.0 OR MIT` to `AGPL-3.0-only` to protect
the framework's source with a network-copyleft clause. That record was unusually candid about the
cost: it stated plainly that "the most likely outcome of plain AGPL on a web framework is that the
target user cannot adopt it," because AGPL §13 reaches an application that links Moso and is served
over a network, and Moso's primary persona is exactly that developer — someone shipping a commercial
product API. It listed, as its own reversal criteria, "reverse — to a linking exception, to LGPL/MPL,
or back to permissive — if an evaluator declines Moso citing the licence" and "if the project decides
to pursue a funding model that needs permissive adoption upstream of it."

This decision takes the simplest of those exits: **plain MIT**. Adoption and zero-friction reuse are
chosen over source protection. MIT — not `Apache-2.0 OR MIT` — because the goal is the least possible
licence surface for a would-be user to reason about: one short permissive file, the Rust ecosystem's
most common default, and nothing a corporate scanner blocks.

The conditions that made ADR-0014 cheap to make still hold, and make this equally cheap: **nothing is
published** (unreleased `0.1.0`, no crate on crates.io) and there is **exactly one copyright holder**
(no external contributions merged), so no third party's assent is required.

## Decision

1. **Licence: `MIT`.** Declared once in `[workspace.package]` and inherited by every member crate.
   The canonical text is `LICENSE` at the repository root (the standard MIT template, © 2026
   Alessandro Zucchiatti).
2. **`deny.toml`'s allowlist becomes MIT-centred.** `AGPL-3.0-only` is removed from the allowed set
   and `MIT` takes the workspace slot; the permissive allowlist otherwise stands. The blanket refusal
   of copyleft *dependencies* is relaxed in rationale — an MIT work has no incompatibility to protect
   against — but the allowlist stays curated rather than opened wide, so a new licence in the graph is
   still a reviewed decision. The scoped WebAuthn exceptions from [ADR-0015](0015-webauthn-openssl-exception.md)
   (MPL-2.0, CDLA-Permissive-2.0) persist unchanged; they were always compatible and are more obviously
   so under MIT.
3. **No CLA; no DCO ceremony.** `CONTRIBUTING.md` is removed along with the sign-off requirement it
   documented. Contributions are inbound-MIT by the presence of the licence; the project is small
   enough that this is the honest posture.

## Alternatives considered

**Keep AGPL (the status quo from ADR-0014).** Protects the framework's source and forecloses a
vendor taking it closed. Rejected for the reason ADR-0014 already anticipated: it blocks the target
user, no competing Rust framework (Axum, Actix, Rocket, Loco, Poem) carries network copyleft, and the
switching cost for an evaluator who sees AGPL is zero.

**AGPL with a linking exception, or LGPL/MPL.** ADR-0014 called the linking exception "the strongest
alternative." It preserves some source protection while letting applications link freely. Rejected
here only because the objective is *minimal* licence surface: a linking exception is a paragraph a
reader must understand, and MPL/LGPL are file/library copyleft a scanner still flags. Simplicity won.

**`Apache-2.0 OR MIT` (the original ADR-0012 terms).** The Rust ecosystem's belt-and-suspenders
default; Apache-2.0 adds an explicit patent grant. Rejected in favour of MIT alone because a single
short file is the least a user has to think about, which is the whole point of this reversal.

## Consequences

- **The target user can adopt Moso freely.** A closed-source commercial product API built on Moso has
  no source obligation. This restores the adoption story ADR-0012 wanted and ADR-0014 knowingly gave
  up.
- **The anti-vendor protection is gone.** A vendor can take Moso, extend it privately, and offer it as
  a hosted service without contributing back. That is the accepted price of MIT, and for a young
  framework whose first risk is *no adoption*, it is the right trade.
- **Ecosystem friction disappears.** MIT is not blocked by corporate policy scanners, and a
  permissively-licensed crate can depend on Moso.
- **`docs/adr/0012` and `0014` remain on file.** The trail — permissive → AGPL → MIT — is the value;
  a reader sees the reasoning at each step, including the protection that was weighed and released.

## Reversal criteria

MIT is maximally permissive, so *loosening* is not a thing that can happen to it; the only move left
is to *tighten* back toward copyleft, and that is a one-way door once the project grows:

- Returning to AGPL/LGPL/MPL after the first external contribution is merged, or after a crates.io
  publish, requires **every contributor's agreement** — the same asymmetry ADR-0014 recorded, now
  pointing the other way. So if a copyleft posture is ever wanted, decide it **before** accepting the
  first outside pull request or publishing, not after.
- Concretely, reverse only if the project pivots to a funding model that needs source protection (an
  open-core or dual-licence strategy) *and* it is still single-authored and unpublished at that moment.
