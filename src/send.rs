//! Outbound delivery subsystem.
//!
//! Outbound traffic is **decoupled** by an in-database queue so that accepting a
//! message is fast, non-blocking, and durable: the producer ([`enqueue_outbound`])
//! only writes a `Message` plus a `Queued` `Outbound` row and returns; a separate
//! background task ([`run_outbound_worker`]) later drains the queue, looks up MX
//! records, performs the SMTP handoff, and resolves each job to a terminal status.
//!
//! The `Outbound` row *is* the queue entry, so no extra table is needed. Every
//! MX attempt appends a `DeliveryStatus` row (audit trail), and transient
//! failures re-queue the job with an exponential backoff until it succeeds or
//! exhausts its attempt cap.

use crate::config::{OutboundTlsMode, OutboundTransport};
use crate::db::{DeliveryStatus, Message, Outbound, OutboundStatus};
// `TransactionMode` is not re-exported by `toasty`, so name it from the core.
use toasty_core::driver::operation::TransactionMode;

use hickory_resolver::TokioAsyncResolver;
use jiff::Timestamp;
use lettre::{
    address::{Address as LettreAddress, Envelope},
    transport::smtp::{
        authentication::Credentials,
        client::{Tls, TlsParameters},
        AsyncSmtpTransport,
     },
    AsyncTransport, Message as LettreMessage, Tokio1Executor,
};
use mail_parser::{Address as ParsedAddress, MessageParser};
use tracing::{info, warn};

// Standard inbound port for an MX-to-MX handoff. `builder_dangerous` is used
// deliberately: we send as ourselves to the recipient's own MX on port 25, the
// classic relay path, not an authenticated upstream submission relay.
const SUBMISSION_PORT: u16 = 25;

// First address of a parsed header value (used for the `From:` header field).
fn first_address(addr: &ParsedAddress) -> String {
    match addr {
        ParsedAddress::List(list) => list
            .first()
            .and_then(|a| a.address.as_deref())
            .map(String::from)
            .unwrap_or_default(),
        ParsedAddress::Group(groups) => groups
            .iter()
            .find_map(|g| g.addresses.first().and_then(|a| a.address.as_deref()))
            .map(String::from)
            .unwrap_or_default(),
    }
}

// Every address carried by a `To:`/`Cc:`/`Bcc:` header value.
fn addrs_to_strings(addr: &ParsedAddress) -> Vec<String> {
    match addr {
        ParsedAddress::List(list) => list
            .iter()
            .filter_map(|a| a.address.as_deref().map(String::from))
            .collect(),
        ParsedAddress::Group(groups) => groups
            .iter()
            .flat_map(|g| g.addresses.iter())
            .filter_map(|a| a.address.as_deref().map(String::from))
            .collect(),
    }

}
/// Box a `Display` error with a context prefix, pinned to `Send + Sync`.
///
/// Pinning the target to a single concrete `Box<dyn Error + Send + Sync>` both
/// makes the worker futures `Send` and sidesteps the multiple-`From` ambiguity
/// (`E0283`) that a bare `Box<dyn Error>` + `.into()` produces.
fn err<E: std::fmt::Display + Send + Sync + 'static>(
    prefix: &str,
    e: E,
) -> Box<dyn std::error::Error + Send + Sync> {
    Box::from(format!("{prefix}: {e}"))
}

// ---------------------------------------------------------------------------
// Queue configuration
// ---------------------------------------------------------------------------

/// Tunable parameters for the outbound worker.
#[derive(Debug, Clone)]
pub struct OutboundWorkerConfig {
    /// How often the worker polls the queue for due work.
    pub poll_interval: std::time::Duration,
    /// After this many total attempts a job is marked `Failed` (terminal).
    pub max_attempts: u64,
    /// Backoff for the first retry; each subsequent retry doubles it.
    pub base_backoff: std::time::Duration,
    /// Upper bound on the retry backoff.
    pub max_backoff: std::time::Duration,
           /// Port dialed on a recipient MX host (default 25; override for a relay/sink).
      /// Outbound transport + security posture: TLS posture, an optional fixed
      /// submission relay, and credentials. The default is clear-text,
      /// unauthenticated MX-to-MX on `:25` — the historical behaviour.
      pub transport: OutboundTransport,
      pub submission_port: u16,
}

impl OutboundWorkerConfig {
        /// Override the outgoing SMTP port dialed on a recipient MX host.
        /// A relay/Mailpit endpoint (e.g. 1025) lets outbound be redirected for
        /// testing without touching the global default.
      pub fn with_submission_port(mut self, port: u16) -> Self {
            self.submission_port = port;
            self
            }
           /// The outgoing SMTP port the worker dials on a recipient MX host.
        pub fn submission_port(&self) -> u16 { self.submission_port }
          }

impl Default for OutboundWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: std::time::Duration::from_secs(5),
            max_attempts: 8,
            base_backoff: std::time::Duration::from_secs(60),
            max_backoff: std::time::Duration::from_secs(3_600),
                transport: OutboundTransport::default(),
            submission_port: SUBMISSION_PORT,
        }
    }
}

// ---------------------------------------------------------------------------
// Producer
// ---------------------------------------------------------------------------

