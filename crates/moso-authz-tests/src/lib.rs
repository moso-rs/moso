#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![doc = "Test-only crate: the corpus lives in `tests/`."]
//!
//! `moso-orm-tests` has the same shape and the same reason. A macro's output is
//! only proved by compiling it in a crate that is *not* the one that defines
//! it, which is what this crate is for.
