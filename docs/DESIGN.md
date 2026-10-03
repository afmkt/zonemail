# Zonemail - System Design

Zonemail is a lightweight, custom-built email service and provisioning
microservice written in Rust. It combines a JSON API for dynamic email-address
provisioning, a minimal authoritative DNS server, and an inbound/outbound SMTP
pipeline into a single deployable that owns its own data and control plane.

---

## 1. System Architecture

```text
  Client / Frontend
    |  (HTTP provision + /services control)    |  (SMTP send / receive)
    v                                        v
 +---------------------+           +-----------------------------+
 | API + Services       |           | Inbound SMTP (smtpd)          |
 | salvo, :8081         |           |  STARTTLS (:25) + SMTPS (:465) |
 +----+----------------+           +-------------+----------------+
      |                                    |
      +-------------------+--------------------+
                                v
            +-----------------------------------------+
            | Data: toasty -> Turso / libsql  (JWT opt) |
            +-----------------------------------------+
                                 ^   lookups / audit
      +----------------------+---------------------------------+
      v                                           v
 +-----------------------+           +-------------------------+
 | DNS server (hickory)    |           | Outbound worker (lettre)|
 | authoritative UDP        |           | background in-DB queue   |
 +-----------------------+           +-------------------------+
```

The API owns two responsibilities: provisioning (`domains`, `mailboxes`,
`records`, `messages`) **and** service control (`/services`). DNS and inbound
SMTP also live in-process as controllable listeners.

---

## 2. Core Components

### A. API & Persistence Layer (`salvo` + `toasty` + `toasty-driver-turso`)

* **Purpose**: A JSON HTTP API (built on `salvo`) that dynamically provisions
    domains, mailboxes, and DNS records, controls the service listeners, and
    reads/writes stored mail.
* **Storage**: `toasty` (a thin async ORM over `toasty-driver-turso`) — an
    SQLite-compatible store. The default `database_url` is a local libsql file
    (`turso:zonemail.db`); point it at a remote Turso endpoint with a
    `libsql://…?authToken=…` URL to share state across replicas.
* **Control plane**: The API is itself a *service* (see §4). The same router is
    either bound to a port by the `ServiceManager` or mounted by an embedding
    host via `Daemon::routes()` when `BootMode` omits `Api`.
* **Authz**: The router is gated by a default-open `AuthGuard` (§5). Every
    endpoint wraps its payload in a uniform success/error envelope.

### B. Custom DNS Server (`hickory-proto` + `tokio`)

* **Purpose**: A lightweight authoritative UDP DNS server for the instance's
    provisioned domains.
* **Functionality**: Answers `A`, `AAAA`, `MX`, `TXT`, and `PTR` queries by
    looking the name up in the database. It is authoritative (no recursion, no
    DNSSEC, UDP-only) and is itself a controllable `Service::Dns`.
* **TLS**: The DNS listener is not TLS-wrapped.

### C. Inbound Mail Server (`smtpd` + `tokio`)

* **Purpose**: Listens for incoming SMTP traffic on the `smtp` service and
     accepts mail for provisioned addresses.
* **Functionality**: Performs session-level verification — `RCPT TO` validation
    through `Mailbox::get_by_id` (a recipient is accepted only if a matching
    mailbox exists; otherwise it is rejected with a 5xx `Abort`). Accepted
    payloads are parsed and stored.
* **Parsing**: Raw MIME bytes are parsed using `mail-parser` to extract headers,
    subject, text/HTML bodies, and attachments; the raw payload is kept
    losslessly regardless of parse success.
* **Forwarding**: For a mailbox with a `forward_to`, the message is additionally
    enqueued for outbound delivery; a forwarding failure is logged but never
    aborts the SMTP session or discards the stored copy.
* **Transport security**: STARTTLS on the `smtp` port and a separate SMTPS
    listener (implicit TLS, conventionally :465) are **opt-in** via a
    `[smtp_tls]` table (see §6). With no `[smtp_tls]` table the listener is
    plaintext — the historical behaviour, at zero cost.

