# 52 - Governance & Sustainability

> The two formative failures of the Rust web ecosystem were both **single-maintainer failures**:
> Actix-web's 2020 collapse (a security debate turned toxic, the sole maintainer wrote "I am done
> with Open Source" and briefly threatened to delete the repositories) and Rocket's ~five-year
> stagnation between 0.4 and 0.5, which it only addressed by creating the RWF2 governance
> organisation after the fact.
>
> The frameworks Moso is trying to emulate - Laravel, FastAPI, NestJS - all pair open source with a
> funding model. This is not an afterthought chapter. It is a design constraint.

## Governance

### The bus factor rule

**Moso does not make a public 0.1 announcement with fewer than three maintainers holding commit
rights and release keys.** This is a hard gate in `50-roadmap.md § M4`. A framework asking companies
to bet production systems on it must be able to survive any one person leaving.

### Structure

| Role | Count | Responsibilities |
| --- | --- | --- |
| **Core team** | 3–7 | merge rights, release authority, RFC decisions, code of conduct enforcement |
| **Area owners** | 1 per major crate | reviews in their area, roadmap input; not necessarily core |
| **Contributors** | unbounded | PRs, issues, docs, triage |

Decisions are made by **lazy consensus**: an RFC with no unresolved objection after 10 days from at
least two core members is accepted. Objections must be substantive and must propose an alternative.
If consensus fails, a simple majority of the core team decides, with the dissent recorded in the
ADR. Recording dissent matters - it keeps disagreement in the open rather than in DMs.

### The RFC process

Required for: any breaking change; any new public trait; any new crate; anything affecting the
project layout `moso new` generates; anything affecting security defaults.

Not required for: bug fixes, docs, tests, internal refactors, additive non-trait APIs.

Template: motivation, guide-level explanation (as it would appear in the tutorial), reference-level
explanation, drawbacks, alternatives (with an honest case for each), prior art (what do FastAPI /
Django / Rails / Laravel do), unresolved questions, and **reversal criteria** - what would make us
undo this.

RFCs live in `rfcs/`, are numbered, and are merged whether accepted or rejected. A rejected RFC with
a written rationale is one of the most valuable artefacts a project can have; it stops the same
proposal returning every six months.

### Code of conduct

Contributor Covenant 2.1, with a **named enforcement contact who is not the project lead** and a
documented, timeboxed process. The Actix episode escalated in part because there was no structure
to absorb a heated technical dispute. A code of conduct that nobody administers is decorative.

### Maintainer wellbeing (stated policy, not sentiment)

- **No obligation to respond outside working hours.** Written in `AGENTS.md` so the norm is
  explicit rather than assumed.
- Issues get a triage label within 7 days; that is the only response SLA, and there is none for
  resolution. Setting expectations low and meeting them beats the reverse.
- A rotating triage duty so one person is not always the first responder.
- **Abusive interactions end the conversation immediately**, with no obligation to justify. The
  precedent that "everyone believes there is a large team behind this" - when there is not - is
  addressed by naming the team size on the README.
- Sabbatical policy: any maintainer may step back for up to three months without losing standing.

### Succession

Written down before it is needed: if a maintainer is unreachable for 90 days, the remaining core
team may reassign their permissions. Release keys are held by at least two people. The crates.io
owner list includes a project-owned account, not only individuals. Domains and infrastructure are
registered to an organisation account with documented recovery contacts.

## Sustainability & funding

### The problem

Moso's scope - a core, an ORM, migrations, auth, authz, jobs, an admin, a CLI, and tutorial-grade
docs - is not a nights-and-weekends project. It is 2–3 engineer-years to 0.1 and a continuing
maintenance load thereafter. Unfunded, it becomes Rocket: a strong start followed by a multi-year
gap that costs the community more than never starting.

### The models, assessed

| Model | Precedent | Fit for Moso |
| --- | --- | --- |
| **First-party commercial products** | Laravel (Forge, Vapor, Nova, Cloud) - self-funded for years before raising a $57M Series A led by Accel in Sept 2024, its first VC round in 13 years | **Best fit.** Aligns incentives: the OSS must be good for the paid product to sell. |
| **Funded creator + managed cloud** | FastAPI (Sequoia Open Source Fellowship, then FastAPI Labs / FastAPI Cloud) | Good fit, but depends on a specific fellowship/funding event we cannot plan around. |
| **Consulting + training + sponsorship** | NestJS (Trilon, the official course, Open Collective) | Sustainable but caps growth; the maintainer's time goes to consulting, not the framework. |
| **Pure sponsorship** | most Rust crates | Insufficient at this scope. Reliably funds ~0.2 FTE. |
| **Closed source / usage pricing** | Pavex (explicitly not open source, planned usage pricing) | Poor fit for our positioning. Adoption depends on trust, and Rust developers are wary. Watch Pavex's reception as the closest live experiment. |
| **Foundation** | - | Premature. Foundations govern; they rarely fund early work. |

