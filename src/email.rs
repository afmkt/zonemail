use crate::app::AppState;
use crate::db::{Inbound, Mailbox, Message};
use mail_parser::{Address, MessageParser};
use smtpd::{async_trait, start_server, Error as SmtpError, Response, Session, SmtpConfig};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{info, warn};

// Best-effort first e-mail address from a parsed `From:`/`To:`/etc. header value
// (`mail_parser::Address` has no `Display` impl). Used for single-recipient fields.
fn address_to_string(addr: &Address) -> String {
    match addr {
        Address::List(list) => list
              .first()
              .and_then(|a| a.address.as_deref())
              .map(String::from)
              .unwrap_or_default(),
        Address::Group(groups) => groups
              .iter()
              .find_map(|g| g.addresses.first().and_then(|a| a.address.as_deref()))
              .map(String::from)
              .unwrap_or_default(),
      }
}

// Every address carried by a parsed header value (`From:`/`To:`/`Cc:`/`Bcc:`),
// preserving lists rather than collapsing to a single address.
fn address_list_to_strings(addr: &Address) -> Vec<String> {
    match addr {
        Address::List(list) => list
              .iter()
              .filter_map(|a| a.address.as_deref().map(String::from))
              .collect(),
        Address::Group(groups) => groups
              .iter()
              .flat_map(|g| g.addresses.iter())
              .filter_map(|a| a.address.as_deref().map(String::from))
              .collect(),
      }
}

// 1. Define your connection-scoped handler
struct ZonedMailHandler {
    state: Arc<AppState>,
      // Accumulate every accepted `RCPT TO:` so a message with multiple
      // recipients produces one Inbound row per recipient (not just the last one).
    recipients: Vec<String>,
}

#[async_trait]
impl smtpd::SmtpHandler for ZonedMailHandler {
      // Intercepts `RCPT TO:` to check if the address was provisioned via your API
    async fn handle_rcpt(&mut self, _session: &Session, to: &str) -> Result<Response, SmtpError> {
        let clean_email = to
              .trim_matches(|c| c == '<' || c == '>' || c == ' ')
              .to_lowercase();
        info!("Checking inbound recipient: {clean_email}");
          // Security: only accept a recipient that is a provisioned mailbox in our
          // database. Without this check the server would be an open relay that
          // accepts (and relays) mail for arbitrary addresses on the Internet.
        let mailbox_exists = {
            let mut db = self.state.db.clone();
            Mailbox::get_by_id(&mut db, &clean_email).await.is_ok()
          };

        if mailbox_exists {
            self.recipients.push(clean_email);
            Ok(Response::Default) // Tells smtpd to accept the recipient (250 OK)
          } else {
            warn!("Rejected unknown recipient: {clean_email}");
            Err(SmtpError::Abort) // Rejects the recipient with a 5xx error
          }
      }

