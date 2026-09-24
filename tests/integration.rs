//! Cross-module, black-box integration tests.
//!
//! These exercise `zonemail` purely through its **public** API: each test spins
//! up a fresh, fully-isolated in-memory Turso database via
//! [`AppState::connect_in_memory`], builds the real router with
//! [`create_router`](zonemail::api::create_router), injects the state by value
//! (the handlers retrieve it with `depot.get_typed_mut::<AppState>()` and
//! affix-state keys the depot by the exact `TypeId` of the injected value, so
//! the two must agree — `AppState`, not `Arc<AppState>`), and drives real
//! requests through `salvo::test::TestClient`. No sockets, no on-disk state, no
//! external services, and the tests are parallel-safe by construction.

use salvo::affix_state;
use salvo::prelude::*;
use salvo::test::{ResponseExt, TestClient};

use zonemail::api::{ApiResponse, create_router, MessageDTO, SendResult};
use zonemail::app::AppState;
use zonemail::services::{Service, ServiceManager, Status};

/// A freshly-built API service, each with its own in-memory database.
async fn api_service() -> salvo::Service {
     let state = AppState::connect_in_memory().await.expect("in-memory state");
     let router = create_router().hoop(affix_state::inject(state));
     salvo::Service::new(router)
}

/// Provision a single mailbox address so the send path clears the
/// "sender must be provisioned" gate. `create_mailbox` accepts a bare
/// `local@domain` id and provisions the domain and mailbox together.
async fn provision_mailbox(service: &salvo::Service, address: &str) {
     let res = TestClient::post("http://localhost/mailboxes")
       .json(&serde_json::json!({"id": address}))
       .send(service)
       .await;
     assert_eq!(res.status_code, Some(StatusCode::OK), "mailbox provisioned");
}

   #[tokio::test]
async fn domain_crud_roundtrip_through_public_router() {
     let service = api_service().await;

       // Create -> Get -> delete -> 404, all through the public router.
     let res = TestClient::post("http://localhost/domains")
       .json(&serde_json::json!({"id": "example.com"}))
       .send(&service)
       .await;
     assert_eq!(res.status_code, Some(StatusCode::OK), "create");

       // A duplicate create 409s.
     let res = TestClient::post("http://localhost/domains")
       .json(&serde_json::json!({"id": "example.com"}))
       .send(&service)
       .await;
     assert_eq!(res.status_code, Some(StatusCode::CONFLICT), "duplicate domain");

       // Delete succeeds, then the domain is gone.
     let res =
        TestClient::delete("http://localhost/domains/example.com").send(&service).await;
     assert_eq!(res.status_code, Some(StatusCode::OK), "delete");

     let res = TestClient::get("http://localhost/domains/example.com").send(&service).await;
     assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND), "absent after delete");
}

   #[tokio::test]
async fn sending_mail_enqueues_jobs_and_stores_the_message() {
     let service = api_service().await;
     provision_mailbox(&service, "alice@example.com").await;

       // One message, two recipient jobs (to + cc), enqueued synchronously.
     let mut res = TestClient::post("http://localhost/mail")
       .json(&serde_json::json!({
            "from": "alice@example.com",
            "to": ["bob@ext.com"],
            "cc": ["carol@ext.com"],
            "subject": "Integration",
            "body": "hello world"
        }))
       .send(&service)
       .await;
     assert_eq!(res.status_code, Some(StatusCode::OK), "send");

     let result: ApiResponse<SendResult> =
        res.take_json().await.expect("json body");
     assert!(result.ok, "send reported ok");
     assert_eq!(result.data.outbound_ids.len(), 2, "one job per recipient");

       // The message is then addressable by its id through the public router.
     let mut res =
        TestClient::get(&format!("http://localhost/messages/{}", result.data.message_id))
         .send(&service)
         .await;
     assert_eq!(res.status_code, Some(StatusCode::OK), "fetch message");
     let msg: ApiResponse<MessageDTO> = res.take_json().await.expect("json body");
     assert_eq!(msg.data.mail_from, "alice@example.com", "stored sender");
     assert_eq!(msg.data.subject.as_deref(), Some("Integration"), "stored subject");
}

   #[tokio::test]