### The recommendation

**Open-source core (`MIT` since [ADR-0018](../adr/0018-mit-relicence.md); no CLA), plus first-party
commercial products that do not cripple the OSS.**

Candidate commercial products, in order of fit:
1. **Moso Cloud** - deploy a Moso app with managed Postgres, Redis, workers, migrations-on-deploy,
   and preview environments. The natural product; the framework already generates the artefacts.
2. **Observability for Moso** - the trace/query/job views the framework already emits, hosted, with
   history and alerting. Low marginal cost, high perceived value.
3. **Enterprise support** - SLA, LTS branches, private advisory notice, upgrade assistance.
4. **Training and certification** - later, and only if demand appears.

Non-negotiable constraints on the commercial model, published as a **promise** in the README:

- The framework is never crippled to sell the product. No feature is removed from OSS to create a
  paid tier.
- No open-core split of core functionality (auth, ORM, admin are OSS forever).
- No licence rug-pull. The permissive licence is permanent; if the project is sold or wound down,
  the licence does not change.
- Trademark policy allows forks to exist under a different name (standard practice, stated up front).

Making this promise explicit and specific is itself a marketing asset in a market that has watched
several licence changes.

### Timeline

- **Now → M3:** self-funded or grant-funded. Apply to relevant open-source funding programmes early;
  they take months.
- **M4 (0.1 launch):** GitHub Sponsors + Open Collective live, with named corporate tiers. Modest,
  but it signals seriousness and captures early goodwill.
- **M5 → 0.2:** if adoption metrics from `00-foundations/01` are met, begin building the commercial
  product. If they are not, do **not** build it - a paid product on top of an unadopted framework is
  two failures instead of one.

### The specialisation off-ramp

If broad adoption stalls but a **specific vertical** shows traction, specialise there rather than
competing head-on. Candidate verticals visible today:

- **AI-agent-facing APIs.** Strong types + generated OpenAPI + an MCP server + conventions give an
  agent a checkable contract. A framework positioned as "the backend framework agents can write
  correctly" is a defensible, growing niche.
- **Rails/Django-refugee SaaS.** Teams that want Django's completeness with Rust's cost profile.
- **Regulated/embedded deployments.** Single static binary, no CDN dependencies, air-gap friendly,
  SBOM, audit log - properties we get almost for free and that enterprise buyers pay for.

The trigger for this decision is written into the M5 review, so it is made deliberately rather than
by drift.

## Community

- **Discord/Zulip** for conversation, **GitHub Discussions** for durable Q&A. Answers that matter
  are promoted into the docs - an answer given once in chat is lost.
- A weekly public changelog post from M1, even during private development. Building in public
  compounds; announcing at the end does not.
- A `good-first-issue` queue kept genuinely stocked (≥ 10 open at all times), each with a
  description of the fix and the file to change.
- Recognise contributors in release notes by name, not just a bot-generated list.
- **Deliberately no launch hype before M4.** A framework discovered before it is good gets one
  impression, and it is the wrong one. Rocket's arc is the reference.

## Acceptance criteria (WP-29)

1. Contributor rules live in `AGENTS.md`. `CONTRIBUTING.md`, `SECURITY.md`, `GOVERNANCE.md` and
   `CODE_OF_CONDUCT.md` were removed alongside the relicence
   ([ADR-0018](../adr/0018-mit-relicence.md)); if a code of conduct returns, it needs a named
   contact who is not the project lead.
2. Three maintainers with commit and release rights; release keys held by ≥ 2; crates.io ownership
   includes a project account.
3. The RFC process is live with at least three merged RFCs (accepted or rejected) before 0.1.
4. The licence is published in the README (`MIT`, ADR-0018). ADR-0012's commercial-model
   promise was superseded and no funding model currently stands in its place.
5. A funding decision is recorded in an ADR before the public announcement.
6. Succession policy documented and tested by a dry run (one maintainer's access is temporarily
   revoked and the others confirm they can still release).
