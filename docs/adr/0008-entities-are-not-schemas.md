# ADR-0008 - Entities do not implement `Schema`

Status: Accepted
Date: 2026-07-29

## Context

The convenient thing is to return an entity straight from a handler. The dangerous thing is that
entities carry fields that must not be exposed: password hashes, internal flags, soft-delete
timestamps, encrypted columns, tenant ids.

Every framework that allows it produces a steady stream of data-exposure incidents, and every
framework that forbids it produces a steady stream of complaints about DTO boilerplate.

## Decision

`#[derive(Entity)]` does **not** implement `Schema`. Returning an entity from a handler is a compile
error with a hand-written message that shows the fix.

To remove the boilerplate objection, `#[schema(from = Entity)]` generates the `From` impl by
field-name matching and fails at compile time if a field is missing or mistyped. Writing an output
DTO becomes three lines, and it stays correct when the entity changes.

`#[entity(expose)]` opts a genuinely public entity out, for the cases where the entity really is the
public shape (a lookup table of countries, say).

## Alternatives considered

- **Allow it, and provide `#[entity(skip_serializing)]` per field.** Rejected: the default is
  unsafe, and the failure mode is silent. A forgotten attribute leaks a password hash; a forgotten
  DTO produces a compile error.
- **Allow it with a lint.** A lint you can ignore is not a boundary.

## Consequences

- Every resource has three types: entity, input DTO, output DTO. This is more typing than returning
  the entity, and we accept that cost - the generators write all three.
- The compiler enforces a layering discipline that would otherwise depend on code review.
- `Related<T>::NotLoaded` interacts well with this: a DTO built from a partially loaded entity fails
  at compile time or returns `NotLoaded`, rather than silently serialising an empty list.

## Reversal criteria

- If field-level `From` generation proves inadequate for common shapes (nested relations,
  computed fields) and users route around the rule with `#[entity(expose)]` at scale, revisit -
  the fix would be better conversion generation, not removing the boundary.
