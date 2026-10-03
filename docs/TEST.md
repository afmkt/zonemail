# Local Development & Testing Guide

This guide covers testing the full Zonemail stack — **API provisioning + DNS
server + inbound SMTP (+ optional STARTTLS/SMTPS) + outbound SMTP** — without
root privileges or a live external domain.

---

## Default ports

Zonemail's config defaults (`src/config.rs`) are:

| Service        | Default port | Note                                  |
| -------------- | ------------ | ------------------------------------- |
| API (salvo)    | `8081`       | unprivileged; also serves `/doc`      |
| DNS (hickory)  | `53`         | **privileged** — override to e.g. `5353`|
| Inbound SMTP   | `25`         | **privileged** — override to e.g. `2525`|
| Inbound SMTPS  | *(off)*      | opt-in via `[smtp_tls].smtps_port`; e.g. `465` |

For local testing override the privileged ports to unprivileged ones. The cleanest
way is a small `zonemail.toml` in the working directory:

```toml
host = "127.0.0.1"     # loopback only; keep off 0.0.0.0 in dev
mode = "full"          # API + SMTP (+ SMTPS if configured) + DNS
api = 8081
dns = 5353
smtp = 2525
database_url = "turso:zonemail-dev.db"   # local libsql file

# Optional: enable STARTTLS (:2525) + a separate SMTPS listener.
# [smtp_tls]
# mode = "optional"
# smtps_port = 14465
# cert_source = { kind = "files", cert_path = "cert.pem", key_path = "key.pem" }
```

Every field is overridable by an environment variable using the `ZONEMAIL__`
prefix, e.g. `ZONEMAIL__DNS=5353 ZONEMAIL__SMTP=2525 ZONEMAIL__MODE=smtp`.

---

## Step 1: Outbound catch-all (Mailpit)

