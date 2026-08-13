# moso-orm-macros

The procedural macros of [Moso](https://github.com/lowsbarrel/moso)'s ORM:
`#[derive(Entity)]`, `#[derive(Projection)]`, `#[derive(Embedded)]` and
`#[derive(DbEnum)]`.

This crate depends on no runtime Moso crate (dependency rule 1). Generated code
resolves against `::moso::__private::*` and nothing else (decision D6), so the
runtime layout can move without touching a macro - and the derives are
re-exported by the **facade**, not by `moso-orm`.

Use them through `moso`:

```rust,ignore
use moso::db::prelude::*;

/// A tag on a post.
#[derive(Entity, Clone, Debug)]
pub struct Tag {
    /// The primary key.
    #[entity(pk)]
    pub id: Id<Tag>,
    /// The display name.
    #[entity(unique)]
    pub name: String,
}
```

## Status

**Implemented.** The attribute vocabulary is parsed and frozen, with tests over every container and
field setting, the "did you mean" suggestions and the refusals - and each derive now generates real
code that resolves against `::moso::__private::*`. A detectable mistake still produces exactly one
`compile_error!` with a `help:` line and a well-typed placeholder, never a plausible-looking wrong
expansion. Round-trip tests pass against real Postgres.

## Licence

MIT - see the root [`LICENSE`](../../LICENSE).
