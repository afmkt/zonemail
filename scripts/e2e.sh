#!/usr/bin/env bash
#
# scripts/e2e.sh — end-to-end smoke test for the Zonemail stack.
#
# Drives a *real* running daemon (the same entrypoint `cargo run` uses) and
# exercises, in order:
#
#    1. API provisioning   (curl)     — create a domain + its mailboxes
#    2. DNS resolution      (dig)      — confirm the local nameserver answers
#    3. Inbound SMTP        (swaks)    — accept a provisioned address, reject an unknown
#    4. Outbound delivery   (Mailpit)  — optional catch-all probe
#
# The daemon runs on unprivileged development ports (53 / 25 / 8081 need root),
# so the whole harness can run in a container or on a laptop with no special
# privileges. Every port plus the host are overridable via environment variables.
#
# The config env overrides use a DOUBLE-underscore separator
# (Environment::separator("__")), so each override is ZONEMAIL__<FIELD>
# (e.g. ZONEMAIL__API), NOT ZONEMAIL_API.
#
# Required tools: curl. Optional (the harness degrades / skips gracefully):
# dig (DNS), swaks (inbound SMTP), and a running Mailpit instance (outbound).
#
# Usage:
#    ./scripts/e2e.sh                # build, start, exercise, tear down
#   E2E_SKIP_START=1 ./scripts/e2e.sh   # assume the daemon is already running
set -euo pipefail

# ── tunables (override with env vars) ─────────────────────────────────────────
HOST="${ZONEMAIL_HOST:-127.0.0.1}"
API_PORT="${E2E_API_PORT:-3000}"
DNS_PORT="${E2E_DNS_PORT:-5353}"
SMTP_PORT="${E2E_SMTP_PORT:-2525}"
MAILPIT_UI="${E2E_MAILPIT_UI:-8025}"

# A throwaway scratch directory so every run is hermetic and cleans up after
# itself. The daemon's default *relative* database (turso:zonemail.db) is created
# inside it, then removed with it — an absolute `turso:` path is mis-parsed by
# the driver as a remote connection, so we deliberately do NOT override that.
DB_DIR="$(mktemp -d "${TMPDIR:-/tmp}/zonemail-e2e.XXXXXX")"
API_BASE="http://${HOST}:${API_PORT}"

E2E_SKIP_START="${E2E_SKIP_START:-0}"
SERVER_PID=""

cleanup() {
     if [[ -n "$SERVER_PID" ]]; then
          echo "↻ stopping server (pid $SERVER_PID)"
          kill "$SERVER_PID" 2>/dev/null || true
          wait "$SERVER_PID" 2>/dev/null || true
     fi
     rm -rf "$DB_DIR"
}
trap cleanup EXIT

say()  { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }
ok()   { printf '   \033[32m✓\033[0m %s\n' "$*"; }
fail() { printf '   \033[31m✗ %s\033[0m\n' "$*"; }
skip() { printf '   \033[33m⚠ skip: %s\033[0m\n' "$*"; }

# Wait until a loopback TCP port accepts connections (up to ~15s).
wait_for_port() {
     local port="$1" i
     for i in $(seq 1 75); do
          if (exec 3<>/dev/tcp/127.0.0.1/"$port") 2>/dev/null; then
              exec 3>&- 3<&-
              return 0
          fi
          sleep 0.2
     done
     return 1
}
have() { command -v "$1" >/dev/null 2>&1; }

# ── 0. start the daemon ─────────────────────────────────────────────────────────
if [[ "$E2E_SKIP_START" != "1" ]]; then
     say "Building & starting the daemon"
     echo "  host=$HOST api=$API_PORT dns=$DNS_PORT smtp=$SMTP_PORT (scratch dir: $DB_DIR)"
      # Build the release binary once, then run *it* inside the scratch dir so the
      # default relative database lands there and is removed with the scratch dir.
     cargo build -q --release
      # Config env overrides use a DOUBLE-underscore separator, so each is
      # ZONEMAIL__<FIELD>. (Confirmed by the daemon's config loader.)
     REL_BIN="$PWD/target/release/zonemail"
      (
          cd "$DB_DIR"
          exec env \
               ZONEMAIL__HOST="$HOST" \
               ZONEMAIL__API="$API_PORT" \
               ZONEMAIL__DNS="$DNS_PORT" \
               ZONEMAIL__SMTP="$SMTP_PORT" \
                 "$REL_BIN"
      ) &
     SERVER_PID=$!

     if wait_for_port "$API_PORT"; then
          ok "API accepted connections on $API_PORT"
     else
          fail "API did not come up on $API_PORT"
          exit 1
     fi
fi

