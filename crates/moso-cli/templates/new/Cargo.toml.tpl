[package]
name = "@@CRATE_NAME@@"
version = "0.1.0"
edition = "2024"
rust-version = "1.90"
publish = false
@@WORKSPACE@@
[dependencies]
@@MOSO_DEP@@
# Moso does not pick your runtime for you: `#[tokio::main]` is written in your
# `main`, in your crate, with a version you control.
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
@@DB_DEPS@@@@AUTH_DEPS@@

# Your own code stays unoptimised so it compiles fast; everything you depend on
# is optimised, because it is compiled once and then runs on every request.
[profile.dev.package."*"]
opt-level = 2
