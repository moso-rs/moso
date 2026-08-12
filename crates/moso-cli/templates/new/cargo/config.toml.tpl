# Cargo settings for @@CRATE_NAME@@.
#
# Nothing in this file is required — delete it and the project still builds.
# `moso doctor` reads it and tells you what is worth changing on the machine
# you are actually on.

# A dependency that will break in a future compiler is worth hearing about on
# every build, not once a quarter.
[future-incompat-report]
frequency = "always"

# ── Faster linking ──────────────────────────────────────────────────────────
#
# On an incremental rebuild the linker, not the compiler, is usually the thing
# you are waiting for. Install a fast linker, then uncomment the stanza for your
# platform. `moso doctor` prints the exact install command for this machine and
# tells you whether the stanza below is already in effect.
#
# Linux — mold (apt install mold, dnf install mold, brew install mold):
#
# [target.x86_64-unknown-linux-gnu]
# rustflags = ["-C", "link-arg=-fuse-ld=mold"]
#
# [target.aarch64-unknown-linux-gnu]
# rustflags = ["-C", "link-arg=-fuse-ld=mold"]
#
# macOS — the LLD that ships with rustup (rustup component add llvm-tools):
#
# [target.aarch64-apple-darwin]
# rustflags = ["-C", "link-arg=-fuse-ld=lld"]
#
# Windows — rust-lld is already the default on the MSVC toolchain.
