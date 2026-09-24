use salvo::prelude::*;
use std::sync::Arc;
use tracing::{error, info};
use zonemail::api::api_with_doc;
use zonemail::app::AppState;
use zonemail::config::Config;
use zonemail::send::{OutboundWorkerConfig, run_outbound_worker};
use zonemail::services::ServiceManager;
// Define a proper Salvo handler function

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
     // 1. Initialize Tracing subscriber for logging
    tracing_subscriber::fmt::init();
    info!("Starting Zonemail daemon...");

     // 2. Load configuration (defaults + zonemail.toml + Env overrides)
    let cfg = Config::load()?;
    info!("Configuration loaded successfully.");
    info!("Database URL: {}", cfg.database_url);

     // 3. Optional: Run database connection and seed initial data

    let app_state = AppState::init(&cfg).await?;
    let shared_state = Arc::new(app_state.clone());

    info!("Database initialized and seeded successfully.");

     // 4. Build the service control plane and bring up whatever the configured
     // boot mode names. The API and the outbound worker are always on; only the
     // SMTP and DNS listeners are controllable.
    let services = Arc::new(ServiceManager::new(&cfg, shared_state.clone()));
    info!("Booting services for mode {:?}", cfg.resolved_mode());
    services.boot(cfg.resolved_mode());

     // 5. Set up Salvo Web & API Router
     //
     // The router must be injected with `AppState` **by value**: handlers retrieve
     // it via `depot.get_typed_mut::<AppState>()`, and affix-state keys the depot by
     // the exact `TypeId` of the injected value. Injecting `Arc<AppState>` instead
     // would key it as `Arc<AppState>` and every handler's retrieval would fail with
     // "AppState not found". We inject the original by value and the service manager
     // (an `Arc`, retrieved by its own `TypeId`) as a second depot value so the
     // `/services` control handlers reach the same control plane as `main`.
    let router = api_with_doc();
    let router = router.hoop(salvo::affix_state::inject(app_state));
    let router = router.hoop(salvo::affix_state::inject(services.clone()));

     // 6. Build Server Address Bindings from Config
    let api_addr = cfg.api_addr()?;

     // 7. Define Subsystems concurrently
    let api_server = async move {
         info!("Salvo API listening on http://{}", api_addr);
        let acceptor = TcpListener::new(api_addr).bind().await;
        Server::new(acceptor).serve(router).await;
      };

     // The outbound delivery worker is a fire-and-forget poll loop that runs
     // until the surrounding `select!` is cancelled; it drains the in-database
     // queue on a timer and never returns under normal operation. It is *not* a
     // listener, so it is always on and not part of the controllable services.
    let outbound_worker = {
         let db = shared_state.db.clone();
        async move {
            run_outbound_worker(db, OutboundWorkerConfig::default()).await;
          }
      };

     // 8. Run everything together. A single service no longer tears the daemon
     // down: the controllable listeners are owned by the service manager, and on
     // a shutdown signal we stop them all before returning.
    tokio::select! {
         _ = api_server => error!("Salvo API server stopped unexpectedly"),
         _ = outbound_worker => error!("Outbound delivery worker stopped unexpectedly"),
         _ = tokio::signal::ctrl_c() => {
             info!("Received shutdown signal, shutting down gracefully.");
             services.stop_all();
         }
      }

    Ok(())
}