### D. Outbound Mail System (`lettre` + in-DB queue)

* **Purpose**: Handles outgoing email delivery, decoupled into a *producer* and a
    *worker* so accepting a message is cheap and durable.
* **Producer** (`enqueue_outbound`): stores one `Message` row (the shared
    content and `raw` MIME payload) plus one `Outbound` row per recipient
    carrying the delivery job. It performs **no** network I/O and returns
    immediately with the job id. `Outbound` *is* the queue row
    (`Queued` -> `InProgress` -> `Delivered`/`Failed`); no separate table is
    needed.
* **Worker** (`run_outbound_worker`): a background `tokio` task — **not** a
    `Service`, driven independently by the runtime — that each tick reclaims any
    job a previous cycle left `InProgress`, then delivers every due `Queued`
    job via `deliver_hosts`: MX lookup, `lettre` SMTP handoff per MX host,
    exponential backoff on failure, and terminal `Failed` after
    `max_attempts`. Each MX attempt appends a `DeliveryStatus` audit row.
* **Atomic claim**: before touching the network the worker *claims* a job
    (`claim`), flipping `Queued` -> `InProgress` **inside a `BEGIN IMMEDIATE`
    transaction** (`TransactionMode::Immediate`). `Immediate` acquires the write
    lock at begin time, so two workers on the shared database cannot both claim
    the same job: the loser blocks until the winner commits, then re-reads the
    now-`InProgress` row and backs off. This prevents a double-send under
    concurrent workers. `concurrent_writes()` (Turso MVCC) is enabled so the
    per-claim write locks don't stall readers.