/// Persist a message and enqueue one delivery job for a single recipient. Returns
/// `(message_id, outbound_id)` — the stored [`Message`] id and the queued
/// [`Outbound`] job id. Performs **no** network I/O.
///
/// `sender` must be a provisioned mailbox because `Outbound.sender_id` is a
/// non-nullable foreign key.
pub async fn enqueue_outbound(
    db: &mut toasty::Db,
    sender: &str,
    recipient: &str,
    message: LettreMessage,
) -> Result<(u64, u64), Box<dyn std::error::Error + Send + Sync>> {
    // 1. Envelope sender (MAIL FROM) and the lossless raw bytes that will be
    // transmitted later, exactly as built.
    let mail_from = message
        .envelope()
        .from()
        .map(|a| a.to_string())
        .unwrap_or_default();
    let raw = message.formatted();

    // 2. Best-effort header extraction (same parser as the inbound path).
    let parsed = MessageParser::default().parse(&raw);
    let subject = parsed.as_ref().and_then(|e| e.subject()).map(|s| s.to_string());
    let content_type = parsed
        .as_ref()
        .and_then(|e| e.header_raw("content-type"))
        .map(|s| s.to_string());
    let message_id_header = parsed
        .as_ref()
        .and_then(|e| e.message_id())
        .map(|s| s.to_string());
    let from_address = parsed.as_ref().and_then(|e| e.from()).map(first_address);
    let to = parsed
        .as_ref()
        .and_then(|e| e.to())
        .map_or_else(Vec::new, addrs_to_strings);
    let cc = parsed
        .as_ref()
        .and_then(|e| e.cc())
        .map_or_else(Vec::new, addrs_to_strings);
    let bcc = parsed
        .as_ref()
        .and_then(|e| e.bcc())
        .map_or_else(Vec::new, addrs_to_strings);

    // 3. Store the shared message content exactly once.
    let created = toasty::create! {
        Message {
            mail_from,
            raw,
            subject,
            message_id_header,
            content_type,
            from_address,
            to,
            cc,
            bcc,
        }
    }
    .exec(db)
    .await
    .map_err(|e| err("failed to store outbound Message", e))?;
    info!("Stored outbound message {}", created.id);

    // 4. Enqueue the delivery job as `Queued`, with no backoff yet.
    let job = toasty::create! {
        Outbound {
            message_id: created.id,
            rcpt_to: recipient.to_string(),
            sender_id: sender.to_string(),
            status: OutboundStatus::Queued,
            attempts: 0,
            next_attempt_at: None,
            last_error: None,
         }
     }
     .exec(db)
    .await
    .map_err(|e| err("failed to enqueue Outbound", e))?;
    info!("Enqueued outbound job {} for {sender} -> {recipient}", job.id);

    Ok((created.id, job.id))
}

// ---------------------------------------------------------------------------
// Forward
// ---------------------------------------------------------------------------

/// Enqueue delivery of an existing `Message` to a new recipient address.
///
/// This is the *forward* path: the `Message` row already exists (stored by the
/// inbound handler), so only a new `Outbound` job is created — no duplicate
/// `Message` row. The job is `Queued` with zero attempts and no backoff, so the
/// worker picks it up on its next tick.
///
/// `sender_id` must be a provisioned `Mailbox.id` because it is the non-nullable
/// FK `Outbound.sender_id` — the provisioned address that received and forwarded
/// the mail (e.g. `user@zonemail.net`), not the original external `From:`.
///
/// `forward_to` is the external recipient address (e.g. the user's personal inbox).
///
pub async fn enqueue_forward(
    db: &mut toasty::Db,
    message_id: u64,
    sender_id: &str,
    forward_to: &str,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let job = toasty::create! {
        Outbound {
            message_id,
            rcpt_to: forward_to.to_string(),
            sender_id: sender_id.to_string(),
            status: OutboundStatus::Queued,
            attempts: 0,
            next_attempt_at: None,
            last_error: None,
         }
     }
     .exec(db)
    .await
    .map_err(|e| err("failed to enqueue forward Outbound", e))?;
    info!(
        "Enqueued forward job {} for {sender_id} -> {forward_to} (message {message_id})",
        job.id
    );
    Ok(job.id)
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

/// Background task that drains the outbound queue. Runs until cancelled; on every
/// `config.poll_interval` tick it first reclaims any job a previous cycle left
/// `InProgress` (so no message is orphaned), then delivers every due `Queued` job
/// in FIFO order.
pub async fn run_outbound_worker(mut db: toasty::Db, config: OutboundWorkerConfig) {
    // A long-running poll loop. Nothing to signal from inside, so the task lives
    // until the surrounding `tokio::select!` (in `main`) cancels it.
    let mut ticker = tokio::time::interval(config.poll_interval);
    loop {
        // The first `interval` tick fires immediately; later ticks pace the loop.
        ticker.tick().await;
        reclaim_in_progress(&mut db).await;
        if let Err(e) = drain_once(&mut db, &config).await {
            warn!("Outbound worker tick failed (will retry next tick): {e}");
        }
    }
}

// Reset any job left mid-flight by a previous cycle (e.g. a crash or a delivery
// that failed before reaching a terminal state) back to `Queued` so it is retried.
async fn reclaim_in_progress(db: &mut toasty::Db) {
    let status = OutboundStatus::InProgress;
    let rows = match toasty::query!(Outbound filter .status == #status).exec(db).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Could not scan InProgress Outbound rows: {e}");
            return;
        }
    };
    for mut job in rows {
        let id = job.id;
        if let Err(e) = toasty::update! {
            job {
                status: OutboundStatus::Queued,
                next_attempt_at: None,
                last_error: None,
            }
        }
        .exec(db)
        .await
        {
            warn!("Failed to recover Outbound {id}: {e}");
        } else {
            info!("Recovered stuck Outbound {id} (was InProgress)");
        }
    }
}

