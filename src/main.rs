use salvo::prelude::*;
use zonemail::db::{Domain, InboundMessage, Mailbox, OutboundMessage};

use tracing::{error, info};
// Define a proper Salvo handler function
#[handler]
async fn hello(res: &mut Response) {
    res.render(Text::Plain("Zonemail API is running!"));
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Initialize Tracing subscriber for logging
    tracing_subscriber::fmt::init();
    info!("Starting Zonemail daemon...");

    // 2. Set up Salvo Web & API Router
    let router = Router::new().get(hello);

    // 3. Spawn Subsystems concurrently
    let api_server = async {
        let addr = "127.0.0.1:8080";
        info!("Salvo API listening on http://{}", addr);
        // Salvo v0.96 listener pattern: TcpListener::new(addr).bind().await
        let acceptor = TcpListener::new(addr).bind().await;
        Server::new(acceptor).serve(router).await;
    };

    let dns_server = async {
        info!("Hickory DNS server initializing on port 53...");
        // Use std::future::pending() instead of tokio::pending!()
        std::future::pending::<()>().await;
    };

    let mail_inbound = async {
        info!("Samotop SMTP inbound server initializing on port 25...");
        std::future::pending::<()>().await;
    };

    // Run everything together
    tokio::select! {
        _ = api_server => error!("Salvo API server stopped unexpectedly"),
        _ = dns_server => error!("DNS server stopped unexpectedly"),
        _ = mail_inbound => error!("Inbound Mail server stopped unexpectedly"),
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal, shutting down gracefully.");
        }
    }

    Ok(())
}
