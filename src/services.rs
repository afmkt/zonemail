//! Optional server lifecycle: the SMTP and DNS listeners can be started and
//! stopped, individually or as a named mode, both from config (at boot) and from
//! the HTTP API (at runtime). The API and the outbound delivery worker are
//! always on and are *not* controllable here.
//!
//! ## Design
//!
//! Each controllable service ([`Service`]) is driven by a [`ServiceManager`], a
//! lock-guarded control plane that owns the running task's [`tokio::task::JoinHandle`].
//! A "stop" is a `JoinHandle::abort()`: the task and its bound socket are dropped
//! together, which releases the port. `smtpd` exposes no clean shutdown hook (its
//! listener loop is a bare `loop { accept }`), so abort-on-drop is the practical
//! mechanism — the socket is freed deterministically when the task is dropped.
//!
//! The manager spawns servers through a pluggable [`SpawnFn`] so tests can inject
//! a fake that never binds a real port. Production uses [`ServiceManager::new`],
//! which spawns `run_dns_server` / `run_mail_server`; tests use `for_test`.
use crate::app::AppState;
use crate::config::Config;
use crate::dns::run_dns_server;
use crate::email::run_mail_server;
use salvo::oapi::ToSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tracing::warn;

// ===========================================================================
// Service identity and status
// ===========================================================================

/// A controllable optional server. The API and the outbound worker are *not*
/// members: they are always on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Service {
    /// The DNS (`:53` by default) listener.
    Dns,
    /// The inbound SMTP (`:25` by default) listener.
    Smtp,
}

impl Service {
    /// Every controllable service, in a stable order.
    pub fn all() -> [Service; 2] {
        [Service::Dns, Service::Smtp]
     }

    /// Parse a service name from the wire (path param / API body), case-insensitive.
    pub fn parse(s: &str) -> Option<Service> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dns" => Some(Service::Dns),
            "smtp" => Some(Service::Smtp),
            _ => None,
         }
     }

    /// The human-facing name used in messages and responses.
    pub fn as_str(&self) -> &'static str {
        match self {
            Service::Dns => "dns",
            Service::Smtp => "smtp",
         }
     }
}

/// Whether a service is currently listening. Purely a control-plane intent:
/// `Running` means a task was spawned and not yet stopped; a task that dies on its
/// own is logged by the task itself but is not reflected in this field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// No listener task is running for this service.
    Idle,
    /// A listener task is running.
    Running,
}

/// The set of services to bring up at boot. A named *preset* that resolves to
/// per-service on/off flags — the manager's real state is always the individual
/// service statuses, and `mode` is only the shortcut used at startup (and by
/// `POST /services/mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BootMode {
    /// Both SMTP and DNS. (Default — preserves the historical "everything on".)
    #[default]
    Full,
    /// SMTP only.
    Smtp,
    /// DNS only.
    Dns,
    /// Neither optional server (API + outbound worker only).
    ApiOnly,
}

impl BootMode {
    /// The services this mode brings up.
    pub fn services(&self) -> HashSet<Service> {
        let mut set = HashSet::new();
        match self {
            BootMode::Full => {
                set.insert(Service::Smtp);
                set.insert(Service::Dns);
            }
            BootMode::Smtp => {
                set.insert(Service::Smtp);
              }
            BootMode::Dns => {
                set.insert(Service::Dns);
              }
            BootMode::ApiOnly => {}
        }
        set
    }
}

// ===========================================================================
// Result / report DTOs
// ===========================================================================

/// The status of one service, returned by `GET /services`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ServiceReport {
    pub service: Service,
    pub status: Status,
}

/// The outcome of a start/stop command. `changed` is `false` when the command was
/// a no-op (start of an already-running service, stop of an already-idle one), so
/// callers can tell idempotent repetition from a real transition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ServiceResult {
    pub service: Service,
    pub status: Status,
    pub changed: bool,
}

// ===========================================================================
// Service manager
// ===========================================================================

/// The spawn seam. Returns an already-spawned task for the given service; the
/// production implementation wraps the real SMTP/DNS runners, tests wrap a
/// fake. Kept *synchronous* so the manager never awaits while holding its lock.
type SpawnFn = dyn Fn(Service, SocketAddr) -> Result<JoinHandle<()>, String> + Send + Sync;

struct Slot {
    status: Status,
    handle: Option<JoinHandle<()>>,
}

struct Inner {
    dns: Slot,
    smtp: Slot,
    dns_addr: SocketAddr,
    smtp_addr: SocketAddr,
}