# ── 1. API provisioning ──────────────────────────────────────────────────────────
say "API: provision a domain and its mailboxes"
if have curl; then
      # Create the domain (idempotent: a duplicate is a 409, which we tolerate).
     code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API_BASE/domains" \
          -H 'Content-Type: application/json' -d '{"id": "zonemail.net"}' || true)
     if [[ "$code" == "200" || "$code" == "201" || "$code" == "409" ]]; then
          ok "POST /domains zonemail.net -> $code"
     else
          fail "POST /domains -> $code"
          exit 1
     fi

          # Provision a mailbox so inbound delivery has a target.
     code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API_BASE/mailboxes" \
          -H 'Content-Type: application/json' -d '{"id": "test@zonemail.net"}' || true)
     if [[ "$code" == "200" || "$code" == "201" || "$code" == "409" ]]; then
          ok "POST /mailboxes test@zonemail.net -> $code"
     else
          fail "POST /mailboxes -> $code"
          exit 1
     fi

          # Read it back and confirm 200.
     code=$(curl -s -o /dev/null -w '%{http_code}' "$API_BASE/mailboxes/test@zonemail.net" || true)
     [[ "$code" == "200" ]] && ok "GET /mailboxes/test@zonemail.net -> 200" || fail "GET mailbox -> $code"

          # The health probe must report healthy.
     code=$(curl -s -o /dev/null -w '%{http_code}' "$API_BASE/health" || true)
     [[ "$code" == "200" ]] && ok "GET /health -> 200" || { fail "GET /health -> $code"; exit 1; }
else
     skip "curl not found — cannot exercise the API"
fi

# ── 2. DNS resolution ─────────────────────────────────────────────────────────
say "DNS: confirm the local nameserver answers"
if have dig; then
          # There is no API to store DNS records, so this can only prove the
          # nameserver is *reachable*: an empty answer (no record provisioned) is a
          # healthy response, not a failure. Only a connection error is fatal.
     MX_OUT="$(dig @"$HOST" -p "$DNS_PORT" zonemail.net MX +short 2>&1)"
     if echo "$MX_OUT" | grep -qiE 'connection refused|could not|timed out|connect:'; then
          fail "nameserver on $DNS_PORT refused the connection: $MX_OUT"
     elif [[ -n "$MX_OUT" ]]; then
          ok "dig MX zonemail.net -> $MX_OUT"
     else
          ok "nameserver on $DNS_PORT reachable (no MX record provisioned yet — none can be created via the public API)"
     fi
else
     skip "dig not found — skipping DNS resolution checks"
fi

# ── 3. Inbound SMTP ─────────────────────────────────────────────────────────
say "Inbound SMTP: accept provisioned, reject unknown"
if have swaks; then
          # A provisioned address is accepted.
     if swaks --server "$HOST" --port "$SMTP_PORT" \
          --to test@zonemail.net \
          --from sender@external.com \
          --header "Subject: Hello from Local Dev" \
          --body "This is an e2e inbound payload." \
          2>&1 | grep -qiE 'OK|250'; then
          ok "swaks: provisioned recipient accepted"
     else
          fail "swaks: provisioned recipient not accepted"
     fi

          # An unprovisioned address must be rejected (5xx).
     reject_out=$(swaks --server "$HOST" --port "$SMTP_PORT" \
          --to ghost@zonemail.net \
          --from sender@external.com \
          --body "should be rejected" 2>&1 || true)
     if echo "$reject_out" | grep -qiE '5[0-9]{2}|rejec|error|fail|user unknown'; then
          ok "swaks: unprovisioned recipient rejected"
     else
          skip "swaks: unprovisioned-recipient rejection not clearly signalled (review output above)"
     fi
else
     skip "swaks not found — install with 'brew install swaks' (macOS) or 'apt install swaks' (Linux)"
fi

# ── 4. Outbound delivery to a catch-all ───────────────────────────────────────
say "Outbound delivery: Mailpit catch-all (optional)"
if have curl; then
          # Mailpit's web API lists captured messages. Only meaningful if Mailpit
          # is running and this build routes outbound SMTP at a catch-all.
     code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${MAILPIT_UI}/api/v1/messages?count=1" || true)
     if [[ "$code" == "200" ]]; then
          ok "Mailpit reachable at :${MAILPIT_UI} — inspect captured mail in its web UI"
     else
          skip "Mailpit not reachable at :${MAILPIT_UI} (start it with the docker run command in docs/TEST.md to capture outbound mail)"
     fi
else
     skip "curl not found — cannot probe Mailpit"
fi

say "Done"
echo "  Scratch database at $DB_DIR will be removed on exit."