async fn send_rejects_an_unprovisioned_sender() {
     let service = api_service().await;

       // `ghost@example.com` was never provisioned: the send is refused at the
       // preflight gate even though the recipient is well-formed.
     let res = TestClient::post("http://localhost/mail")
       .json(&serde_json::json!({
            "from": "ghost@example.com",
            "to": ["bob@ext.com"],
            "body": "never sent"
        }))
       .send(&service)
       .await;
     assert_eq!(res.status_code, Some(StatusCode::CONFLICT), "unprovisioned sender");
}

   #[tokio::test]
async fn send_validates_recipients() {
     let service = api_service().await;
     provision_mailbox(&service, "alice@example.com").await;

       // A list with no recipients is a client error.
     let res = TestClient::post("http://localhost/mail")
       .json(&serde_json::json!({"from": "alice@example.com", "to": [], "body": "x"}))
       .send(&service)
       .await;
     assert_eq!(res.status_code, Some(StatusCode::BAD_REQUEST), "empty recipients");

       // A syntactically broken address is a client error.
     let res = TestClient::post("http://localhost/mail")
       .json(&serde_json::json!({
            "from": "alice@example.com",
            "to": ["not-an-email"],
            "body": "x"
        }))
       .send(&service)
       .await;
     assert_eq!(res.status_code, Some(StatusCode::BAD_REQUEST), "malformed recipient");
}

   #[tokio::test]
async fn health_probe_and_unmatched_route() {
     let service = api_service().await;

       // The readiness probe reports healthy.
     let res = TestClient::get("http://localhost/health").send(&service).await;
     assert_eq!(res.status_code, Some(StatusCode::OK), "health");

       // An unregistered path surfaces as 404, proving the probe is not a
       // catch-all that swallows unmatched routes.
     let res = TestClient::get("http://localhost/does-not-exist").send(&service).await;
     assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND), "404 for unknown route");
}

// ---------------------------------------------------------------------------
// Service control (`/services/...`). These drive the real router with a
// `ServiceManager` built from `for_test()` — a spawner that never opens a real
// port — so the lifecycle endpoints are exercised end-to-end without needing
// root for :25/:53.
//
// Note the second injected state: the control handlers retrieve `Arc<ServiceManager>`
// by *reference*, so `get_typed_mut::<Arc<_>>` yields it as-is and the handler clones
// out the cheap `Arc`. Both `AppState` and `Arc<ServiceManager>` live in one depot
// (keyed by `TypeId`) exactly as production wires them in `main`.

/// An API service that also carries a test `ServiceManager` (no real sockets)
/// alongside the usual in-memory `AppState`.
async fn service_api_service() -> salvo::Service {
     let mgr = std::sync::Arc::new(ServiceManager::for_test());
     let state = AppState::connect_in_memory().await.expect("in-memory state");
     let router = create_router()
        .hoop(affix_state::inject(state))
        .hoop(affix_state::inject(mgr));
     salvo::Service::new(router)
}

    #[tokio::test]
async fn services_list_reports_both_listeners_idle_by_default() {
     let service = service_api_service().await;

     let mut res = TestClient::get("http://localhost/services").send(&service).await;
     assert_eq!(res.status_code, Some(StatusCode::OK), "list");
     let body: ApiResponse<Vec<zonemail::services::ServiceReport>> =
        res.take_json().await.expect("json body");
     assert!(body.ok, "list ok");
     assert_eq!(body.data.len(), 2, "one report per controllable service");
     assert!(body.data.iter().all(|r| r.status == Status::Idle), "both idle at boot");
     let has_dns = body.data.iter().any(|r| r.service == Service::Dns);
     let has_smtp = body.data.iter().any(|r| r.service == Service::Smtp);
     assert!(has_dns, "a report for dns");
     assert!(has_smtp, "a report for smtp");
}

    #[tokio::test]
