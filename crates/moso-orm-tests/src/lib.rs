#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![doc = "The Moso ORM end-to-end corpus."]
//!
//! This crate has no runtime contents. It exists to own `tests/`: entity
//! declarations written the way an application writes them — through the
//! `moso` facade, with `#[derive(Entity)]` — compiled and then run against a
//! real database.
//!
//! # Why a separate package
//!
//! `#[derive(Entity)]` expands to paths under `::moso::__private::*` and
//! nothing else (decision D6), so it only compiles against the facade with its
//! `orm` feature on. `moso-orm` cannot host these tests: it does not — and by
//! rule 1 must not — depend on the macro crate, and it cannot depend on the
//! facade that re-exports it either. `moso` cannot host them either: turning
//! `orm` on in its own dev-dependencies would turn it on for everything that
//! resolves the facade in the same build, and `xtask check-deps` rule 6 exists
//! to keep a database driver out of the facade's default graph.
//!
//! A test-only member with `moso = { features = ["orm"] }` in its
//! *dev*-dependencies is the one place where the derives can be exercised the
//! way a user meets them. This is the same argument that gives
//! `moso-ui-tests` its own package.
//!
//! # Running it
//!
//! ```text
//! cargo test -p moso-orm-tests
//! DATABASE_URL=postgres://moso:moso@localhost:55433/moso_test \
//!     cargo test -p moso-orm-tests
//! ```
//!
//! SQLite runs everywhere. The PostgreSQL leg gates on `DATABASE_URL` and skips
//! with a message naming the command that starts a server, so the suite is
//! green on a machine with no Docker.
