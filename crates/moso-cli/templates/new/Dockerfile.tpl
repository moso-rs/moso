# @@CRATE_NAME@@ — a single container image.
#
# Two stages: build against the full toolchain, ship the binary and nothing
# else. The result is a few megabytes over the base image rather than the two
# gigabytes a Rust toolchain weighs.
#
#   docker build -t @@CRATE_NAME@@ .
#   docker run --rm -p 3000:3000 -e @@ENV_PREFIX@@__GREETING=hei @@CRATE_NAME@@
#
# Configuration is entirely through the environment, so the same image runs in
# every environment — see `.env.example` for the keys this application reads.

# --- build -------------------------------------------------------------------
# Pinned to a specific Rust version rather than `latest`: an image that rebuilds
# differently next week is an image you cannot roll back to.
FROM rust:1.90-slim-bookworm AS build

WORKDIR /build

# Dependencies first, in their own layer. Copying the manifests and building a
# stub means a change to `src/` does not re-download and re-compile every
# dependency — the difference between a 20-second rebuild and a 4-minute one.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --locked 2>/dev/null || cargo build --release \
    && rm -rf src

COPY . .

# `touch` so cargo notices the real sources are newer than the stub it just
# compiled; without it the binary from the dependency layer is kept.
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --locked \
    && strip target/release/@@CRATE_NAME@@ || true

# --- run ---------------------------------------------------------------------
# `debian:bookworm-slim` and not `scratch`: the binary is dynamically linked
# against glibc, and TLS needs the system certificate store. `distroless/cc` is
# the smaller alternative if you do not need a shell to debug with.
FROM debian:bookworm-slim AS runtime

# `ca-certificates` is not optional the moment this application makes an
# outbound HTTPS request — to an OAuth provider, an object store, a webhook.
# Installing it here rather than debugging a certificate error in production.
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# An unprivileged user: a process that does not need root must not have it, and
# a container that runs as root is one CVE away from a host problem.
RUN useradd --system --create-home --uid 10001 app
USER app
WORKDIR /home/app

COPY --from=build --chown=app:app /build/target/release/@@CRATE_NAME@@ /usr/local/bin/@@CRATE_NAME@@

# Listen on every interface: `127.0.0.1` inside a container is unreachable from
# outside it, which is the single most common reason a containerised service
# appears to start and then refuses every connection.
ENV @@ENV_PREFIX@@__BIND=0.0.0.0:3000
EXPOSE 3000

# Moso already serves `/healthz` (is the process alive) and `/readyz` (can it
# serve traffic), so configure the probes where they belong — in the
# orchestrator, which is the thing that can act on the answer:
#
#   Kubernetes:  livenessProbe.httpGet.path: /healthz
#                readinessProbe.httpGet.path: /readyz
#   Compose:     healthcheck.test: ["CMD", "curl", "-f", "http://localhost:3000/readyz"]
#
# There is deliberately no `HEALTHCHECK` here: this image has no curl and no
# shell utilities to probe with, and adding them to every deployment to serve a
# check most orchestrators ignore is the wrong trade.

# No shell form, so the binary is PID 1 and receives SIGTERM directly. Moso
# drains in-flight requests on SIGTERM within `server.grace`; a shell wrapper
# would swallow the signal and the container would be killed mid-request.
ENTRYPOINT ["/usr/local/bin/@@CRATE_NAME@@"]
