# Zonemail

Zonemail is a lightweight, custom-built email and DNS service written in Rust.
It bundles a REST **API**, an authoritative **DNS** server, an inbound **SMTP**
listener, and an outbound **SMTP** delivery worker into a single, self-contained
binary. Provision an address through the API and zonemail answers for it over
DNS, accepts mail for it over SMTP, stores it durably, and (optionally)
forwards it onward — all in one process.

This guide focuses on **what zonemail can and cannot do**, so you can decide
whether it fits before you install it.

---

## When to reach for zonemail

It is a good fit when you want a small, dependency-light, API-driven
mailcatcher / forwarder / MX host that you can run yourself as a single
container, backed by a SQLite-compatible database. It is *not* a hosted mailbox
provider: there are no accounts, passwords, POP/IMAP, or a webmail UI.

---

## Supported features

### REST API (always on)

A JSON API (Salvo) for provisioning and administering the service.
Every endpoint returns a uniform `{ "ok": true, "data": … }` envelope or an
RFC-7807-style error document, and list endpoints are paginated (`limit`/`offset`).

- **Domains** — `GET|POST /domains`, `GET|PATCH|DELETE /domains/{id}`
- **Mailboxes** (a provisioned email address) —
  `GET|POST /mailboxes`, `GET|PATCH|DELETE /mailboxes/{id}`
- **Send mail** — `POST /mail` (enqueues one delivery job per recipient)
- **Message history** — list a mailbox's messages
  (`/mailboxes/{id}/messages`, `/inbox`, `/sent`, filterable by direction),
  and `GET|DELETE /messages/{id}`
- **Service control** (at runtime, no restart) — `GET /services`,
  `POST /services/{service}/start|stop`, `POST /services/mode`
- **Health probe** — `GET /health`
- **Interactive docs** — a generated OpenAPI spec at `/doc/openapi.json` and a
  browser UI at `/doc/scalar`

### Email addresses

- An address is either **store-only** (mail is captured and kept) or **forwarded**
  to a configured external address (`forward_to`).
- Address is the identity; case is normalized on write so lookups are stable.
- **No** user accounts, passwords, webmail, POP3/IMAP, or per-user
  authentication of the mail itself.

### Inbound SMTP

- Listens on a configurable port (default `25`) using `smtpd`.
- **Recipient validation**: only a provisioned mailbox is accepted; unknown
  recipients are rejected, so the server is **not an open relay**.
- **Lossless storage**: the raw MIME payload is always kept, even if headers
  fail to parse.
- **De-duplication** by `Message-ID` header so a relayed duplicate isn't
  stored twice.
- **Automatic forwarding**: mail delivered to a forwarded mailbox is re-enqueued
  for delivery to its `forward_to` address.
- **No SMTP authentication / no TLS on inbound** — public MX senders connect on
  plaintext; gate the port at the network layer.

### Outbound SMTP & mail forwarding

- A **durable, in-database queue** decouples "accept a message" (fast, no network
  I/O) from "deliver a message" (a background worker).
- The worker resolves the recipient's **MX hosts by priority**, performs the
  SMTP handoff (`lettre`), and records a **delivery audit row per attempt**.
- **Automatic retries with exponential backoff**; a job becomes terminal
  `Failed` after the attempt cap. Stale jobs (left `InProgress` by a crash) are
  reclaimed on the next tick — nothing is silently lost.
- **Concurrency-safe**: jobs are claimed inside a write-locked transaction, so
  two workers on one database never double-send the same message.
- Outbound can be redirected to a local sink/relay (e.g. Mailpit) by
  overriding the submission port for testing.

### DNS

- A custom **authoritative UDP** DNS server (`hickory`), answering queries
  directly from the database.
- Returns `NXDOMAIN` for unknown names; multiple A/MX/TXT/PTR records per name
  are supported.
- **Answered record types**: `A`, `MX`, `TXT`, `PTR`.
- A broad `RecordType` enum is modeled, and many are stored, but only the four
  above are serialized into responses today.

