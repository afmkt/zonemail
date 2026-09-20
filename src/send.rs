use hickory_resolver::TokioAsyncResolver;
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::{AsyncTransport, Message, Tokio1Executor};

pub async fn send_direct_email(
    recipient: &str,
    message: Message,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Extract the domain from the recipient's email address
    let domain = recipient
        .split('@')
        .last()
        .ok_or("Invalid email address format")?;

    // 2. Initialize the async DNS resolver using system configuration
    let resolver = TokioAsyncResolver::tokio_from_system_conf()?;

    // 3. Query MX records for the domain
    let response = resolver.mx_lookup(domain).await?;
    let mut mx_records: Vec<_> = response.iter().collect();

    if mx_records.is_empty() {
        return Err(format!("No MX records found for domain: {}", domain).into());
    }

    // 4. Sort MX records by preference (lower number = higher priority)
    mx_records.sort_by_key(|mx| mx.preference());

    // 5. Try delivering to the MX servers in order of priority
    let mut last_error = None;
    for mx in mx_records {
        let mx_host = mx.exchange().to_string();
        let mx_host = mx_host.trim_end_matches('.');

        println!(
            "Trying MX host: {} (Preference: {})",
            mx_host,
            mx.preference()
        );

        // Configure an async SMTP transport targeting Port 25 of the destination MX
        let mailer: AsyncSmtpTransport<Tokio1Executor> =
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(mx_host)
                .port(25)
                .build();

        // Attempt transmission
        match mailer.send(message.clone()).await {
            Ok(_) => {
                println!("Successfully delivered email to {}", mx_host);
                return Ok(());
            }
            Err(e) => {
                eprintln!("Failed to connect/deliver to {}: {}", mx_host, e);
                last_error = Some(e);
            }
        }
    }

    Err(last_error
        .map(|e| e.into())
        .unwrap_or_else(|| "All MX delivery attempts failed".into()))
}