// Deliver every `Queued` job that is due right now (`next_attempt_at` is `None` or
// already in the past). Each job is first *claimed* under a write-locked
// transaction so two workers can never both pick up the same one.
async fn drain_once(
    db: &mut toasty::Db,
    config: &OutboundWorkerConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let now = Timestamp::now();
    let status = OutboundStatus::Queued;
    let rows = toasty::query!(Outbound filter .status == #status).exec(db).await
        .map_err(|e| err("failed to fetch queued Outbound rows", e))?;

    for job in rows {
        if let Some(next) = job.next_attempt_at {
            if next > now {
                // Not due yet: leave it queued until its backoff elapses.
                continue;
            }
        }
        // Atomically claim this job before any network I/O, so only one worker
        // (across processes) proceeds with it. A peer that reached the job first
        // has moved it off `Queued`, so `claim` returns `None` and we skip it.
        let claimed = claim(db, job.id).await?;
        if let Some(job) = claimed {
            let id = job.id;
            if let Err(e) = deliver_one(db, job, config, now).await {
                warn!("Delivery cycle for Outbound {id} failed; it will be reclaimed: {e}");
                // The job may still be `InProgress`; `reclaim_in_progress` re-queues it
                // on the next tick so it is not lost.
            }
        }
    }
    Ok(())
}

// Atomically claim one job and hand it back to the caller, or `None` if a peer
// already claimed it.
//
// The claim runs inside an `Immediate`/write-locked transaction: only one such
// transaction at a time can hold the write lock, so a second worker claiming the
// same row blocks in `begin()` until the first commits. When it resumes it
// re-reads the row *after* the first flip and observes `InProgress`, so the
// `status == Queued` guard below rejects it. Across any number of processes
// sharing the database this guarantees exactly one worker owns each claim.
async fn claim(
    db: &mut toasty::Db,
    id: u64,
) -> Result<Option<Outbound>, Box<dyn std::error::Error + Send + Sync>> {
    let mut tx = db
        .transaction_builder()
        .mode(TransactionMode::Immediate)
        .begin()
        .await
        .map_err(|e| err("failed to begin claim transaction", e))?;

    // Re-read inside the transaction so we observe the state after any committed
    // peer's work.
    let job = toasty::query!(Outbound filter .id == #id)
        .first()
        .exec(&mut tx)
        .await
        .map_err(|e| err("failed to load job for claim", e))?;

    let Some(mut job) = job else {
        // The row vanished between the fetch and the claim; nothing to do.
        tx.commit()
            .await
            .map_err(|e| err("failed to commit empty claim", e))?;
        return Ok(None);
    };

    // Only the winner flips it. A loser sees a status other than `Queued` and
    // declines to take ownership.
    if job.status != OutboundStatus::Queued {
        tx.commit()
            .await
            .map_err(|e| err("failed to commit discarded claim", e))?;
        return Ok(None);
    }

    toasty::update! {
        job {
            status: OutboundStatus::InProgress,
        }
    }
    .exec(&mut tx)
    .await
    .map_err(|e| err("failed to flip Outbound InProgress", e))?;
    tx.commit()
        .await
        .map_err(|e| err("failed to commit claim", e))?;

    Ok(Some(job))
}

// One delivery attempt on an already-claimed job: try each MX host in priority
// order, then mark it `Delivered`, or re-queue / fail it. The caller has
// already flipped this job to `InProgress`, so this worker owns it exclusively
// and only resolves the terminal / re-queued state below.
async fn deliver_one(
    db: &mut toasty::Db,
    outbound: Outbound,
    config: &OutboundWorkerConfig,
    now: Timestamp,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
       // Load the message content to transmit, exactly as stored.
    let message = Message::get_by_id(db, &outbound.message_id).await
            .map_err(|e| err("failed to load Message for Outbound", e))?;

        // Build the envelope from the stored MAIL FROM and the job's RCPT TO.
    let envelope = build_envelope(&message.mail_from, &outbound.rcpt_to)?;

        // Resolve MX hosts, highest priority first, then hand off to the transport core.
    let hosts = lookup_mx_hosts(&outbound.rcpt_to)
            .await
            .map_err(|e| err("failed to look up MX hosts", e))?;
    deliver_hosts(db, outbound, &message, &envelope, config, now, &hosts).await
}

/// Map the configured [`OutboundTransport`] to `lettre`'s `Tls` selection for a
/// peer `domain`, honouring `insecure` (a dev/localhost self-signed relay).
///
/// `Plain` yields `Tls::None` and never touches the TLS stack, so the default
/// path (no TLS) stays zero-cost. Credentials are applied separately by the
/// caller when a relay user is set.
fn build_outbound_tls(
    transport: &OutboundTransport,
    domain: &str,
) -> Result<Tls, Box<dyn std::error::Error + Send + Sync>> {
    if !transport.tls_enabled() {
        return Ok(Tls::None);
     }
     // `dangerous_accept_invalid_{certs,hostnames}` is the seam for a dev/localhost
      // relay that presents a self-signed CA or a localhost name. Off by default;
      // turning it on is a conscious, local-only choice.
    let mut params = TlsParameters::builder(domain.to_string());
    if transport.insecure {
        params = params
               .dangerous_accept_invalid_hostnames(true)
               .dangerous_accept_invalid_certs(true);
        }
    let params = params.build_native()?;
    Ok(match transport.tls {
        OutboundTlsMode::Opportunistic => Tls::Opportunistic(params),
        OutboundTlsMode::Required => Tls::Required(params),
        OutboundTlsMode::Implicit => Tls::Wrapper(params),
        // `Plain` is unreachable: the `tls_enabled()` guard above handles it.
        OutboundTlsMode::Plain => Tls::None,
      })
    }

/// Transport-level core of [`deliver_one`]: attempt delivery of one `Outbound`
/// job to an explicit list of SMTP hosts (highest priority first). Each attempt
/// appends a `DeliveryStatus` row; the first `250` resolves the job to
/// `Delivered`, otherwise it finishes via [`fail_or_requeue`].
///
/// Split out from [`deliver_one`] so it can be exercised in isolation against a
/// local in-process SMTP sink with **no** DNS resolution: pass `127.0.0.1` as the
/// host and point [`OutboundWorkerConfig::with_submission_port`] at the sink. This
/// keeps the SMTP handoff testable and free of external dependencies.
///
/// # Panics
///
/// None by design — every fallible step is returned as a boxed, `Send + Sync`
/// error rather than unwrapping.
pub async fn deliver_hosts(
    db: &mut toasty::Db,
    mut outbound: Outbound,
    message: &Message,
    envelope: &Envelope,
    config: &OutboundWorkerConfig,
    now: Timestamp,
    hosts: &[String],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Choose the target host(s): a fixed submission relay when one is
     // configured, else the recipient's per-recipient MX hosts in priority
      // order. A relay is the production norm (one upstream, usually
      // authenticated on :587); MX-to-MX is the zero-config default.
    let targets: Vec<String> = if config.transport.relay_configured() {
        let host = config
               .transport
               .relay
               .as_ref()
               .and_then(|r| r.host.clone())
               .ok_or("outbound relay is configured but has no `host`")?;
        info!("Outbound {} using fixed submission relay {host}", outbound.id);
        vec![host]
      } else {
    hosts.to_vec()
      };

    if targets.is_empty() {
        warn!("No MX records for recipient of Outbound {}; requeueing for retry", outbound.id);
        return fail_or_requeue(db, &mut outbound, config, now, "no MX records for domain").await;
      }

        // Try each target in priority order, recording one DeliveryStatus per
        // attempt. With a relay the target list is the single relay host.
    let mut last_error: Option<String> = None;
    for host in &targets {
       let port = config.transport.relay.as_ref().map_or(config.submission_port(),
            |r| if r.port == 0 { config.submission_port() } else { r.port });
        info!("Outbound {} attempting {host} on port {port}", outbound.id);
         // Build the per-target transport: the TLS posture mapped from
     // `transport.tls` (Plain => no TLS, the default and zero-cost), the
     // configured port, and AUTH credentials when a relay user is set.
        let tls = build_outbound_tls(&config.transport, host)?;
        let mut builder =
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host.clone())
                       .port(port)
                       .tls(tls);
        if let Some((user, pass)) =
            config.transport.relay.as_ref().and_then(|r| r.credentials())
        {
        builder = builder.credentials(Credentials::new(user, pass));
      }
        let transport = builder.build();

        match transport.send_raw(&envelope, &message.raw).await {
            Ok(response) => {
                record_delivery(
                    db,
                    outbound.id,
                    host.clone(),
                    Some(250),
                    Some(format!("{response:?}")),
                    )
                    .await?;
                    // Pre-compute the counter so the update does not read a field it
                    // is simultaneously mutating.
                let attempts = outbound.attempts + 1;
                toasty::update! {
                    outbound {
                        status: OutboundStatus::Delivered,
                        attempts,
                        next_attempt_at: None,
                        last_error: None,
                        }
                    }
                    .exec(db)
                    .await
                    .map_err(|e| err("failed to mark Outbound Delivered", e))?;
                info!("Delivered Outbound {} -> {} via {host}", outbound.id, outbound.rcpt_to);
                return Ok(());
                 }
            Err(e) => {
                warn!("Outbound {} failed via {host}: {e}", outbound.id);
                record_delivery(db, outbound.id, host.clone(), None, Some(e.to_string()))
                        .await?;
                last_error = Some(format!("via {host}: {e}"));
                 }
             }
        }

        // Every MX attempt failed: bump the counter and re-queue or fail terminally.
    fail_or_requeue(
        db,
            &mut outbound,
        config,
        now,
        last_error.as_deref().unwrap_or("all MX attempts failed"),
        )
        .await
}


// Append a per-attempt audit row.
async fn record_delivery(
    db: &mut toasty::Db,
    outbound_id: u64,
    peer_host: String,
    code: Option<u64>,
    message: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    toasty::create! {
        DeliveryStatus {
            outbound_id,
            peer_host,
            code,
            message,
        }
    }
    .exec(db)
    .await
    .map_err(|e| err("failed to record DeliveryStatus", e))?;
    Ok(())
}

// After all MX attempts for a job fail: increment the attempt counter and either
// re-queue with an exponential backoff (retryable) or mark the job terminally
// `Failed` once the cap is exceeded.
async fn fail_or_requeue(
    db: &mut toasty::Db,
    outbound: &mut Outbound,
    config: &OutboundWorkerConfig,
    now: Timestamp,
    reason: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let attempts = outbound.attempts + 1;
    let reason = reason.to_string();

    if attempts > config.max_attempts {
        // Exceeded the retry cap: give up.
        toasty::update! {
            outbound {
                status: OutboundStatus::Failed,
                attempts,
                next_attempt_at: None,
                last_error: Some(reason.clone()),
            }
        }
        .exec(db)
        .await
        .map_err(|e| err("failed to mark Outbound Failed", e))?;
        warn!("Outbound {} permanently failed after {attempts} attempts", outbound.id);
    } else {
        let delay = backoff_delay(config, attempts);
        // `Span::new().seconds(n)` is jiff's builder form; `Timestamp + Span`
        // yields the next due instant.
        let next = now + jiff::Span::new().seconds(delay.as_secs() as i64);
        toasty::update! {
            outbound {
                status: OutboundStatus::Queued,
                attempts,
                next_attempt_at: Some(next),
                last_error: Some(reason.clone()),
            }
        }
        .exec(db)
        .await
        .map_err(|e| err("failed to requeue Outbound", e))?;
        warn!(
            "Outbound {} failed attempt {attempts}; retry in {}s: {reason}",
            outbound.id,
            delay.as_secs()
        );
    }

    Ok(())
}

// Exponential backoff: `base_backoff * 2^(attempts-1)`, capped at `max_backoff`.
fn backoff_delay(config: &OutboundWorkerConfig, attempts: u64) -> std::time::Duration {
    // Capped at 2^62 so the shift can't overflow a u64 even for a huge max_attempts.
    let shift = attempts.saturating_sub(1).min(62);
    let multiplier = 1u64 << shift;
    let seconds = config.base_backoff.as_secs().saturating_mul(multiplier);
    let capped = seconds.min(config.max_backoff.as_secs());
    std::time::Duration::from_secs(capped)
}

// ---------------------------------------------------------------------------
// SMTP + DNS plumbing
// ---------------------------------------------------------------------------

// Build the SMTP envelope: the stored `mail_from` as the reverse path and the
// job's `rcpt_to` as the single forward path.
fn build_envelope(
    mail_from: &str,
    recipient: &str,
) -> Result<Envelope, Box<dyn std::error::Error + Send + Sync>> {
    let from: Option<LettreAddress> = if mail_from.trim().is_empty() {
        None
    } else {
        Some(mail_from.parse().map_err(|e| err("invalid MAIL FROM", e))?)
    };
    let to = vec![recipient.parse().map_err(|e| err("invalid RCPT TO", e))?];
    Ok(Envelope::new(from, to).map_err(|e| err("failed to build envelope", e))?)
}

// Resolve a recipient's domain to MX hosts, highest priority (lowest number) first.
async fn lookup_mx_hosts(
    recipient: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let domain = recipient
        .split('@')
        .last()
        .ok_or_else(|| err("invalid RCPT TO: no domain part", "recipient"))?;

    let resolver = TokioAsyncResolver::tokio_from_system_conf()
        .map_err(|e| err("failed to build DNS resolver", e))?;
    let response = resolver
        .mx_lookup(domain)
        .await
        .map_err(|e| err("MX lookup failed", e))?;

    let mut records: Vec<_> = response.iter().collect();
    // Sort by preference ascending; ties keep resolver order.
    records.sort_by_key(|mx| mx.preference());

    let mut hosts = Vec::new();
    for mx in records {
        let host = mx.exchange().to_string();
        // Normalise the trailing DNS root dot for the SMTP target.
        let host = host.trim_end_matches('.');
        if !host.is_empty() {
            hosts.push(host.to_string());
        }
    }
    Ok(hosts)
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{split, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

        // Write one CRLF-terminated reply through the connection's write half.
        // `split()` gives independent read/write halves, so the sink can interleave
        // reads and writes on one socket with no shared-mutable-borrow problem.
    async fn write_resp<W: AsyncWrite + Unpin>(
         w: &mut W,
        bytes: &[u8],
     ) {
        let _ = w.write_all(bytes).await;
        }

        // A minimal submission-style SMTP sink: it accepts a connection, greets, and
        // responds 250/354/221 so an outbound delivery attempt records a 250 without
        // any external server or DNS. Returns the bound localhost port. A `BufReader`
        // is required because this tokio build places `read_until` on `AsyncBufReadExt`.
    async fn smtp_sink_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => continue,
                     };
                tokio::spawn(handle_sink_session(sock));
                 }
             });
        port
         }

        // Drive one accepted TCP connection to a successful `QUIT`-free acceptance.
    async fn handle_sink_session(sock: tokio::net::TcpStream) {
        let (rd, mut wr) = split(sock);
        let mut reader = BufReader::new(rd);
        let mut buf: Vec<u8> = Vec::new();
            // The greeting precedes the client's first request.
        write_resp(&mut wr, b"220 sink ESMTP\r\n").await;
        loop {
            let n = reader.read_until(b'\n', &mut buf).await;
            match n {
                Ok(0) => break, // client closed (EOF / QUIT without a final line)
                Ok(_) => {}
                Err(_) => break,
                }
            let cmd = String::from_utf8_lossy(&buf).trim_end().to_ascii_uppercase();
            if cmd.starts_with("EHLO") || cmd.starts_with("HELO") {
                write_resp(&mut wr, b"250 sink\r\n").await;
                } else if cmd.starts_with("MAIL") {
                write_resp(&mut wr, b"250 OK\r\n").await;
                } else if cmd.starts_with("RCPT") {
                write_resp(&mut wr, b"250 OK\r\n").await;
                } else if cmd == "DATA" {
                    // Admit the body, absorb it to the CRLF-`.` terminator, then
                    // accept the message so the transport records a 250.
                write_resp(&mut wr, b"354 Go ahead\r\n").await;
                loop {
                    let n = reader.read_until(b'\n', &mut buf).await;
                    if n.unwrap_or(0) == 0 {
                        break;
                        }
                    if String::from_utf8_lossy(&buf).trim_end() == "." {
                        break;
                        }
                    }
                write_resp(&mut wr, b"250 2.0.0 OK queued\r\n").await;
                } else if cmd.starts_with("QUIT") {
                write_resp(&mut wr, b"221 bye\r\n").await;
                break;
                } else {
                    // RSET / NOOP / VRFY / ...: acknowledge so the client can continue.
                write_resp(&mut wr, b"250 OK\r\n").await;
                }
            }
        }
    // An implicit-TLS (SMTPS) sink: accept a TCP connection, complete a TLS
    // handshake with an acceptor built from a self-signed `native_tls::Identity`,
    // then drive the same cleartext-style SMTP handshake so an implicit-TLS
    // outbound delivery records a 250 with no external server. Used to exercise
    // `deliver_hosts` over `OutboundTlsMode::Implicit`. Returns `None` when
    // `openssl` is unavailable, so the caller can skip in a hermetic env.
    async fn smtps_sink_port() -> Option<u16> {
        let id = generate_self_signed_identity()?;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor: ::tokio_native_tls::TlsAcceptor =
            ::native_tls::TlsAcceptor::new(id).ok()?.into();
        tokio::spawn(async move {
            loop {
                let stream = match listener.accept().await {
                    Ok((s, _)) => s,
                    Err(_) => continue,
                      };
                     // A failed handshake (a client that disconnected) just ends this
                     // connection; the next accept starts fresh.
                let tls = match acceptor.accept(stream).await {
                    Ok(t) => t,
                    Err(_) => continue,
                      };
                      // One connection at a time suffices for the single-job test.
                handle_smtps_session(tls).await;
                   }
               });
        Some(port)
           }

          // Drive one accepted *implicit-TLS* connection through a `QUIT`-free SMTP
          // handshake - identical responses to `handle_sink_session`, over a TLS
          // stream produced by `tokio_native_tls::TlsStream`.
    async fn handle_smtps_session(tls: ::tokio_native_tls::TlsStream<tokio::net::TcpStream>) {
        let (rd, mut wr) = split(tls);
        let mut reader = BufReader::new(rd);
        let mut buf: Vec<u8> = Vec::new();
        write_resp(&mut wr, b"220 sink ESMTP\r\n").await;
        loop {
            let n = reader.read_until(b'\n', &mut buf).await;
            match n {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
                  }
            let cmd = String::from_utf8_lossy(&buf).trim_end().to_ascii_uppercase();
            if cmd.starts_with("EHLO") || cmd.starts_with("HELO") {
                write_resp(&mut wr, b"250 sink\r\n").await;
                  } else if cmd.starts_with("MAIL") {
                write_resp(&mut wr, b"250 OK\r\n").await;
                  } else if cmd.starts_with("RCPT") {
                write_resp(&mut wr, b"250 OK\r\n").await;
                  } else if cmd == "DATA" {
                     write_resp(&mut wr, b"354 Go ahead\r\n").await;
                loop {
                    let n = reader.read_until(b'\n', &mut buf).await;
                    if n.unwrap_or(0) == 0 {
                        break;
                          }
                    if String::from_utf8_lossy(&buf).trim_end() == "." {
                        break;
                          }
                      }
                write_resp(&mut wr, b"250 2.0.0 OK queued\r\n").await;
                  } else if cmd.starts_with("QUIT") {
                write_resp(&mut wr, b"221 bye\r\n").await;
                break;
                  } else {
                write_resp(&mut wr, b"250 OK\r\n").await;
                  }
              }
          }

     // Generate a short-lived self-signed cert + a PKCS#8 key with the `openssl`
     // CLI (the same tool the inbound TLS smoke test uses) and turn them into a
     // `native_tls::Identity`. Returns `None` when `openssl` is unavailable.
    fn generate_self_signed_identity() -> Option<::native_tls::Identity> {
        if ::std::process::Command::new("openssl").arg("--version").output().is_err() {
            return None;
          }
        let dir = ::std::env::temp_dir()
                  .join(format!("zonemail-out-smtps-{}", ::std::process::id()));
        let _ = ::std::fs::remove_dir_all(&dir);
         ::std::fs::create_dir_all(&dir).ok()?;
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let out = ::std::process::Command::new("openssl")
                  .args([
                      "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                      "-subj", "/CN=127.0.0.1",
                      "-keyout", key.to_string_lossy().as_ref(),
                      "-out", cert.to_string_lossy().as_ref(),
                  ])
                  .output()
                  .ok()?;
        assert!(out.status.success(), "openssl generated the cert");
        let cert_bytes = ::std::fs::read(&cert).ok()?;
        let key_bytes = ::std::fs::read(&key).ok()?;
        let _ = ::std::fs::remove_dir_all(&dir);
         ::native_tls::Identity::from_pkcs8(&cert_bytes, &key_bytes).ok()
        }


        // Open a port, drop the listener, and return the now-free port so a client
        // connection is refused — this drives the retry path without DNS.
    async fn refused_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        tokio::time::sleep(Duration::from_millis(50)).await;
        port
        }

        // Reload a single `Outbound` row by id, asserting it still exists.
    async fn load_outbound(db: &mut toasty::Db, id: u64) -> Outbound {
        toasty::query!
                (Outbound filter .id == #id).first().exec(db).await.unwrap().expect("outbound present")
        }

        // Create a fresh in-memory DB plus one stored `Message` and a Queued `Outbound`
        // job for `sender` -> `recipient`. Returns (db, message_id, outbound_id). The
        // message is built exactly as the API builds it, so the worker's stored form
        // is identical to what `send_email` would produce.
    async fn fresh_message_and_job(sender: &str, recipient: &str) -> (toasty::Db, u64, u64) {
        let mut db = crate::app::AppState::connect_in_memory().await.unwrap().db;
        let message = LettreMessage::builder()
                .from(sender.parse().expect("from mailbox"))
                .to(recipient.parse().expect("to mailbox"))
                .subject("Hello".to_string())
                .body("Hello, test world!\r\n".to_string())
                .expect("build message");
        let (mid, oid) =
             enqueue_outbound(&mut db, sender, recipient, message).await.expect("enqueue_outbound");
        (db, mid, oid)
        }

        // ----- pure logic -----

        #[test]
     fn backoff_delay_doubles_and_caps() {
         let cfg = OutboundWorkerConfig::default(); // base 60s, cap 3600s
         assert_eq!(backoff_delay(&cfg, 1).as_secs(), 60);
         assert_eq!(backoff_delay(&cfg, 2).as_secs(), 120);
         assert_eq!(backoff_delay(&cfg, 4).as_secs(), 480);
         assert_eq!(backoff_delay(&cfg, 64).as_secs(), 3600); // clamped to cap
        }

        #[test]
     fn submission_port_defaults_to_25_and_is_overridable() {
         assert_eq!(OutboundWorkerConfig::default().submission_port(), 25);
         assert_eq!(
             OutboundWorkerConfig::default().with_submission_port(1025).submission_port(),
              1025
            );
        }

        // ----- queue state machine -----

        #[tokio::test]
     async fn fail_or_requeue_requeues_before_cap() {
          let (mut db, _mid, oid) =
              fresh_message_and_job("alice@example.com", "bob@other.example").await;
          let mut job = load_outbound(&mut db, oid).await;
          fail_or_requeue(
                   &mut db,
                   &mut job,
                   &OutboundWorkerConfig::default(),
              jiff::Timestamp::now(),
                   "transient",
             )
             .await
             .unwrap();
          let reread = load_outbound(&mut db, oid).await;
          assert_eq!(reread.status, OutboundStatus::Queued, "retried, still Queued");
          assert_eq!(reread.attempts, 1, "one attempt recorded");
          assert!(reread.next_attempt_at.is_some(), "backoff scheduled");
          assert_eq!(reread.last_error.as_deref(), Some("transient"));
        }

        #[tokio::test]
     async fn fail_or_requeue_marks_failed_at_cap() {
          let (mut db, _mid, oid) = fresh_message_and_job("a@x.example", "b@y.example").await;
          let cap = OutboundWorkerConfig::default().max_attempts;
          let mut job = load_outbound(&mut db, oid).await;
          job.attempts = cap; // simulate having exhausted the attempt cap
          fail_or_requeue(
                   &mut db,
                   &mut job,
                   &OutboundWorkerConfig::default(),
              jiff::Timestamp::now(),
                   "exhausted",
             )
             .await
             .unwrap();
          let reread = load_outbound(&mut db, oid).await;
          assert_eq!(reread.status, OutboundStatus::Failed, "terminal past the cap");
          assert!(reread.next_attempt_at.is_none(), "no further attempts scheduled");
        }

        #[tokio::test]
     async fn reclaim_in_progress_requeues_stale_jobs() {
          let mut db = crate::app::AppState::connect_in_memory().await.unwrap().db;
          let job = toasty::create! {
              Outbound {
                  message_id: 0,
                  rcpt_to: "stale@x.example".to_string(),
                  sender_id: "s@x.example".to_string(),
                  status: OutboundStatus::InProgress,
                  attempts: 0,
                  next_attempt_at: None,
                  last_error: Some("crash".to_string()),
                   }
               }
               .exec(&mut db)
               .await
               .unwrap();
          let oid = job.id;
          reclaim_in_progress(&mut db).await;
          let reread = load_outbound(&mut db, oid).await;
          assert_eq!(reread.status, OutboundStatus::Queued, "stale job reclaimed");
          assert!(reread.next_attempt_at.is_none());
        }

        // ----- the actual SMTP handoff, against the local sink -----

        #[tokio::test]
     async fn deliver_hosts_delivers_to_local_sink() {
          let (mut db, mid, oid) =
              fresh_message_and_job("alice@example.com", "bob@other.example").await;
          let outbound = load_outbound(&mut db, oid).await;
          let stored = Message::get_by_id(&mut db, &mid).await.expect("stored message present");
          let envelope =
              build_envelope(&stored.mail_from, &outbound.rcpt_to).expect("build envelope");
          let port = smtp_sink_port().await;
          let cfg = OutboundWorkerConfig::default().with_submission_port(port);
          deliver_hosts(
                   &mut db,
              outbound,
                   &stored,
                   &envelope,
                   &cfg,
              jiff::Timestamp::now(),
                   &["127.0.0.1".to_string()],
             )
             .await
             .expect("deliver to sink");
          let job = load_outbound(&mut db, oid).await;
          assert_eq!(job.status, OutboundStatus::Delivered, "job Delivered via sink");
          assert_eq!(job.attempts, 1, "one successful attempt");
          let statuses =
              toasty::query!(DeliveryStatus filter .outbound_id == #oid)
                   .exec(&mut db)
                   .await
                   .unwrap();
          assert_eq!(statuses.len(), 1, "one DeliveryStatus row per attempt");
          assert_eq!(statuses[0].code, Some(250), "recorded a 250 success code");
        }

         #[tokio::test]
     async fn deliver_hosts_smtps_implicit_tls_delivers() {
      // End-to-end implicit-TLS (SMTPS) delivery: a job is delivered through
      // `OutboundTlsMode::Implicit` to the self-signed in-process acceptor,
      // proving the TLS path records a 250. Skips when `openssl` is
      // unavailable (a hermetic env), where the cleartext path always covers
      // delivery via `deliver_hosts_delivers_to_local_sink`.
        let port = match smtps_sink_port().await {
            Some(p) => p,
            None => {
                eprintln!("[skip] openssl unavailable; skipping SMTPS e2e");
                return;
           }
         };
        let (mut db, mid, oid) =
            fresh_message_and_job("alice@example.com", "bob@example.com").await;
        let outbound = load_outbound(&mut db, oid).await;
        let stored = Message::get_by_id(&mut db, &mid).await.expect("stored message present");
        let envelope =
            build_envelope(&stored.mail_from, &outbound.rcpt_to).expect("build envelope");
        // Relay to the implicit-TLS sink on a self-signed cert; `insecure`
        // accepts the untrusted localhost certificate, as a real relay would be
        // trusted via `danger_accept_invalid_certs`.
        let cfg = OutboundWorkerConfig {
            transport: OutboundTransport {
                tls: OutboundTlsMode::Implicit,
                relay: Some(crate::config::OutboundRelay {
                    host: Some("127.0.0.1".to_string()),
                    port: port,
                    user: None,
                    password: None,
                    password_env: None,
               }),
                   insecure: true,
         ..Default::default()
             },
         ..Default::default()
             };
        deliver_hosts(
                     &mut db,
                 outbound,
                     &stored,
                     &envelope,
                     &cfg,
                 jiff::Timestamp::now(),
                     &["127.0.0.1".to_string()],
           )
           .await
           .expect("deliver over implicit TLS");
        let job = load_outbound(&mut db, oid).await;
        assert_eq!(job.status, OutboundStatus::Delivered, "job Delivered via SMTPS sink");
        assert_eq!(job.attempts, 1, "one successful implicit-TLS attempt");
         }

   #[tokio::test]
     async fn deliver_hosts_requeues_when_port_refused() {
          let (mut db, mid, oid) =
              fresh_message_and_job("alice@example.com", "bob@no.example").await;
          let outbound = load_outbound(&mut db, oid).await;
          let stored =
              Message::get_by_id(&mut db, &mid).await.expect("stored message present");
          let envelope =
              build_envelope(&stored.mail_from, &outbound.rcpt_to).expect("build envelope");
          let port = refused_port().await;
          let cfg = OutboundWorkerConfig::default().with_submission_port(port);
              // A refused connection is a failed attempt -> re-queue, never a panic.
          deliver_hosts(
                   &mut db,
              outbound,
                   &stored,
                   &envelope,
                   &cfg,
              jiff::Timestamp::now(),
                   &["127.0.0.1".to_string()],
             )
             .await
             .expect("re-queue on refusal");
          let job = load_outbound(&mut db, oid).await;
          assert_eq!(job.status, OutboundStatus::Queued, "refused delivery is retried");
          assert_eq!(job.attempts, 1, "one attempted delivery before re-queue");
          assert!(job.next_attempt_at.is_some(), "backoff scheduled for the retry");
          assert!(job.last_error.is_some(), "failure reason recorded");
        }
          #[test]
    fn build_outbound_tls_plain_is_none() {
           // The default posture is clear-text and never touches the TLS stack.
        let t = OutboundTransport::default();
        assert!(!t.tls_enabled());
        assert!(!t.relay_configured());
        let tls = build_outbound_tls(&t, "mx.example.com").expect("plain builds");
        assert!(matches!(tls, Tls::None));
        }

          #[test]
    fn build_outbound_tls_maps_enabled_modes() {
           // Opportunistic / Required / Implicit all yield a real `Tls` selection
      // (a native connector is built), distinct from the `Plain` =`None` case.
      // `insecure` lets the native connector build for a localhost/self-signed name.
        for mode in [OutboundTlsMode::Opportunistic, OutboundTlsMode::Required,
        OutboundTlsMode::Implicit] {
        let t = OutboundTransport {
             tls: mode,
             insecure: true,
               ..Default::default()
               };
        let tls = build_outbound_tls(&t, "smtp.example.com").expect("tls builds");
        assert!(!matches!(tls, Tls::None), "mode {mode:?} produced a TLS selection");
          }
        }
}
