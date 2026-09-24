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
