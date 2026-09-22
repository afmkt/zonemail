# ── zonemail container ──────────────────────────────────────────────────
#
# Multi-arch by design: no platform is pinned.
#   dev  (Apple Silicon): docker build .                  -> native arm64
#   prod (x64 linux):      docker buildx --platform linux/amd64 .
#
# Base images are pinned to a tag. Lock them to a manifest-list digest for
# fully reproducible builds, e.g.:
#   rust:1-bookworm       -> docker manifest inspect rust:1-bookworm
#   debian:bookworm-slim  -> docker manifest inspect debian:bookworm-slim
#
# Runtime notes:
#   * The API listener (default :8081) is what the HEALTHCHECK / smoke test
#     probe. DNS (53) and SMTP (25) are privileged: either run with
#     `--cap-add NET_BIND_SERVICE`, or override to unprivileged ports via the
#     ZONEMAIL__DNS / ZONEMAIL__SMTP environment variables.
#   * A zonemail.toml can be bind-mounted at the working directory; all
#     fields have defaults, so the image runs with no config.
#   * `database_url` defaults to `turso:zonemail.db` (a local libsql file).
#     Mount a named volume at /app/data and point database_url at it for
#     persistence.

# ── 1. Dependencies (cached layer: only recompiles on Cargo.toml/lock) ───
FROM rust:1-bookworm AS deps
WORKDIR /build

# pkg-config + libssl-dev: salvo `openssl`/`native-tls` and lettre
# `tokio1-native-tls` link against the system OpenSSL.
RUN apt-get update \
     && apt-get install -y --no-install-recommends pkg-config libssl-dev \
     && rm -rf /var/lib/apt/lists/*

# Dependency-only build: real Cargo manifest + trivial source stubs so only
# the third-party graph compiles. The real sources replace these next.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
     && echo 'fn main() {}' > src/main.rs \
     && printf '' > src/lib.rs \
     && cargo build --release

# ── 2. Application ───────────────────────────────────────────────────────
COPY src/ ./src/
# Force a recompile of just the `zonemail` crate over the dep layer.
RUN touch src/main.rs src/lib.rs && cargo build --release

# ── 3. Runtime ───────────────────────────────────────────────────────────
FROM debian:bookworm-slim
# libssl3 (OpenSSL runtime), ca-certificates, curl for the HEALTHCHECK probe.
RUN apt-get update \
     && apt-get install -y --no-install-recommends ca-certificates libssl3 curl \
     && rm -rf /var/lib/apt/lists/* \
     # An email/DNS server must not run as root. Fixed UID/GID so volume
     # ownership is portable across hosts.
     && groupadd --gid 10001 zonemail \
     && useradd --uid 10001 --gid zonemail --shell /usr/sbin/nologin --no-create-home zonemail \
     && mkdir -p /app/data \
     && chown -R zonemail:zonemail /app
WORKDIR /app
COPY --from=deps /build/target/release/zonemail /usr/local/bin/zonemail

# Persisted libsql data. An empty named volume here inherits image ownership.
VOLUME /app/data

# API is unprivileged (8081). DNS/SMTP are privileged (53/25) — see header.
EXPOSE 8081 53 25

# Liveness/readiness: the dependency-free probe at /health.
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8081/health || exit 1

USER zonemail

# Config (zonemail.toml) is bind-mounted into the working dir; the default
# invocation loads it from CWD (all fields optional). Bind-mount it so it is
# readable by UID 10001.
CMD ["zonemail"]