/// A lock-guarded control plane over the optional SMTP/DNS listeners.
///
/// Shares a single [`Mutex`] so concurrent start/stop requests are serialized and
/// can never double-spawn a service. Cloning is cheap (`Arc`); the clone reaches
/// the same underlying state.
#[derive(Clone)]
pub struct ServiceManager {
    inner: Arc<Mutex<Inner>>,
    spawn: Arc<SpawnFn>,
}

impl Inner {
    /// The slot for a service, mutably.
    fn slot_mut(&mut self, svc: Service) -> &mut Slot {
        match svc {
            Service::Dns => &mut self.dns,
            Service::Smtp => &mut self.smtp,
         }
     }

    /// The bind address a service listens on, from config.
    fn addr(&self, svc: Service) -> SocketAddr {
        match svc {
            Service::Dns => self.dns_addr,
            Service::Smtp => self.smtp_addr,
         }
     }

    fn status_of(&self, svc: Service) -> Status {
        match svc {
            Service::Dns => self.dns.status,
            Service::Smtp => self.smtp.status,
         }
     }
}

impl ServiceManager {
    /// Production manager: spawns the real `run_dns_server` / `run_mail_server`
    /// for each service, bound to the addresses in `cfg`. Does not spawn anything
    /// at construction time — boot the services with [`ServiceManager::boot`].
    pub fn new(cfg: &Config, app: Arc<AppState>) -> Self {
        let dns_addr = cfg
             .dns_addr()
             .expect("config dns address must be valid; validated at load");
        let smtp_addr = cfg
             .smtp_addr()
             .expect("config smtp address must be valid; validated at load");

        let spawn: Arc<SpawnFn> = Arc::new(move |svc: Service, addr: SocketAddr| {
            let app = app.clone();
            let handle = match svc {
                Service::Dns => tokio::spawn(async move {
                    if let Err(e) = run_dns_server(addr, app).await {
                        warn!("DNS server stopped: {e}");
                    }
                }),
                Service::Smtp => tokio::spawn(async move {
                    if let Err(e) = run_mail_server(addr, app).await {
                        warn!("SMTP server stopped: {e}");
                    }
                }),
            };
            Ok(handle)
         });

        ServiceManager {
            inner: Arc::new(Mutex::new(Inner {
                dns: Slot { status: Status::Idle, handle: None },
                smtp: Slot { status: Status::Idle, handle: None },
                dns_addr,
                smtp_addr,
             })),
            spawn,
         }
     }

    /// A test manager: a fake spawner that creates a task which lives until it is
    /// aborted (so no real port is ever bound). Bind addresses are inert.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test() -> Self {
        let spawn: Arc<SpawnFn> =
             Arc::new(|_svc: Service, _addr: SocketAddr| Ok(tokio::spawn(std::future::pending::<()>())));
        ServiceManager {
            inner: Arc::new(Mutex::new(Inner {
                dns: Slot { status: Status::Idle, handle: None },
                smtp: Slot { status: Status::Idle, handle: None },
                dns_addr: "127.0.0.1:0".parse().unwrap(),
                smtp_addr: "127.0.0.1:0".parse().unwrap(),
             })),
            spawn,
         }
     }

    /// Start one service. Idempotent: starting an already-running service is a
    /// no-op that reports `changed: false`. A bind/spawn failure surfaces as
    /// `Err` and leaves the service idle.
    pub fn start(&self, svc: Service) -> Result<ServiceResult, String> {
        let mut guard = self.inner.lock().expect("service manager poisoned");
        let addr = guard.addr(svc);
        let slot = guard.slot_mut(svc);

        if matches!(slot.status, Status::Running) {
            return Ok(ServiceResult { service: svc, status: Status::Running, changed: false });
         }

        let handle = (self.spawn)(svc, addr)?;
        slot.handle = Some(handle);
        slot.status = Status::Running;
        Ok(ServiceResult { service: svc, status: Status::Running, changed: true })
     }

    /// Stop one service. Idempotent: stopping an already-idle service is a no-op
    /// that reports `changed: false`. Aborts the task, which drops its bound
    /// socket and releases the port.
    pub fn stop(&self, svc: Service) -> Result<ServiceResult, String> {
        let mut guard = self.inner.lock().expect("service manager poisoned");
        let slot = guard.slot_mut(svc);

        if let Some(handle) = slot.handle.take() {
            handle.abort();
            slot.status = Status::Idle;
            return Ok(ServiceResult { service: svc, status: Status::Idle, changed: true });
         }

        Ok(ServiceResult { service: svc, status: Status::Idle, changed: false })
     }

    /// The current status of one service.
    pub fn status_of(&self, svc: Service) -> Status {
        self.inner.lock().expect("service manager poisoned").status_of(svc)
     }

    /// A status report for every controllable service, in a stable order.
    pub fn list(&self) -> Vec<ServiceReport> {
        let guard = self.inner.lock().expect("service manager poisoned");
        Service::all()
             .into_iter()
             .map(|svc| ServiceReport { service: svc, status: guard.status_of(svc) })
             .collect()
     }

    /// Bring up a named mode: start every service the mode wants, stop the rest.
    /// Returns the resulting list of statuses.
    pub fn apply_mode(&self, mode: BootMode) -> Vec<ServiceReport> {
        let wanted = mode.services();
        for svc in Service::all() {
            if wanted.contains(&svc) {
                if let Err(e) = self.start(svc) {
                    warn!("Failed to start {svc:?} for mode {mode:?}: {e}");
                }
             } else {
                let _ = self.stop(svc);
             }
         }
        self.list()
     }

    /// Boot the services named by a mode (used at startup). Best-effort: a service
    /// that fails to spawn is logged but does not prevent the others from starting.
    pub fn boot(&self, mode: BootMode) {
        for svc in mode.services() {
            if let Err(e) = self.start(svc) {
                warn!("Failed to boot {svc:?} from mode {mode:?}: {e}");
            }
         }
     }

    /// Stop every controllable service.
    pub fn stop_all(&self) {
        for svc in Service::all() {
            let _ = self.stop(svc);
         }
     }
}

