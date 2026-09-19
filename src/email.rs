use crate::app::AppState;
use crate::db::Mailbox;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{info, warn};
// Note: Samotop can be configured with custom handlers, or you can run a
// custom Tokio TCP loop that parses SMTP lines manually or integrates with Samotop primitives.
// Below is the clean integration pattern using Samotop's service architecture.

pub async fn run_mail_server(
    bind_addr: SocketAddr,
    app_ctx: Arc<AppState>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting real SMTP inbound server on {}", bind_addr);

    // Bind the TCP listener using Tokio
    let listener = TcpListener::bind(&bind_addr).await?;

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(val) => val,
            Err(e) => {
                warn!("Failed to accept incoming SMTP connection: {}", e);
                continue;
            }
        };

        let ctx = app_ctx.clone();
        tokio::spawn(async move {
            info!("Accepted inbound SMTP connection from {}", peer_addr);
            // Here you handle the SMTP state machine or pass the stream
            // to Samotop's session processor.
            // For a lightweight custom parser or Samotop service handler:
            if let Err(e) = handle_smtp_session(stream, ctx).await {
                warn!("Error handling SMTP session from {}: {}", peer_addr, e);
            }
        });
    }
}

async fn handle_smtp_session(
    mut stream: tokio::net::TcpStream,
    app_ctx: Arc<AppState>,
) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // 1. Send SMTP Greeting (Banner)
    stream
        .write_all(b"220 zonemail.net ESMTP Ready\r\n")
        .await?;

    let mut buf = [0; 1024];
    let mut db = app_ctx.db.clone();

    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) => return Ok(()), // Connection closed
            Ok(n) => n,
            Err(e) => return Err(e.into()),
        };

        let command = String::from_utf8_lossy(&buf[..n]);
        let upper_cmd = command.trim().to_uppercase();

        if upper_cmd.starts_with("HELO") || upper_cmd.starts_with("EHLO") {
            stream
                .write_all(b"250-zonemail.net Hello\r\n250 OK\r\n")
                .await?;
        } else if upper_cmd.starts_with("MAIL FROM:") {
            stream.write_all(b"250 2.1.0 Sender OK\r\n").await?;
        } else if upper_cmd.starts_with("RCPT TO:") {
            // Extract recipient email address from command string (e.g., "RCPT TO:<user@domain.com>")
            let email = extract_email(&command);

            // Query the Toasty database to check if the mailbox exists!
            let mailbox_exists = if let Some(addr) = email {
                Mailbox::get_by_id(&mut db, &addr).await.is_ok()
            } else {
                false
            };

            if mailbox_exists {
                stream.write_all(b"250 2.1.5 Recipient OK\r\n").await?;
            } else {
                // Reject unknown mailbox gracefully per RFC 5321
                stream
                    .write_all(b"550 5.1.1 User unknown / Mailbox not provisioned\r\n")
                    .await?;
            }
        } else if upper_cmd.starts_with("DATA") {
            stream
                .write_all(b"354 Start mail input; end with <CRLF>.<CRLF>\r\n")
                .await?;
            // Read data payload until <CRLF>.<CRLF> and process/save raw email bytes
        } else if upper_cmd.starts_with("QUIT") {
            stream.write_all(b"221 2.0.0 Bye\r\n").await?;
            break;
        } else {
            stream.write_all(b"500 Command unrecognized\r\n").await?;
        }
    }

    Ok(())
}

// Simple helper to parse out the email address inside angle brackets <...>
fn extract_email(cmd: &str) -> Option<String> {
    let start = cmd.find('<')?;
    let end = cmd.find('>')?;
    if start < end {
        Some(cmd[start + 1..end].to_string())
    } else {
        None
    }
}
