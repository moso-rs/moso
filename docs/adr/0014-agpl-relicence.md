# ADR-0014 — AGPL-3.0-only, superseding the permissive-forever commitment

Status: Superseded by [ADR-0018](0018-mit-relicence.md)
Date: 2026-08-05
Deciders: Alessandro Zucchiatti
Supersedes [ADR-0012](0012-licence-and-commercial-model.md).

> **Superseded.** Moso is now `MIT` — see [ADR-0018](0018-mit-relicence.md). The AGPL decision below
> is kept for the trail: it records the source-protection posture that was weighed and, one week
> later, released in favour of adoption. Its own reversal criteria anticipated exactly this move.

## Context

[ADR-0012](0012-licence-and-commercial-model.md) chose `Apache-2.0 OR MIT` and called it
**permanent**, with DCO-only contribution precisely so that a future relicence would be
impossible. That commitment is being withdrawn before it ever bound anyone, and this record exists
so the reversal is legible rather than discovered in a diff.

The conditions that make the reversal possible are narrow and will not recur:

- Nothing has been published. The workspace is an unreleased `0.1.0`; no crate exists on crates.io,
  so no downstream user has relied on the previous terms.
- There is exactly one copyright holder. The repository has two commits by one author and no
  external contributions, so no third party's permission is required. ADR-0012's own reasoning —
  that DCO-without-CLA forecloses relicensing — holds only once contributions from other people
  exist. Today they do not.

**This is the last moment at which this decision is cheap.** The first outside contribution merged
under AGPL, or the first crates.io publish, makes it expensive; the first of either that happens
under a permissive licence would make reversing *back* to permissive terms require every
contributor's assent.

## Decision

1. **Licence: `AGPL-3.0-only`** (GNU Affero General Public License v3.0), declared once in
   `[workspace.package]` and inherited by every member crate. The canonical licence text is
   `LICENSE` at the repository root.
2. **No linking exception**, for now. Moso ships plain AGPL-3.0. See *Consequences*, which is where
   the whole cost of that sits.
3. **`LICENSE-APACHE` and `LICENSE-MIT` are removed**, along with `CHANGELOG.md`,
   `DEPENDENCIES.md`, `GOVERNANCE.md` and `CODE_OF_CONDUCT.md`.
4. **Inbound dependencies stay permissive.** `deny.toml` keeps its permissive allowlist and adds
   only `AGPL-3.0-only`, for Moso's own crates. Apache-2.0 and MIT are one-way compatible *into*
   AGPL-3.0, so the existing dependency graph is unaffected; a copyleft *dependency* is still
   refused, because that would impose terms inherited from a transitive crate rather than chosen
   here.
5. **ADR-0012's commercial-model section is not carried forward.** It is superseded along with the
   rest of that record, and no funding model currently stands in its place.

## Alternatives considered

**Apache-2.0 OR MIT (the status quo).** The Rust ecosystem standard, and the reason every crate in
Moso's dependency graph can be linked without thought. It maximises adoption and forecloses the
open-core and hosted-service business models entirely. Rejected by this decision, but it is the
option with the strongest case on adoption grounds and the roadmap's own success metrics were
written assuming it.

**AGPL-3.0 with a linking exception** (the GCC Runtime Library Exception shape): copyleft on Moso
itself, explicitly not reaching applications that merely link it. This is what most projects mean
when they say "AGPL" for a *library*, and it preserves the framework's adoption story while
protecting the framework's own source. **It is the strongest alternative and was not taken only
because plain AGPL was asked for.** Adopting it later is a strictly loosening change and therefore
remains available.

**LGPL-3.0 or MPL-2.0.** File-level or library-level copyleft with no network clause. Weaker
protection than AGPL, and no obligation triggered by SaaS deployment; MPL in particular is well
understood and uncontroversial in commercial review.

**Dual licence: AGPL plus a paid commercial licence.** The MongoDB/Sentry shape. It is the only
option here that funds the work ADR-0012 was worried about, and it requires a CLA — which the
project does not have and which imposes real friction on contributors.

## Consequences

**The intended one.** Modifying Moso and offering it over a network obliges the operator to offer
their modified source. A vendor cannot take the framework closed.

**The unintended one, stated plainly.** Under the FSF's reading, an application that links Moso
forms a combined work, and AGPL §13 reaches it across the network boundary. A company shipping a
product API on Moso would have to release that application's source. That directly contradicts
`00-foundations/00-vision.md`, whose primary persona is "the FastAPI refugee" — a developer
shipping commercial product APIs — and whose adoption metrics (production deployments, a company
hiring for "Moso experience") assume they can. **The most likely outcome of plain AGPL on a web
framework is that the target user cannot adopt it.** No competing Rust framework carries a network
copyleft: Axum, Actix, Rocket, Loco and Poem are all permissive, so the switching cost for an
evaluator is zero.

**Ecosystem friction.** Many organisations block AGPL dependencies by policy, at the scanner, with
no appeal. Moso cannot be depended on by a permissively-licensed crate. `docs/05-delivery/50-roadmap.md`
and `52-governance.md` still describe an adoption-led strategy that this decision undercuts, and
they have not been rewritten to match.

**Process debt.** `CONTRIBUTING.md` and several documents referenced the removed files; those
references have been updated or dropped. CI gate G19 (a changelog entry per user-visible change) is
retired with `CHANGELOG.md`, and `docs/05-delivery/53-quality-gates.md` records that.

## Reversal criteria

Reverse — to a linking exception, to LGPL/MPL, or back to permissive — if any of these appear:

- An evaluator declines Moso *citing the licence*, twice. One data point is noise; two is a
  pattern, and it is the pattern ADR-0012 was written to avoid.
- A legal review by a would-be adopter concludes the combined-work reading applies to their
  application and they walk.
- Six months after a public 0.1 with zero production deployments outside the maintainers, while the
  core-loop feedback from `50-roadmap.md`'s M1 gate is otherwise positive — that isolates the
  licence as the cause.
- The project decides to pursue a funding model that needs permissive adoption upstream of it.

Adding a linking exception is a loosening change and needs only the copyright holder's assent while
that remains one person. **Returning to permissive terms after outside contributions are merged
requires every contributor's agreement** — so if reversal is plausible, decide it before accepting
the first external pull request, not after.