      // Intercepts the final `DATA` payload once transmission is complete
    async fn handle_email(
          &mut self,
          _session: &Session,
          data: Vec<u8>,
        ) -> Result<Response, SmtpError> {
            // Take ownership of the accepted recipients so we don't keep per-connection state.
            let mut recipients = std::mem::take(&mut self.recipients);

            // Fall back to "unknown" only when no recipient was recorded so the message
            // itself is never lost even if `handle_rcpt` rejected them all.
            if recipients.is_empty() {
                warn!("No valid recipient recorded; storing message under 'unknown'");
                recipients = vec!["unknown".to_string()];
              }

            // Parse headers best-effort. `raw` always preserves the full original payload
            // (headers + body), so a malformed message is still stored losslessly.
            let parsed = MessageParser::default().parse(&data);
            if parsed.is_none() {
                warn!("Failed to parse raw email payload; headers left empty, raw bytes preserved");
              }

            let from_address = parsed.as_ref().and_then(|e| e.from()).map(address_to_string);
            let subject = parsed.as_ref().and_then(|e| e.subject()).map(|s| s.to_string());
            let content_type =
                parsed.as_ref().and_then(|e| e.header_raw("content-type")).map(|s| s.to_string());
            let message_id_header =
                parsed.as_ref().and_then(|e| e.message_id()).map(|s| s.to_string());
            let to = parsed
                  .as_ref()
                  .and_then(|e| e.to())
                  .map_or_else(Vec::new, address_list_to_strings);
            let cc = parsed
                  .as_ref()
                  .and_then(|e| e.cc())
                  .map_or_else(Vec::new, address_list_to_strings);
            let bcc = parsed
                  .as_ref()
                  .and_then(|e| e.bcc())
                  .map_or_else(Vec::new, address_list_to_strings);

              // `smtpd`'s `SmtpHandler` exposes no hook for the envelope `MAIL FROM`, so the
              // `From:` header is used as the best available proxy for `mail_from`.
            let mail_from = from_address.clone().unwrap_or_default();

            info!(
                  "Parsed email -> Subject: '{}', Message-ID: '{}', {} recipient(s)",
                subject.as_deref().unwrap_or("(no subject)"),
                message_id_header.as_deref().unwrap_or("(none)"),
                recipients.len(),
              );

            let mut db = self.state.db.clone();

              // De-duplication is done here in the handler rather than enforced by the DB,
              // because the `Message-ID` header is not always present or valid. When we do
              // have a usable `Message-ID`, first reuse an already-stored `Message` (e.g. a
              // relay re-sending the same message) instead of inserting a duplicate.
            let dedup_key =
                message_id_header.as_deref().map(str::trim).filter(|m| !m.is_empty());

            let mut message_id: Option<u64> = None;
            if let Some(key) = dedup_key {
                match toasty::query!(Message filter .message_id_header == #key)
                    .first()
                    .exec(&mut db)
                    .await
                {
                    Ok(Some(existing)) => {
                        info!("Reusing existing message {} for Message-ID '{key}'", existing.id);
                        message_id = Some(existing.id);
                    }
                    Err(e) => {
                        warn!("Failed to look up message by Message-ID '{key}': {e}");
                    }
                    Ok(None) => {}
                }
              }

            // No usable `Message-ID`, or no existing copy: store the message.
            if message_id.is_none() {
                let created = toasty::create!(Message {
                        mail_from,
                     raw: data,
                    subject,
                    message_id_header,
                    content_type,
                    from_address,
                    to,
                    cc,
                    bcc,
                 })
                .exec(&mut db)
                .await
                .map_err(|e| {
                    warn!("Failed to store inbound message: {e}");
                    SmtpError::Abort
                 })?;
                message_id = Some(created.id);
                info!("Stored new message {}", created.id);
              }

              // Guaranteed to be set by the lookup-reuse or the insert above.
                let message_id = message_id.expect("message id is always assigned above");

              // One Inbound row per accepted recipient, all pointing at the shared message.
            for recipient in &recipients {
                toasty::create!(Inbound {
                        message_id,
                     rcpt_to: recipient.clone(),
                 })
                .exec(&mut db)
                .await
                .map_err(|e| {
                    warn!("Failed to save inbound link for {recipient}: {e}");
                    SmtpError::Abort
                 })?;
                info!("Stored inbound link for {recipient} -> message {message_id}");
              }

            Ok(Response::Default)
        }
}

// 2. Define the Handler Factory required by smtpd
struct ZonedHandlerFactory {
    state: Arc<AppState>,
}

impl smtpd::SmtpHandlerFactory for ZonedHandlerFactory {
    type Handler = ZonedMailHandler;

    fn new_handler(&self, _session: &Session) -> Self::Handler {
        ZonedMailHandler {
            state: self.state.clone(),
            recipients: Vec::new(),
          }
     }
}

// 3. Entry point to spawn the server in your main application
pub async fn run_mail_server(
    bind_addr: SocketAddr,
    state: Arc<AppState>,
) -> Result<(), std::io::Error> {
    let config = SmtpConfig {
        bind_addr: bind_addr.to_string(),
        require_auth: false, // Inbound mail from public MX servers won't have your API auth
          ..Default::default()
      };

    let factory = ZonedHandlerFactory { state };

    info!("Starting smtpd inbound mail server on {}", config.bind_addr);
    start_server(config, factory).await
}