async fn service_start_stop_is_idempotent_through_router() {
     let service = service_api_service().await;

        // First start is a real change.
     let mut res = TestClient::post("http://localhost/services/smtp/start").send(&service).await;
     assert_eq!(res.status_code, Some(StatusCode::OK), "start");
     let r: ApiResponse<zonemail::services::ServiceResult> =
        res.take_json().await.expect("json body");
     assert!(r.data.changed, "first start");
     assert_eq!(r.data.status, Status::Running);
     assert_eq!(r.data.service, Service::Smtp);

        // The second start is a no-op.
     let mut res = TestClient::post("http://localhost/services/smtp/start").send(&service).await;
     assert_eq!(res.status_code, Some(StatusCode::OK), "start again");
     let r: ApiResponse<zonemail::services::ServiceResult> =
        res.take_json().await.expect("json body");
     assert!(!r.data.changed, "idempotent start");

        // Stopping is a real change, then a no-op.
     let mut res = TestClient::post("http://localhost/services/smtp/stop").send(&service).await;
     assert_eq!(res.status_code, Some(StatusCode::OK), "stop");
     let r: ApiResponse<zonemail::services::ServiceResult> =
        res.take_json().await.expect("json body");
     assert!(r.data.changed, "first stop");
     assert_eq!(r.data.status, Status::Idle);

     let mut res = TestClient::post("http://localhost/services/smtp/stop").send(&service).await;
     assert_eq!(res.status_code, Some(StatusCode::OK), "stop again");
     let r: ApiResponse<zonemail::services::ServiceResult> =
        res.take_json().await.expect("json body");
     assert!(!r.data.changed, "idempotent stop");
}

    #[tokio::test]
async fn service_mode_switches_all_listeners() {
     let service = service_api_service().await;

        // DNS-only shuts anything else down; here SMTP must be Idle and DNS Running.
     let mut res =
        TestClient::post("http://localhost/services/mode")
          .json(&serde_json::json!({ "mode": "dns" }))
          .send(&service)
          .await;
     assert_eq!(res.status_code, Some(StatusCode::OK), "set mode");
     let body: ApiResponse<Vec<zonemail::services::ServiceReport>> =
        res.take_json().await.expect("json body");
     let dns = body.data.iter().find(|r| r.service == Service::Dns).unwrap();
     let smtp = body.data.iter().find(|r| r.service == Service::Smtp).unwrap();
     assert_eq!(dns.status, Status::Running, "dns up for dns-mode");
     assert_eq!(smtp.status, Status::Idle, "smtp down for dns-mode");

        // Full brings both up.
     let mut res =
        TestClient::post("http://localhost/services/mode")
          .json(&serde_json::json!({ "mode": "full" }))
          .send(&service)
          .await;
     assert_eq!(res.status_code, Some(StatusCode::OK), "set full mode");
     let body: ApiResponse<Vec<zonemail::services::ServiceReport>> =
        res.take_json().await.expect("json body");
     assert!(
        !body.data.is_empty() && body.data.iter().all(|r| r.status == Status::Running),
        "both running after full mode"
     );

        // An unknown mode is a client error.
     let res =
        TestClient::post("http://localhost/services/mode")
          .json(&serde_json::json!({ "mode": "banana" }))
          .send(&service)
          .await;
     assert_eq!(res.status_code, Some(StatusCode::BAD_REQUEST), "unknown mode");
}

    #[tokio::test]
async fn service_control_rejects_non_controllable_names() {
     let service = service_api_service().await;

        // The API worker and outbound consumer are always-on and have no control
        // route: a lifecycle path for them 400s instead of silently no-oping.
     for name in ["api", "outbound"] {
        let res =
             TestClient::post(format!("http://localhost/services/{name}/start"))
               .send(&service)
               .await;
        assert_eq!(
             res.status_code,
             Some(StatusCode::BAD_REQUEST),
             "{name} is not controllable"
        );
     }
}
