# The workspace root for @@CRATE_NAME@@.
#
# `crates/@@CRATE_NAME@@` is the application. Everything that was in this
# directory before the split moved there unchanged, and the package kept its
# name — so every `use @@LIB_NAME@@::…` in your binary and your tests still
# resolves, and `target/release/@@CRATE_NAME@@` is still where the binary lands.
#
# ## Growing into it
#
# Split in the order `00-foundations/04-project-structure.md` recommends: the
# crate that changes most often on top of the ones that change least.
#
#   cargo new --lib crates/@@CRATE_NAME@@-domain   # entities, DTOs, pure logic. No I/O.
#   cargo new --lib crates/@@CRATE_NAME@@-db       # queries, repositories
#   cargo new --lib crates/@@CRATE_NAME@@-web      # routes, extractors, OpenAPI
#
# Each one is picked up by the `crates/*` glob below the moment it exists. Add
# it to `crates/@@CRATE_NAME@@/Cargo.toml` as
#
#   @@CRATE_NAME@@-domain = { path = "../@@CRATE_NAME@@-domain" }
#
# and move the types across. A route edit then recompiles the web crate and the
# application, and nothing underneath them.
#
# ## Running it
#
# `cargo run`, `cargo test` and `cargo build` work from here: there is one
# binary in the workspace, so cargo needs no help choosing. The `moso` commands
# that interrogate the application look for the nearest package, so run them
# from `crates/@@CRATE_NAME@@`, or pass
# `--manifest-path crates/@@CRATE_NAME@@/Cargo.toml` from here.
[workspace]
members = ["crates/*"]
# The edition-2024 resolver, named explicitly: a virtual workspace root that
# does not choose one is a warning on every build.
resolver = "3"
@@PROFILES@@
