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

### D. Outbound Mail Client (`lettre`)
* **Purpose**: Handles outgoing email delivery from the application layer.
* **Functionality**: Constructs and dispatches messages over TLS/SMTP to external relays or destinations.

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
   * Application business logic triggers an email send via `lettre`, dispatching it through the configured SMTP transport.