use crate::app::AppState;
use mail_parser::MessageParser;
use smtpd::{Error as SmtpError, Response, Session, SmtpConfig, async_trait, start_server};
use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{info, warn}; // Bring MessageParser into scope
// 1. Define your connection-scoped handler
struct ZonedMailHandler {
    state: Arc<AppState>,
    recipient: Option<String>,
}

#[async_trait]
impl smtpd::SmtpHandler for ZonedMailHandler {
    // Intercepts `RCPT TO:` to check if the address was provisioned via your API
    async fn handle_rcpt(&mut self, _session: &Session, to: &str) -> Result<Response, SmtpError> {
        let clean_email = to
            .trim_matches(|c| c == '<' || c == '>' || c == ' ')
            .to_lowercase();
        info!("Checking inbound recipient: {}", clean_email);

        // TODO: Query your database using self.state.db to verify if `clean_email` exists
        let mailbox_exists = true; // Replace with actual DB check

        if mailbox_exists {
            self.recipient = Some(clean_email);
            Ok(Response::Default) // Tells smtpd to accept the recipient (250 OK)
        } else {
            warn!("Rejected unknown recipient: {}", clean_email);
            Err(SmtpError::Abort) // Rejects the recipient with a 5xx error
        }
    }

    // Intercepts the final `DATA` payload once transmission is complete
    async fn handle_email(
        &mut self,
        _session: &Session,
        data: Vec<u8>,
    ) -> Result<Response, SmtpError> {
        let recipient = self
            .recipient
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        info!(
            "Received email payload for {} ({} bytes)",
            recipient,
            data.len()
        );

        if let Some(email) = MessageParser::default().parse(&data) {
            // Parse the raw bytes using mail-parser

            let subject = email.subject().unwrap_or("(No Subject)");
            let body = email.body_text(0).unwrap_or(Cow::Borrowed("(Empty Body)"));

            info!(
                "Parsed Email -> Subject: '{}', Body length: {}",
                subject,
                body.len()
            );

            // TODO: Save parsed email content or raw bytes to database/storage for the user
        } else {
            warn!("Failed to parse raw email payload");
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
            recipient: None,
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
