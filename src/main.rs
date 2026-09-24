use tracing::{error, info};
use zonemail::config::Config;
use zonemail::runtime::Daemon;
use zonemail::send::{OutboundWorkerConfig, run_outbound_worker};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
      // 1. Initialize Tracing subscriber for logging
    tracing_subscriber::fmt::init();
    info!("Starting Zonemail daemon...");

       // 2. Load configuration (defaults + zonemail.toml + Env overrides)
    let cfg = Config::load()?;
    info!("Configuration loaded successfully. Database URL: {}", cfg.database_url);

       // 3. Build the control plane: database init + seed, and the service manager.
       //    No listener binds yet.
    let boot_mode = cfg.resolved_mode();
    let daemon = Daemon::build(cfg).await?;
    info!("Database initialized and seeded successfully.");

       // 4. Bring up the controllable listeners named by the boot mode. The API,
       //    inbound SMTP, and DNS are all services here; the mode decides which
       //    come up (`full` = all three, the historical default).
    info!("Booting services for mode {:?}", boot_mode);
    daemon.boot(boot_mode);

       // 5. The outbound delivery worker is *not* a service — it is a fire-and-forget
       //    poll loop that the runtime drives on its own, so it is composed here
       //    alongside the control plane rather than owned by it. It drains the
       //    in-database queue on a timer and runs until cancelled. A separate
       //    `AbortHandle` lets the shutdown path cancel it without moving the
       //    `JoinHandle` that the select awaits.
    let db = daemon.app().db.clone();
    let outbound = tokio::spawn(async move {
        run_outbound_worker(db, OutboundWorkerConfig::default()).await;
      });
    let outbound_abort = outbound.abort_handle();

       // 6. Run everything together: the outbound worker stopping on its own is
       //    reported, while a shutdown signal tears down the services and cancels
       //    the outbound worker.
    tokio::select! {
          _ = outbound => error!("Outbound delivery worker stopped unexpectedly"),
         _ = tokio::signal::ctrl_c() => {
             info!("Received shutdown signal, shutting down gracefully.");
            daemon.shutdown();
            outbound_abort.abort();
           }
       }

    Ok(())
}
