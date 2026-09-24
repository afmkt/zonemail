use crate::app::AppState;
use crate::db::{Inbound, Mailbox, Message};
use crate::send::enqueue_forward;
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

                  // Forward inbound mail to the user's registered external address.
                  // Best-effort: a forwarding enqueue failure is logged but does
                  // NOT abort the SMTP session or discard the stored Inbound row.
               for recipient in &recipients {
                   if let Ok(mailbox) = Mailbox::get_by_id(&mut db, recipient).await {
                       if let Some(ref forward_to) = mailbox.forward_to {
                           if let Err(e) =
                               enqueue_forward(&mut db, message_id, recipient, forward_to).await
                             {
                               warn!(
                                     "Forwarding {recipient} -> {forward_to}: failed to enqueue: {e}"
                                 );
                             } else {
                               info!("Forwarding {recipient} -> {forward_to}: enqueued OK");
                             }
                        }
                    }
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

// ===========================================================================
// Tests
// ===========================================================================
//
// The inbound handler reduces a raw SMTP payload to a set of fields through
// `mail_parser`. These tests exercise that reduction directly — the pure
// address helpers plus the header extraction the handler performs — using real
// RFC-style payloads and hand-built `Address` values for the degenerate cases.
#[cfg(test)]
mod tests {
    use super::*;
    use mail_parser::{Addr, Group, MimeHeaders};
    use std::borrow::Cow;

      /// A minimal but representative payload: From, To, Subject, Message-ID,
      /// Content-Type, and a body.
    fn sample_message() -> Vec<u8> {
        b"Date: Mon, 1 Jan 2024 00:00:00 +0000\r\n\
             From: Alice Smith <alice@example.com>\r\n\
             To: Bob Jones <bob@ext.com>\r\n\
             Subject: Hello world\r\n\
             Message-ID: <1234@example.com>\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             A short body.\r\n"
                 .to_vec()
          }

          #[test]
    fn parse_extracts_from_subject_and_message_id() {
        let raw = sample_message();
        let msg = MessageParser::default().parse(&raw).expect("parses");

             // Single-address `From:` collapses to the email, not the display name.
        let from = msg.from().expect("From present");
        assert_eq!(address_to_string(from), "alice@example.com");
        assert_eq!(msg.subject().unwrap(), "Hello world");
        assert_eq!(msg.message_id().unwrap(), "1234@example.com");
          }

          #[test]
    fn parse_extracts_content_type() {
        let raw = sample_message();
        let msg = MessageParser::default().parse(&raw).expect("parses");
        let ct = msg.content_type().expect("content-type parsed");
             // `text/plain; charset=utf-8` -> type "text", subtype "plain".
        assert_eq!(ct.c_type.as_ref(), "text");
        assert_eq!(ct.c_subtype.as_deref(), Some("plain"));
          }

          #[test]
    fn multiple_to_recipients_preserved_in_order() {
        let raw = b"From: a@x.com\r\nTo: b@ext.com, Carol <c@ext.com>, d@ext.com\r\n\r\nbody\r\n"
                 .to_vec();
        let msg = MessageParser::default().parse(&raw).expect("parses");
        let to = msg.to().expect("To present");
        assert_eq!(
             address_list_to_strings(to),
             vec!["b@ext.com".to_string(), "c@ext.com".to_string(), "d@ext.com".to_string()],
            );
          }

          #[test]
    fn missing_subject_is_none() {
        let raw = b"From: a@x.com\r\nTo: b@ext.com\r\n\r\nonly a body\r\n".to_vec();
        let msg = MessageParser::default().parse(&raw).expect("parses");
        assert!(msg.subject().is_none());
          }

          #[test]
    fn address_to_string_is_empty_when_only_a_display_name() {
             // A bare name with no `<addr>` yields no address (empty string).
        let addr = Address::List(vec![Addr {
             name: Some(Cow::Borrowed("Just a Name")),
             address: None,
            }]);
        assert_eq!(address_to_string(&addr), "");
        let strings = address_list_to_strings(&addr);
        assert!(strings.is_empty());
          }

          #[test]
    fn address_list_flattens_groups() {
             // A group's members are flattened into the address list.
        let addr = Address::Group(vec![Group {
             name: Some(Cow::Borrowed("team")),
             addresses: vec![
                Addr {
                   name: None,
                   address: Some(Cow::Borrowed("m1@team.com")),
                    },
                Addr {
                   name: None,
                   address: Some(Cow::Borrowed("m2@team.com")),
                    },
                  ],
            }]);
        assert_eq!(
             address_list_to_strings(&addr),
             vec!["m1@team.com".to_string(), "m2@team.com".to_string()],
            );
             // The first address of the first group is the "single" representative.
        assert_eq!(address_to_string(&addr), "m1@team.com");
          }

          #[test]
    fn empty_list_yields_no_addresses() {
        let addr = Address::List(Vec::new());
        assert!(address_list_to_strings(&addr).is_empty());
        assert_eq!(address_to_string(&addr), "");
          }
}
