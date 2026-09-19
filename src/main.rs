use salvo::prelude::*;
use tracing::{error, info};
use zonemail::api::create_router;
use zonemail::app::AppState;
use zonemail::config::Config;
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

    // 2. Load configuration (defaults + Settings.toml + Env overrides)
    let cfg = Config::load()?;
    info!("Configuration loaded successfully.");
    info!("Database URL: {}", cfg.database_url);

    // 3. Optional: Run database connection and seed initial data

    let app_state = AppState::init(&cfg).await?;

    info!("Database initialized and seeded successfully.");

    // 4. Set up Salvo Web & API Router
    let router = create_router();
    let router = router.hoop(salvo::affix_state::inject(app_state.clone()));

    // 5. Build Server Address Bindings from Config
    let api_addr = cfg.api_addr()?;
    let dns_addr = cfg.dns_addr()?;
    let smtp_addr = cfg.smtp_addr()?;

    // 6. Define Subsystems concurrently
    let api_server = async move {
        info!("Salvo API listening on http://{}", api_addr);
        let acceptor = TcpListener::new(api_addr).bind().await;
        Server::new(acceptor).serve(router).await;
    };

    let dns_server = async move {
        info!("Hickory DNS server initializing on {}", dns_addr);
        std::future::pending::<()>().await;
    };

    let mail_inbound = async move {
        info!("Samotop SMTP inbound server initializing on {}", smtp_addr);
        std::future::pending::<()>().await;
    };

    // 7. Run everything together under Tokio Select
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