Outbound delivery uses `lettre`. To capture it locally instead of hitting the
real MX, run [Mailpit](https://github.com/axllent/mailpit):

```bash
docker run -d --name mailpit -p 1025:1025 -p 8025:8025 axllent/mailpit
```

* **Outbound target**: point the delivery at `127.0.0.1:1025` (unencrypted, no
    auth). In dev you can force this by adding `submission_port = 1025`
    (`OutboundWorkerConfig::with_submission_port(1025)`).
* **Inspection UI**: open `http://localhost:8025`.

> Zonemail does real MX lookup against the recipient's domain by default; the
> catch-all only applies when you point *your* test MX at Mailpit (or run the
> `submission_port` override).

### (Optional) Outbound TLS + an authenticated relay

Outbound delivery starts as clear-text MX-to-MX on `:25` (the historical
behaviour — omitting `[outbound]` is zero-cost). To exercise the STARTTLS / SMTPS
posture, point Zonemail at a local TLS submission endpoint that speaks AUTH,
e.g. Mailpit's TLS port or a dev relay:

```toml
# zonemail.toml — an authenticated SMTPS relay (implicit TLS on 465-style):
# [outbound]
#   tls = "implicit"      # "plain" | "opportunistic" | "required" | "implicit"
#   insecure = true       # only for a self-signed dev/localhost relay
#   submission_port = 1465
#   relay = { host = "127.0.0.1", user = "postmaster",
#             password_env = "ZONEMAIL_OUT_PASS" }   # read from the env, not the file
```

* **Local self-signed endpoint**: a self-signed relay is accepted *only* with
  `insecure = true` (a dev/localhost exception — a real relay's cert is verified
  automatically via the system trust store for its host). **Never enable
  `insecure` in production.**
* **The e2e proof is in the suite**: `deliver_hosts_smtps_implicit_tls_delivers`
  stands up an in-process implicit-TLS acceptor with a generated self-signed cert
  and asserts a `Delivered` outcome — so the `Tls::Wrapper` path is covered
  without any external server. It skips when `openssl` is unavailable.
* **Verify a real STARTTLS handshake** against a TLS-capable endpoint the same way
  you verify the inbound one — e.g. a throwaway `openssl s_client` probe.

---

## Step 2: Run Zonemail

```bash
# either a config file ...
cargo run                      # reads zonemail.toml from the cwd

# ... or all-in-env with no file (unprivileged ports):
ZONEMAIL__HOST=127.0.0.1 ZONEMAIL__API=8081 ZONEMAIL__DNS=5353 \
ZONEMAIL__SMTP=2525 ZONEMAIL__MODE=full ZONEMAIL__DATABASE_URL=turso:zonemail-dev.db \
cargo run
```

This brings up (per `mode = "full"`):
1. The `salvo` API on `:8081` (routes + `/doc` Scalar UI + OpenAPI JSON).
2. The DNS listener on `:5353`.
3. The inbound `smtpd` listener on `:2525` (and `Smtps` on `:14465` if
    `[smtp_tls]` is set).

The data path (`toasty` → `toasty-driver-turso`) is initialized and any
`domains`/`records`/`mailboxes` seeded from config.

---

## Step 3: Provision via the API

Routes are served **at the root** (no `/api` prefix) by `create_router()`.

```bash
# 1. Provision the domain
curl -X POST http://localhost:8081/domains \
  -H "Content-Type: application/json" -d '{"id":"zonemail.net"}'

# 2. Provision a *mailbox* for that domain — inbound acceptance checks the
#    recipient against a mailbox (Mailbox::get_by_id), so a bare domain is not
#    enough for mail to be accepted.
curl -X POST http://localhost:8081/mailboxes \
  -H "Content-Type: application/json" \
  -d '{"id":"test@zonemail.net","domain_id":"zonemail.net"}'

# 3. (Optional) Add an MX record so the domain publishes a mail target.
curl -X POST http://localhost:8081/records \
  -H "Content-Type: application/json" \
  -d '{"domain_id":"zonemail.net","record_type":"MX","value":"10 mail.zonemail.net","ttl":3600}'

# 4. Sanity check: list what's provisioned
curl http://localhost:8081/mailboxes
```

---

## Step 4: DNS lookup (`dig`)

```bash
dig @127.0.0.1 -p 5353 -t MX  zonemail.net      # the MX you just added
dig @127.0.0.1 -p 5353 -t A   mail.zonemail.net  # an A / custom record
```

The server is authoritative and database-driven, so the answer it returns is
whatever you stored in step 3. (Always pass `-p 5353`; `dig` would otherwise try
system port 53.)

---

## Step 5: Inbound delivery (`swaks`)

Use [swaks](https://github.com/wbthompson/swaks) to feed the `smtpd` listener on
`:2525`.

```bash
# Accepted: a provisioned mailbox
swaks --server 127.0.0.1 --port 2525 \
  --to test@zonemail.net --from sender@external.com \
  --header "Subject: Hello from local dev" \
  --body "This is a test inbound payload."

# Rejected: no matching mailbox -> 5xx
swaks --server 127.0.0.1 --port 2525 \
  --to nobody@zonemail.net --from sender@external.com \
  --body "should be refused"
```

Check:
1. **Accepted** mail is stored (verify via `GET /mailboxes/{id}/messages` or the
    raw store) and, if `forward_to` is set, re-enqueued for delivery.
2. **Rejected** mail (no matching `Mailbox::get_by_id`) is refused with a 5xx
    `Abort`.

---

## Step 6 (optional): transport security

With the `[smtp_tls]` block from the config (above), verify the TLS handshakes:

```bash
# SMTPS (implicit TLS) on the dedicated listener
openssl s_client -connect 127.0.0.1:14465 -quiet

# STARTTLS upgrade on the plain listener
openssl s_client -connect 127.0.0.1:2525 -starttls smtp -quiet
```

The cert/key are loaded from the `cert_source` `kind = "files"` paths at boot.
A `kind = "acme"` source is a **fail-fast stub**: `cargo run` will exit with a
clear "not implemented" error rather than silently falling back to plaintext.

---

## Step 7: service control (`/services`)

Any listener can be started/stopped/mode-switched at runtime without a re-deploy:

```bash
curl http://localhost:8081/services                       # status of all
curl -X POST http://localhost:8081/services/smtp/start
curl -X POST http://localhost:8081/services/smtps/stop
curl -X POST http://localhost:8081/services/mode \
   -H "Content-Type: application/json" -d '{"mode":"smtp"}'
```

A service that isn't configured (e.g. `Smtps` with no `smtps_port`) is dormant.

---

## Quick Troubleshooting Checklist

* **Auth blocks your `curl`**: with `auth.enabled = true` every governed route
    needs a bearer token (`health` + `/doc` stay open). Disable it by removing
    the `[auth]` table for local work.
* **Connection refused on `:2525` / `:5353`**: confirm you're binding to
    `127.0.0.1` (or that a firewall allows loopback) and that the service is
    `Running` (`GET /services`) — it is dormant if its `BootMode` omits it.
* **DNS query times out**: always pass the `-p 5353` flag; otherwise `dig`
    targets system port 53.
* **Mail is refused unexpectedly**: inbound acceptance checks
    `Mailbox::get_by_id` — provision the **mailbox** (step 3.2), not just the
    domain. A bare domain + record is not enough.
* **Privileged-port bind errors** (`53` / `25`, `:465`): override to
    unprivileged ports via `zonemail.toml` or `ZONEMAIL__DNS` /
    `ZONEMAIL__SMTP` / `[smtp_tls].smtps_port`.