### Configuration

- A `zonemail.toml` file (every field has a default, so an empty/missing file is
  fine) plus **environment overrides** (`ZONEMAIL__FIELD`, `__` separator).
- **Pre-seed** at boot: domains, DNS records, and mailboxes (including
  forwarding targets).
- **Boot modes** (`full` = SMTP+DNS by default, `smtp`, `dns`, `api-only`)
  select which optional listeners start.

### Storage

- **Toasty ORM over Turso/libSQL** (SQLite-compatible). Back it with a local
  `turso:FILE` database or a remote `libsql://…?authToken=…` endpoint.
- Schema is pushed automatically at boot and tolerates an existing database.
- Concurrent writes (MVCC) are enabled so the outbound queue's per-claim write
  locks don't stall readers.

### Operations & packaging

- **Single binary, single process**: the API server and outbound worker are
  always on; SMTP and DNS are optional and can be started/stopped at runtime.
- **Containerized**: a `Dockerfile` builds a multi-arch image that runs
  **non-root** with a `/health` `HEALTHCHECK`.
- **Privileged ports note**: DNS `53` and SMTP `25` need
  `--cap-add NET_BIND_SERVICE` in a container, or override to unprivileged ports
  via `ZONEMAIL__DNS`/`ZONEMAIL__SMTP`.
- **Tested**: in-memory-DB unit/integration suites plus a local TCP SMTP sink for
  the delivery worker, and an `./scripts/e2e.sh` end-to-end smoke test.

---

## What zonemail does **not** include

Set expectations so you can decide whether it covers your use case:

- **No** accounts, authentication, or passwords.
- **No** POP3/IMAP and **no** webmail UI — access mail through the API.
- **No** TLS on the inbound SMTP or DNS listeners.
- **No API for creating DNS records** — they are seeded via config (or added by
  other means); the API manages domains and mailboxes, not records.
- **No catch-all / wildcard mailboxes** — forwarding is configured per address.
- **Limited DNS**: authoritative, UDP-only, no DNSSEC, and only `A`/`MX`/`TXT`/
  `PTR` responses are supported for now.
- **No built-in rate limiting / CSRF / auth middleware** on the API surface —
  protect the API and the (unauthenticated) SMTP port with your own
  firewall/proxy if exposed.

---

## Project structure

- `src/main.rs` — entry point: loads config, then runs the API server, the
  outbound delivery worker, and any opted-in listeners concurrently via
  `tokio::select!`.
- `src/api.rs` — REST API routes, DTOs, and envelope/problem conventions.
- `src/email.rs` — inbound SMTP listener and the MIME parse/forward path.
- `src/send.rs` — the outbound queue producer and the delivery worker.
- `src/dns.rs` — the authoritative UDP DNS server.
- `src/db.rs` — data model (domains, records, mailboxes, messages, queue).
- `src/services.rs` — start/stop control plane for the optional listeners.
- `src/config.rs` — TOML + environment configuration loading.
- `docs/DESIGN.md`, `docs/TEST.md` — architecture and testing notes.

## Getting Started

1. Clone the repository and navigate into the project directory.
2. Build and run the project using Cargo:

   ```bash
   cargo run
   ```

   Or use a config file: copy `zonemail.example.toml` to `zonemail.toml`, edit it
   (or leave it empty for all defaults), and run again.
3. Or run the container:

   ```bash
   docker build . -t zonemail
   docker run --rm -p 8081:8081 --cap-add NET_BIND_SERVICE zonemail
   ```

   Override `ZONEMAIL__DNS`/`ZONEMAIL__SMTP` to use unprivileged ports.

## Requirements

- Rust (stable; edition 2024, i.e. toolchain ≥ 1.85 — see `rust-toolchain.toml`)
- Cargo
- System OpenSSL (`libssl`/`pkg-config`) for TLS-backed dependencies
