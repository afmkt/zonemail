# Local Development & Testing Guide for Rust Email Server

This guide outlines how to set up your local development environment to test your full custom Rust email stack (**API provisioning + DNS server + Inbound SMTP + Outbound SMTP**) without needing root privileges or live external domains.

---

## Architecture Overview for Local Testing

To avoid permission issues on standard system ports (`53` for DNS, `25` for SMTP), run your local services on unprivileged development ports:
* **API Server (Axum):** `http://localhost:3000`
* **DNS Server:** `127.0.0.1:5353`
* **Inbound SMTP Server:** `127.0.0.1:2525`
* **Outbound Catch-All (Mailpit):** `127.0.0.1:1025` (SMTP) / `http://localhost:8025` (Web UI)

---

## Step 1: Set Up Outbound Mail Catch-All (Mailpit)

When your application sends emails out via `lettre`, you want to catch them locally rather than hitting real external servers. Run [Mailpit](https://github.com/axllent/mailpit) using Docker:

```bash
docker run -d --name mailpit -p 1025:1025 -p 8025:8025 axllent/mailpit
```

* **Outbound SMTP Target:** Configure `lettre` to connect to `127.0.0.1:1025` (unencrypted, no authentication).
* **Inspection UI:** Open your browser at `http://localhost:8025` to view all sent messages.

---

## Step 2: Run Your Rust Application

Start your Rust application normally via cargo:

```bash
cargo run
```

Ensure your startup code initializes:
1. The database (e.g., SQLite via Toasty/SQLx).
2. The Axum API listener on port `3000`.
3. The DNS listener on port `5353`.
4. The Inbound SMTP listener on port `2525`.

---

## Step 3: Test API Provisioning

Use `curl` or any HTTP client to provision a test domain and associated records:

```bash
# 1. Provision a test domain
curl -X POST http://localhost:3000/api/domains \
  -H "Content-Type: application/json" \
  -d '{"id": "zonemail.net"}'
```

*(Optional: Add corresponding records or mailboxes if your app requires explicit database entries for them).*

---

## Step 4: Test Local DNS Resolution (`dig`)

Verify that your DNS suffix-matching logic and database lookups are functioning correctly by querying your local DNS server on port `5353`:

```bash
# Query the MX record for your test domain
dig @127.0.0.1 -p 5353 zonemail.net MX

# Query an A record or custom record
dig @127.0.0.1 -p 5353 mail.zonemail.net A
```

**Expected Result:** You should see the records stored in your database returned successfully in the `dig` output response.

---

## Step 5: Test Inbound Email Receiving (`swaks`)

To test whether your inbound SMTP server correctly handles incoming mail, accepts provisioned addresses, and rejects unprovisioned ones, use **`swaks`** (Swiss Army Knife for SMTP). 

*(Install via `brew install swaks` on macOS or `sudo apt install swaks` on Linux).*

```bash
# Test sending an email to a provisioned address
swaks --server 127.0.0.1 \
      --port 2525 \
      --to test@zonemail.net \
      --from sender@external.com \
      --header "Subject: Hello from Local Dev" \
      --body "This is a test inbound email payload."
```

### What to check:
1. **Server Logs:** Verify that your Rust server receives the connection, performs the database check, and logs a successful acceptance.
2. **Rejection Test:** Try sending to an unprovisioned address (e.g., `fake@zonemail.net`). Your server should reject the transaction with a `5xx` error code (e.g., user unknown).

---

## Quick Troubleshooting Checklist

* **Connection Refused on Port 2525 / 5353:** Ensure your Rust application is actively binding to `127.0.0.1` rather than `0.0.0.0` (or that your firewall allows loopback traffic).
* **DNS Query Timeout:** Double-check that you included the explicit port flag (`-p 5353`) in your `dig` command; otherwise, it will default to querying system port 53.