// ===========================================================================
// Tests
// ===========================================================================
//
// Exercises the control-plane logic through a fake spawner (`for_test`), so no
// real socket is ever bound — start/stop/apply_mode transitions are deterministic
// and parallel-safe.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_mode_services_map_correctly() {
        assert_eq!(BootMode::Full.services(), HashSet::from([Service::Smtp, Service::Dns]));
        assert_eq!(BootMode::Smtp.services(), HashSet::from([Service::Smtp]));
        assert_eq!(BootMode::Dns.services(), HashSet::from([Service::Dns]));
        assert!(BootMode::ApiOnly.services().is_empty());
        assert_eq!(BootMode::default(), BootMode::Full);
     }

    #[test]
    fn service_parse_is_case_insensitive_and_rejects_unknown() {
        assert_eq!(Service::parse("dns"), Some(Service::Dns));
        assert_eq!(Service::parse(" SMTP "), Some(Service::Smtp));
        assert_eq!(Service::parse("api"), None);
        assert_eq!(Service::parse("bogus"), None);
     }

    #[tokio::test]
    async fn start_is_idempotent_and_reflects_status() {
        let mgr = ServiceManager::for_test();
        assert_eq!(mgr.status_of(Service::Smtp), Status::Idle);

        let first = mgr.start(Service::Smtp).unwrap();
        assert!(first.changed);
        assert_eq!(first.status, Status::Running);
        assert_eq!(mgr.status_of(Service::Smtp), Status::Running);

        // Second start is a no-op; the service stays running.
        let second = mgr.start(Service::Smtp).unwrap();
        assert!(!second.changed);
        assert_eq!(mgr.status_of(Service::Smtp), Status::Running);
     }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let mgr = ServiceManager::for_test();
        mgr.start(Service::Dns).unwrap();

        let first = mgr.stop(Service::Dns).unwrap();
        assert!(first.changed);
        assert_eq!(first.status, Status::Idle);
        assert_eq!(mgr.status_of(Service::Dns), Status::Idle);

        // Stopping an idle service is a no-op.
        let second = mgr.stop(Service::Dns).unwrap();
        assert!(!second.changed);
     }

    #[tokio::test]
    async fn apply_mode_starts_wanted_and_stops_rest() {
        let mgr = ServiceManager::for_test();
        mgr.boot(BootMode::Full);
        assert_eq!(mgr.status_of(Service::Smtp), Status::Running);
        assert_eq!(mgr.status_of(Service::Dns), Status::Running);

        let reports = mgr.apply_mode(BootMode::Dns);
        assert_eq!(mgr.status_of(Service::Smtp), Status::Idle);
        assert_eq!(mgr.status_of(Service::Dns), Status::Running);
        assert_eq!(reports.len(), 2);
     }

    #[tokio::test]
    async fn list_reports_both_services() {
        let mgr = ServiceManager::for_test();
        mgr.start(Service::Smtp).unwrap();
        let reports = mgr.list();
        assert_eq!(reports.len(), 2);
        assert!(reports.iter().any(|r| r.service == Service::Smtp && r.status == Status::Running));
        assert!(reports.iter().any(|r| r.service == Service::Dns && r.status == Status::Idle));
     }
}