* **Transport security** (`[outbound]` in the config; `OutboundTransport`):
     the *outbound* counter-part of the inbound `[smtp_tls]`. `tls` selects the
     connection posture on delivery — `plain` (clear text on port 25, the
     default), `opportunistic`/`required` (STARTTLS, via `lettre``'s
     `Tls::Opportunistic` / `Tls::Required`), or `implicit` (SMTPS / `Tls::Wrapper`,
     conventional port 465). A `relay` table fixes a single authenticated
     upstream (`host` / `port` / `user` / `password(_env)`) instead of a
     per-recipient MX lookup, and `insecure` accepts a dev/localhost
     self-signed peer. Omitting `[outbound]` is exactly today's clear-text,
     unauthenticated MX-to-MX handoff — zero cost.

---

## 3. Core Workflows

1. **Provisioning Flow**:
    * Client sends an HTTP request — `POST /domains`, `POST /mailboxes`, or
        `POST /records` — to create the resource.
    * The API handler validates the request, enforces unique-name guards, and
        stores it via `toasty`; the response is the created/patched resource in
        the uniform success envelope.
2. **Inbound Mail Flow**:
    * An external mail server connects to the `smtp` service (optionally over
        STARTTLS / SMTPS).
    * The handler validates each recipient against the database
        (`Mailbox::get_by_id`); unknown recipients are rejected.
    * If valid, the email is accepted, parsed via `mail-parser`, and stored; a
        forwarded mailbox re-enqueues the message for delivery.
3. **Outbound Mail Flow**:
    * Business logic enqueues a send via `enqueue_outbound`, which stores the
        message and a `Queued` job and returns — no network I/O on the request
        path.
    * The background worker claims the job atomically (`BEGIN IMMEDIATE`), looks
        up the recipient's MX hosts, performs the SMTP handoff through `lettre`,
        then marks the job `Delivered`, or re-queues it with an exponential
        backoff (or `Failed` once the attempt cap is exceeded). When an
        `[outbound]` `relay` is configured the MX lookup is replaced by a single
        fixed upstream, and `tls` / `insecure` govern the STARTTLS/SMTPS posture
        of the connection; with no `[outbound]` table the send is a clear-text
        MX-to-MX handoff, exactly as before.

---

## 4. Service Control Plane

The listeners are **services** — `Api`, `Smtp`, `Smtps`, and `Dns` (`enum
Service`) — tracked by a `ServiceManager` with a `Status` of `Idle`/`Running`.
Each service maps to a `Listener` bound in a single `ServiceManager::Inner`:

* At boot a `BootMode` decides which listeners come up: `full` (default; API +
    SMTP + DNS), `api`, `smtp`, `dns`, or `off`. `Smtps` is **not** a mode
    entry: it auto-follows `Smtp` when a `smtps_port` is configured.
* At runtime the control plane is driven through the API — `GET /services`,
    `POST /services/{service}/start|stop`, and `POST /services/mode` — a first
    class capability of the instance rather than a re-deploy.
* A service that isn't configured for the current state (e.g. `Smtps` with no
    `smtps_port`, or a mode that omits `Api`) is dormant and errors cleanly on
    `start`, rather than binding a default address.

The **outbound worker is not a service** — there is no `Service::Outbound`
route — because it is not a listener. It is a fire-and-forget poll loop the
runtime owns independently.

---

## 5. Authentication & Authorization

The API router is wrapped by a default-open `AuthGuard` (`src/auth`):

* When `auth.enabled = false` (the default) the guard is a pure pass-through —
    zero behaviour or latency change, and no secret is required.
* When enabled, every governed route requires a valid bearer token; `health` and
    the `/doc` routes stay public. The instance is a **verifier**, not an authz
    server: roles, grants, and the verification key material are declared in the
    `[auth]` config table.
* Token sources cover symmetric `secret`, inline `jwks`, fetched `jwks_remote`,
    and `issuer`/OIDC discovery. Decisions come from `AuthService` and are
    recorded as `Decision` allow/deny results against a configurable policy.

---

## 6. Transport Security (STARTTLS / SMTPS)

When a `[smtp_tls]` table is present, Zonemail builds a single
`native_tls::Identity` from the configured certificate/key and shares it across
two listeners:

* the `smtp` port negotiates **STARTTLS** (`TlsMode::Explicit` for `optional`,
    `TlsMode::Required` for `required`, i.e. 530 "Must STARTTLS first" for the
    `required` policy),
* an additional first-class `Smtps` service on `smtps_port` (implicit TLS,
    conventionally `:465`) via `TlsMode::Implicit`, controllable through
    `/services` and auto-following `Smtp` at boot.

The certificate source `CertSource` is a tagged union: `kind = "files"` loads
PEM files at boot (the implemented path), while `kind = "acme"` is a **fail-fast
stub** for future automated certificates via an ACME DNS-01 challenge — issued
by this server's own authoritative DNS. Selecting `acme` today surfaces a clear,
actionable error at boot (rather than silently falling back to plaintext) so a
deploy fails loudly and closes over plaintext.

**Status note**: the `smtpd` 0.1.x inline STARTTLS upgrade over
`native-tls` has known rough edges; the implicit-TLS (SMTPS / `TlsMode::Implicit`)
path is the well-tested one, so `mode = "optional"` is the recommended STARTTLS
setting. **Outbound** delivery is governed in parallel by an independent
`[outbound]` table (below).

**Outbound transport security (`[outbound]`)** is the outbound counter-part of
`[smtp_tls]`: it governs how Zonemail *sends*, via `lettre`'s `Tls` selector
(independent of the inbound `native_tls::Identity` shared across the `smtp` / `Smtps`
listeners). `tls` picks the posture — `plain` (clear text on port 25, the default),
`opportunistic` / `required` (`Tls::Opportunistic` / `Tls::Required`, i.e. STARTTLS,
RFC 3207), or `implicit` (`Tls::Wrapper`, i.e. SMTPS / implicit TLS on the conventional
port 465). An optional `relay` table fixes a single authenticated upstream
(`host` / `port` / `user` / `password` / `password_env`) instead of a per-recipient
MX lookup — the production norm of sending through one provider — and `insecure`
accepts a dev/localhost self-signed peer certificate (never in production). Omitting
[outbound] leaves delivery exactly as before: a clear-text, unauthenticated MX-to-MX
handoff on `:25`, with zero cost.
