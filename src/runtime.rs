//! The embeddable control plane for a Zonemail deployment.
//!
//! A [`Daemon`] owns the shared application state (database + config) and the
//! controllable-listener [`ServiceManager`]. It is the seam an *embedding*
//! project hooks into: build it inside your own runtime, then decide which
//! listeners come up and whether to host the API at all.
//!
//! ```ignore
//! # use zonemail::config::Config;
//! # use zonemail::runtime::Daemon;
//! # use zonemail::send::{OutboundWorkerConfig, run_outbound_worker};
//! # tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async {
//! // (A) Full daemon: API + inbound SMTP + DNS on their own ports.
//! let daemon = Daemon::build(Config::default()).await.unwrap();
//! daemon.boot(Default::default());
//! let db = daemon.app().db.clone();
//! tokio::spawn(async move { run_outbound_worker(db, OutboundWorkerConfig::default()).await });
//!
//! // (B) Embed: no API listener — mount the routes into your own Salvo router.
//! let host = your_router().hoop(daemon.routes());
//! daemon.boot(zonemail::services::BootMode::Smtp); // inbound only, API off
//! ```
use salvo::prelude::*;
use salvo::affix_state;
use std::sync::Arc;

use crate::api::api_with_doc;
use crate::app::AppState;
use crate::config::Config;
use crate::services::{BootMode, ServiceManager};

/// A control plane over the optional listeners, plus the shared state every
/// listener and the outbound worker need. The two axes are deliberately split:
/// the *controllable services* live in the [`ServiceManager`]; the *outbound
/// delivery worker* is not a service and so is **not** owned here — the caller
/// spawns `run_outbound_worker` (with `daemon.app().db.clone()`) and drives its
/// own lifecycle.
pub struct Daemon {
    cfg: Config,
    app: Arc<AppState>,
    services: Arc<ServiceManager>,
}

impl Daemon {
    /// Init the database + shared state, then construct the service manager.
     /// Binds *no* listener yet — call [`boot`](Self::boot) (or start
     /// services individually via [`service_manager`](Self::service_manager))
     /// to bring listeners up.
    pub async fn build(cfg: Config) -> Result<Self, Box<dyn std::error::Error>> {
        let app = Arc::new(AppState::init(&cfg).await?);
        let services = ServiceManager::new(&cfg, app.clone());
        Ok(Self { cfg, app, services })
     }

     /// Start the controllable listeners named by `mode` (the API is one of them).
     /// This is how an embedder opts the API in or out: a `mode` without `Api`
     /// leaves that listener down.
    pub fn boot(&self, mode: BootMode) {
        self.services.boot(mode);
     }

     /// A Salvo router wired with this daemon's state, for mounting into a host
     /// application when the API *listener* itself is not wanted (`BootMode::Off`,
     /// or any mode that omits `Api`). No port is bound by this call.
     pub fn routes(&self) -> Router {
        api_with_doc()
             .hoop(affix_state::inject((*self.app).clone()))
             .hoop(affix_state::inject(self.services.clone()))
             .hoop(crate::auth::AuthGuard::new(self.app.auth.clone()))
     }

     /// The controllable service manager — start/stop individual services, query
     /// status, apply modes. Cheap to clone (an `Arc`).
    pub fn service_manager(&self) -> Arc<ServiceManager> {
        self.services.clone()
     }

     /// The shared application state; most usefully `.db` to seed the outbound
     /// delivery worker the caller owns.
    pub fn app(&self) -> &AppState {
        &self.app
     }

     /// The config this daemon was built from.
    pub fn config(&self) -> &Config {
        &self.cfg
     }

     /// Stop every controllable listener. The outbound worker is intentionally
     /// *not* touched — cancelling it is the caller's responsibility, since it is
     /// not a service of this plane.
    pub fn shutdown(&self) {
        self.services.stop_all();
     }
}
