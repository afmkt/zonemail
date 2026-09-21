# Zonemail - System Design

Zonemail is a lightweight, custom-built email service and provisioning microservice written in Rust. It provides dynamic email address provisioning via an API, paired with a custom-built DNS server and SMTP pipeline for handling inbound and outbound mail.

---

## 1. System Architecture

```text
               +---------------------------+
               |      Client / Frontend    |
               +---------------------------+
                 |                       |
          (API: Provision)        (SMTP: Send/Receive)
                 |                       |
                 v                       v
       +-------------------+   +--------------------+
       |   Axum API Server |   | Samotop (Inbound)  |
       +-------------------+   +--------------------+
                 |                       |
                 +----------+------------+
                            |
                            v
               +---------------------------+
               |  Database (SQLite/Toasty) |
               +---------------------------+
                            ^
                            |
       +--------------------+--------------------+
       |                                         |
       v                                         v
+-----------------------+             +-----------------------+
| Hickory DNS Server    |             | Lettre (Outbound)     |
+-----------------------+             +-----------------------+
```

---

## 2. Core Components

### A. API & Persistence Layer (`axum` + `sqlx` / `toasty`)
* **Purpose**: Exposes HTTP endpoints to dynamically provision and manage email addresses, mailboxes, and DNS records.
* **Storage**: SQLite-backed state using lightweight ORM/Query layers to track active mailboxes and DNS records.

### B. Custom DNS Server (`hickory-proto` + `tokio`)
* **Purpose**: Runs a lightweight authoritative UDP DNS server.
* **Functionality**: Dynamically queries the database for `A`, `MX`, `TXT`, and `PTR` records to support mail routing and domain verification.

### C. Inbound Mail Server (`samotop` + `tokio`)
* **Purpose**: Listens for incoming SMTP traffic.
* **Functionality**: Performs session-level verification (`RCPT TO` validation against the provisioned database records) and streams accepted payloads.
* **Parsing**: Raw MIME bytes are parsed using `mail-parser` to extract headers, subjects, text/HTML bodies, and attachments.

### D. Outbound Mail System (`lettre` + in-DB queue)
* **Purpose**: Handles outgoing email delivery, decoupled into a *producer* and a
     *worker* so accepting a message is cheap and durable.
* **Producer** (`enqueue_outbound`): stores one `Message` row (the shared content
    and `raw` MIME payload) plus one `Outbound` row per recipient carrying the
    delivery job. It performs **no** network I/O and returns immediately with the
    job id. `Outbound` *is* the queue row (`Queued` -> `InProgress` ->
       `Delivered`/`Failed`); no separate table is needed.
* **Worker** (`run_outbound_worker`): a background `tokio` task that, each tick,
    reclaims any job a previous cycle left `InProgress`, then delivers every due
      `Queued` job - MX lookup, `lettre` SMTP handoff per MX host, exponential
    backoff on failure, and terminal `Failed` after `max_attempts`. Each MX
    attempt appends a `DeliveryStatus` audit row.
* **Atomic claim**: before touching the network the worker *claims* a job
    (`claim`), flipping `Queued` -> `InProgress` **inside a `BEGIN IMMEDIATE`
     transaction**. `Immediate` acquires the write lock at begin time, so two
   workers on the shared database cannot both claim the same job: the loser blocks
    until the winner commits, then re-reads the now-`InProgress` row and backs off.
    This prevents a double-send under concurrent workers. `concurrent_writes()`
    (Turso MVCC) is enabled so the per-claim write locks don't stall readers.

---

## 3. Core Workflows

1. **Provisioning Flow**:
   * Client sends an HTTP request (e.g., `POST /api/emails`) to create a new address.
   * Axum validates and inserts the address/records into the database.
2. **Inbound Mail Flow**:
   * An external mail server connects to the Samotop SMTP listener.
   * Samotop validates the recipient against the database records.
   * If valid, the email is accepted, parsed via `mail-parser`, and stored.
3. **Outbound Mail Flow**:
   * Business logic enqueues a send via `enqueue_outbound`, which stores the
        message and a `Queued` job and returns - no network I/O on the request
        path.
   * The background worker claims the job atomically (`BEGIN IMMEDIATE`), looks
        up the recipient's MX hosts, performs the SMTP handoff through `lettre`,
         then marks the job `Delivered`, or re-queues it with an exponential
        backoff (or `Failed` once the attempt cap is exceeded).