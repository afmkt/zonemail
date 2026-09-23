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

use crate::db::{DeliveryStatus, Message, Outbound, OutboundStatus};
// `TransactionMode` is not re-exported by `toasty`, so name it from the core.
use toasty_core::driver::operation::TransactionMode;

use hickory_resolver::TokioAsyncResolver;
use jiff::Timestamp;
use lettre::{
    address::{Address as LettreAddress, Envelope},
    transport::smtp::AsyncSmtpTransport,
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
}

impl Default for OutboundWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: std::time::Duration::from_secs(5),
            max_attempts: 8,
            base_backoff: std::time::Duration::from_secs(60),
            max_backoff: std::time::Duration::from_secs(3_600),
        }
    }
}

// ---------------------------------------------------------------------------
// Producer
// ---------------------------------------------------------------------------

/// Persist a message and enqueue it for delivery. Returns the new `Outbound`
/// id (the queued job). Performs **no** network I/O.
///
/// `sender` must be a provisioned mailbox because `Outbound.sender_id` is a
/// non-nullable foreign key.
pub async fn enqueue_outbound(
    db: &mut toasty::Db,
    sender: &str,
    recipient: &str,
    message: LettreMessage,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
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

    Ok(job.id)
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
    mut outbound: Outbound,
    config: &OutboundWorkerConfig,
    now: Timestamp,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Load the message content to transmit, exactly as stored.
    let message = Message::get_by_id(db, &outbound.message_id).await
        .map_err(|e| err("failed to load Message for Outbound", e))?;

          // Build the envelope from the stored MAIL FROM and the job's RCPT TO.
    let envelope = build_envelope(&message.mail_from, &outbound.rcpt_to)?;

    // Resolve MX hosts, highest priority first.
    let hosts = lookup_mx_hosts(&outbound.rcpt_to)
        .await
        .map_err(|e| err("failed to look up MX hosts", e))?;
    if hosts.is_empty() {
        warn!("No MX records for recipient of Outbound {}; requeueing for retry", outbound.id);
        return fail_or_requeue(db, &mut outbound, config, now, "no MX records for domain").await;
    }

    // Try each MX in priority order, recording one DeliveryStatus per attempt.
    let mut last_error: Option<String> = None;
    for host in hosts {
        info!("Outbound {} attempting MX {host}", outbound.id);
        let transport =
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host.clone())
                .port(SUBMISSION_PORT)
                .build();

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
                info!(
                    "Delivered Outbound {} -> {} via {host}",
                    outbound.id, outbound.rcpt_to
                );
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
