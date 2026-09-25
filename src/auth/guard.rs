//! The [`AuthGuard`] handler, wired as a `hoop()` at the top of the API router so it
//! runs for every governed route *before* any route handler.
//!
//! It is a thin wrapper over the pure [`AuthService::enforce`] decision and the
//! RFC 7807 renderer [`write_rejection`], keeping the request-flow concern in one
//! place and leaving the decision/verification logic unit-testable.
//!
//! It holds only an `Arc<AuthService>` — it does **not** read `AppState` from the
//! depot, which keeps it decoupled from the router's `inject` ordering; it is
//! registered *after* the `inject(app)` / `inject(mgr)` hoops so those run first,
//! but it needs neither.
//!
//! On a [`Decision::Continue`] it injects the verified [`VerifiedClaims`] into the
//! request depot (type-keyed) **only when a token was present**, so downstream
//! handlers may enrich responses without every one having to re-decode.

use salvo::prelude::*;
// `Handler` / `async_trait` / `FlowCtrl` are the salvo request-flow machinery.
use salvo::{async_trait, FlowCtrl, Handler};
use std::sync::Arc;

use crate::auth::{write_rejection, AuthService, Decision};

/// A `hoop()` that authenticates every request and authorizes it against the
/// configured role grants.
///
/// - When `auth.enabled` is `false`, it returns immediately and does nothing
///   (zero behaviour/latency change — the API stays open exactly as today).
/// - When enabled, a request that is not authenticated is rejected `401`; an
///   authenticated request without a matching role is rejected `403`.
/// - The configured `public` resources (default `health`, `doc`) are always
///   allowed through.
#[derive(Clone)]
pub struct AuthGuard {
        /// The shared (possibly remote-refreshing) verifier.
    service: Arc<AuthService>,
}

impl AuthGuard {
     /// Construct from a shared verifier.
    pub fn new(service: Arc<AuthService>) -> Self {
        Self { service }
       }
      }

#[async_trait]
impl Handler for AuthGuard {
        /// Authenticate (if required) then authorize the caller's role set against
        /// this route, writing an RFC 7807 rejection + stopping the request when it
        /// must not proceed.
     async fn handle(
          &self, req: &mut Request, depot: &mut Depot, res: &mut Response, ctrl: &mut FlowCtrl,
         ) {
              // Disabled => the `enforce` is a pure pass-through (no token read, no render).
            match self.service.enforce(req).await {
                  // Public route or auth disabled: keep going. Inject claims only when
                   // a token was actually present, so the normal no-auth path allocates
                   // nothing extra.
                Decision::Continue(claims) => {
                     if let Some(c) = claims {
                            // Make the principal + roles available to downstream handlers.
                         #[allow(deprecated)]
                       depot.inject(c);
                       }
                    }
                    // Auth required but missing / invalid / unknown-algorithm.
                Decision::Unauthenticated(detail) => {
                    write_rejection(res, StatusCode::UNAUTHORIZED, detail);
                       // Stop the router so the goal (and any catch handler) can't re-render.
                     ctrl.skip_rest();
                     }
                    // Authenticated, but the caller's role set grants nothing on this route.
                Decision::Forbidden(detail) => {
                    write_rejection(res, StatusCode::FORBIDDEN, detail);
                       // Stop the router so the goal (and any catch handler) can't re-render.
                     ctrl.skip_rest();
                     }
                }
          }
     }
