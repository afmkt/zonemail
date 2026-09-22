//! HTTP API surface for Zonemail.
//!
//! This module defines the *shape* of the REST API: request/response DTOs, the
//! handler signatures, the status-code + envelope conventions, and the route
//! wiring. The per-handler business logic (DB reads/writes, SMTP handoff, etc.)
//! is intentionally left as `todo!()` — only the surface is implemented here.
//!
//! The response envelope types ([`ApiProblem`], [`ApiResponse`], [`Page`]) are
//! borrowed verbatim from the `janux` project so the two services speak the same
//! wire contract.
//!
//! ## Resources
#![allow(clippy::missing_errors_description)]
// Shape-only skeleton: business logic is left unimplemented, so extracted inputs
// and not-yet-fetched views are not always consumed. Silence the resulting noise.
#![allow(unused_variables, unreachable_code)]
//!
//! * **Domain** — `GET/POST /domains`, `GET/PATCH/DELETE /domains/{domain_id}`
//! * **Mailbox (email address)** — `GET/POST /mailboxes`,
//!   `GET/PATCH/DELETE /mailboxes/{mailbox_id}`
//! * **Send email** — `POST /mail`
//! * **Messages of an email address (send or receive)** —
//!   `GET /mailboxes/{mailbox_id}/messages`,
//!   `GET /mailboxes/{mailbox_id}/messages/inbox`,
//!   `GET /mailboxes/{mailbox_id}/messages/sent`,
//!   `GET/DELETE /messages/{message_id}`
//!
//! All list endpoints accept `limit`/`offset` query params and return a
//! [`Page`] inside an [`ApiResponse`]. All endpoints return [`ApiProblem`] on
//! failure and [`ApiResponse`] on success.
use salvo::prelude::*;
use serde::{Deserialize, Serialize};

// ===========================================================================
// Shared return data types (borrowed from `janux`)
// ===========================================================================

/// RFC-7807-ish problem document rendered on every error response.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ApiProblem {
    pub status: u16,
    #[serde(rename = "type")]
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ApiProblem {
    pub fn bad_request(msg: &str) -> Self {
        ApiProblem {
            status: StatusCode::BAD_REQUEST.as_u16(),
            r#type: "bad request".into(),
            detail: Some(msg.into()),
        }
    }
    pub fn not_found(msg: &str) -> Self {
        ApiProblem {
            status: StatusCode::NOT_FOUND.as_u16(),
            r#type: "not_found".into(),
            detail: Some(msg.into()),
        }
    }
    pub fn validation_error(detail: &str) -> Self {
        ApiProblem {
            status: StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
            r#type: "validation_error".into(),
            detail: Some(detail.into()),
        }
    }
    pub fn unauthorized() -> Self {
        ApiProblem {
            status: StatusCode::UNAUTHORIZED.as_u16(),
            r#type: "unauthorized".into(),
            detail: None,
        }
    }
    pub fn forbidden() -> Self {
        ApiProblem {
            status: StatusCode::FORBIDDEN.as_u16(),
            r#type: "forbidden".into(),
            detail: None,
        }
    }
    pub fn conflict(msg: &str) -> Self {
        ApiProblem {
            status: StatusCode::CONFLICT.as_u16(),
            r#type: "conflict".into(),
            detail: Some(msg.into()),
        }
    }
    pub fn server_error(msg: &str) -> Self {
        ApiProblem {
            status: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
            r#type: "server_error".into(),
            detail: Some(msg.into()),
        }
    }
}

/// Uniform success envelope: `{ "ok": true, "data": <T> }`.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ApiResponse<T> {
    pub ok: bool,
    pub data: T,
}

impl<T> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        ApiResponse { ok: true, data }
    }
}

/// Default page size for paginated list endpoints.
pub const DEFAULT_PAGE_LIMIT: usize = 50;

/// Hard cap on page size.
pub const MAX_PAGE_LIMIT: usize = 200;

/// Pagination envelope for list endpoints. `next_offset` is `Some` only when
/// more rows follow this page, so clients can loop without a total count.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub limit: usize,
    pub offset: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
}

impl<T> Page<T> {
    /// Build a page from an already-loaded in-memory collection.
    pub fn from_all(rows: Vec<T>, limit: usize, offset: usize) -> Self {
        let end = offset.saturating_add(limit);
        let has_more = rows.len() > end;
        let items = rows.into_iter().skip(offset).take(limit).collect();
        Page {
            items,
            limit,
            offset,
            next_offset: has_more.then_some(end),
        }
    }
}

/// Parse the `limit`/`offset` query parameters shared by all paginated list
/// endpoints. `limit` is clamped to `[1, MAX_PAGE_LIMIT]` (default
/// [`DEFAULT_PAGE_LIMIT`]); a missing or malformed `offset` falls back to 0.
pub fn page_params(req: &Request) -> (usize, usize) {
    let limit = req
        .query::<usize>("limit")
        .unwrap_or(DEFAULT_PAGE_LIMIT)
        .clamp(1, MAX_PAGE_LIMIT);
    let offset = req
        .query::<usize>("offset")
        .unwrap_or(0)
        .min(i64::MAX as usize);
    (limit, offset)
}

// ===========================================================================
// Domain CRUD
// ===========================================================================

/// Wire view of a [`Domain`](crate::db::Domain).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct DomainView {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Body for creating a domain.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewDomain {
    /// The domain name, e.g. `example.com`.
    pub id: String,
}

/// Body for patching a domain.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PatchDomain {
    // Placeholder fields; the current [`Domain`](crate::db::Domain) model is
    // essentially a key, so PATCH is shape-only for now.
    #[serde(default)]
    pub note: Option<String>,
}

#[endpoint(
    summary = "List all domains",
    parameters(
        ("limit" = Option<usize>, Query, description = "Max items per page (server-enforced default and cap)"),
        ("offset" = Option<usize>, Query, description = "Number of items to skip"),
    ),
    responses(
        (status_code = 200, description = "All domains", body = ApiResponse<Page<DomainView>>),
        (status_code = 400, description = "Bad request", body = ApiProblem)
    )
)]
pub async fn list_domains(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let (limit, offset) = page_params(req);
    // TODO: load all domains from the DB and map to `DomainView`.
    let items: Vec<DomainView> = todo!("load domains into DomainView");
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(Page::from_all(items, limit, offset))));
}

#[endpoint(
    summary = "Create a new domain",
    request_body = NewDomain,
    responses(
        (status_code = 200, description = "Domain created successfully", body = ApiResponse<DomainView>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 409, description = "Domain already exists", body = ApiProblem)
    )
)]
pub async fn create_domain(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let body = match req.parse_json::<NewDomain>().await {
        Ok(b) => b,
        Err(_) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ApiProblem::validation_error(
                "Failed to parse request body",
            )));
            return;
        }
    };
    // TODO: upsert domain `body.id` in the DB.
    let domain_id = body.id;
    let view: DomainView = todo!("persist domain and build DomainView");
    let _ = domain_id;
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(view)));
}

#[endpoint(
    summary = "Get a single domain",
    parameters(
        ("domain_id" = String, Path, description = "Domain name, e.g. example.com")
    ),
    responses(
        (status_code = 200, description = "The domain", body = ApiResponse<DomainView>),
        (status_code = 404, description = "Domain not found", body = ApiProblem)
    )
)]
pub async fn get_domain(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let domain_id = req.param::<String>("domain_id").unwrap_or_default();
    // TODO: fetch domain `domain_id` from the DB.
    let view: DomainView = todo!("fetch domain by id");
    let _ = domain_id;
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(view)));
}

#[endpoint(
    summary = "Update a domain",
    request_body = PatchDomain,
    parameters(
        ("domain_id" = String, Path, description = "Domain name, e.g. example.com")
    ),
    responses(
        (status_code = 200, description = "Domain updated", body = ApiResponse<DomainView>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 404, description = "Domain not found", body = ApiProblem)
    )
)]
pub async fn patch_domain(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let domain_id = req.param::<String>("domain_id").unwrap_or_default();
    let body = match req.parse_json::<PatchDomain>().await {
        Ok(b) => b,
        Err(_) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ApiProblem::validation_error(
                "Failed to parse request body",
            )));
            return;
        }
    };
    // TODO: apply patch to domain `domain_id`.
    let view: DomainView = todo!("apply patch and build DomainView");
    let _ = (domain_id, body);
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(view)));
}

#[endpoint(
    summary = "Delete a domain",
    parameters(
        ("domain_id" = String, Path, description = "Domain name, e.g. example.com")
    ),
    responses(
        (status_code = 200, description = "Domain deleted successfully", body = ApiResponse<()>),
        (status_code = 404, description = "Domain not found", body = ApiProblem)
    )
)]
pub async fn delete_domain(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let domain_id = req.param::<String>("domain_id").unwrap_or_default();
    // TODO: delete domain `domain_id` from the DB.
    todo!("delete domain by id");
}

// ===========================================================================
// Mailbox (email address) CRUD
// ===========================================================================

/// Wire view of a [`Mailbox`](crate::db::Mailbox) (a provisioned email address).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct MailboxView {
    /// Full address, e.g. `user@example.com`.
    pub id: String,
    pub domain_id: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Body for creating a mailbox. `id` is the full address; `domain_id` is
/// optional and, when omitted, is derived from the address's domain part.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewMailbox {
    /// Full address, e.g. `user@example.com`.
    pub id: String,
    /// Owning domain. When omitted it is derived from `id`.
    #[serde(default)]
    pub domain_id: Option<String>,
}

/// Body for patching a mailbox.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PatchMailbox {
    #[serde(default)]
    pub domain_id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[endpoint(
    summary = "List all mailboxes",
    parameters(
        ("limit" = Option<usize>, Query, description = "Max items per page (server-enforced default and cap)"),
        ("offset" = Option<usize>, Query, description = "Number of items to skip"),
        ("domain_id" = Option<String>, Query, description = "Optional filter by owning domain"),
    ),
    responses(
        (status_code = 200, description = "All mailboxes", body = ApiResponse<Page<MailboxView>>),
        (status_code = 400, description = "Bad request", body = ApiProblem)
    )
)]
pub async fn list_mailboxes(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let (limit, offset) = page_params(req);
    let domain_filter = req.query::<String>("domain_id");
    // TODO: load mailboxes (optionally filtered by `domain_filter`) into MailboxView.
    let items: Vec<MailboxView> = todo!("load mailboxes into MailboxView");
    let _ = domain_filter;
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(Page::from_all(items, limit, offset))));
}

#[endpoint(
    summary = "Create a new mailbox (email address)",
    request_body = NewMailbox,
    responses(
        (status_code = 200, description = "Mailbox created successfully", body = ApiResponse<MailboxView>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 409, description = "Mailbox already exists", body = ApiProblem)
    )
)]
pub async fn create_mailbox(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let body = match req.parse_json::<NewMailbox>().await {
        Ok(b) => b,
        Err(_) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ApiProblem::validation_error(
                "Failed to parse request body",
            )));
            return;
        }
    };
    // TODO: validate the address and upsert the mailbox in the DB.
    let view: MailboxView = todo!("persist mailbox and build MailboxView");
    let _ = body;
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(view)));
}

#[endpoint(
    summary = "Get a single mailbox",
    parameters(
        ("mailbox_id" = String, Path, description = "Full address, e.g. user@example.com, URL-encoded")
    ),
    responses(
        (status_code = 200, description = "The mailbox", body = ApiResponse<MailboxView>),
        (status_code = 404, description = "Mailbox not found", body = ApiProblem)
    )
)]
pub async fn get_mailbox(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let mailbox_id = req.param::<String>("mailbox_id").unwrap_or_default();
    // TODO: fetch mailbox `mailbox_id` from the DB.
    let view: MailboxView = todo!("fetch mailbox by id");
    let _ = mailbox_id;
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(view)));
}

#[endpoint(
    summary = "Update a mailbox",
    request_body = PatchMailbox,
    parameters(
        ("mailbox_id" = String, Path, description = "Full address, e.g. user@example.com, URL-encoded")
    ),
    responses(
        (status_code = 200, description = "Mailbox updated", body = ApiResponse<MailboxView>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 404, description = "Mailbox not found", body = ApiProblem)
    )
)]
pub async fn patch_mailbox(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let mailbox_id = req.param::<String>("mailbox_id").unwrap_or_default();
    let body = match req.parse_json::<PatchMailbox>().await {
        Ok(b) => b,
        Err(_) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ApiProblem::validation_error(
                "Failed to parse request body",
            )));
            return;
        }
    };
    // TODO: apply patch to mailbox `mailbox_id`.
    let view: MailboxView = todo!("apply patch and build MailboxView");
    let _ = (mailbox_id, body);
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(view)));
}

#[endpoint(
    summary = "Delete a mailbox",
    parameters(
        ("mailbox_id" = String, Path, description = "Full address, e.g. user@example.com, URL-encoded")
    ),
    responses(
        (status_code = 200, description = "Mailbox deleted successfully", body = ApiResponse<()>),
        (status_code = 404, description = "Mailbox not found", body = ApiProblem)
    )
)]
pub async fn delete_mailbox(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let mailbox_id = req.param::<String>("mailbox_id").unwrap_or_default();
    // TODO: delete mailbox `mailbox_id` from the DB.
    todo!("delete mailbox by id");
}

// ===========================================================================
// Send email
// ===========================================================================

/// Body for sending a message. Mirrors the `From`/`To`/`Cc`/`Bcc`/`Subject`/
/// body of an email; the sender is a provisioned mailbox.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SendEmail {
    /// Envelope sender (`MAIL FROM`); must be a provisioned mailbox.
    pub from: String,
    /// Forward-path recipients.
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    #[serde(default)]
    pub subject: Option<String>,
    /// Message body (text or HTML depending on `content_type`).
    pub body: String,
    /// MIME content type of `body`, defaults to `text/plain`.
    #[serde(default)]
    pub content_type: Option<String>,
}

/// Result of a successful send: the stored message id and the enqueued delivery
/// job id (the [`Outbound`](crate::db::Outbound) queue entry).
#[derive(Debug, Serialize, ToSchema)]
pub struct SendResult {
    pub message_id: u64,
    pub outbound_id: u64,
}

#[endpoint(
    summary = "Send an email",
    request_body = SendEmail,
    responses(
        (status_code = 200, description = "Message enqueued for delivery", body = ApiResponse<SendResult>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 409, description = "Sender is not a provisioned mailbox", body = ApiProblem)
    )
)]
pub async fn send_email(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let body = match req.parse_json::<SendEmail>().await {
        Ok(b) => b,
        Err(_) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ApiProblem::validation_error(
                "Failed to parse request body",
            )));
            return;
        }
    };
    // TODO: build the `lettre` message from `body` and call
    // `crate::send::enqueue_outbound(&mut state.db, &body.from, rcpt, message)`
    // to persist the message and enqueue delivery, then map the returned
    // `(message_id, outbound_id)` to `SendResult`.
    let outbox = depot
        .get_typed_mut::<crate::app::AppState>()
        .expect("AppState not found");
    let result: SendResult = todo!("enqueue outbound delivery");
    let _ = outbox;
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(result)));
}

// ===========================================================================
// Messages of an email address (send or receive)
// ===========================================================================

/// Direction filter for listing a mailbox's messages.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub enum MessageDirection {
    /// Every message the mailbox sent or received.
    Both,
    /// Messages received (the `Inbound` relation).
    Inbound,
    /// Messages sent (the `Outbound` relation).
    Outbound,
}

/// Wire view of a [`Message`](crate::db::Message), annotated with which way it
/// flowed relative to a mailbox.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct MessageView {
    pub id: u64,
    /// `inbound` / `outbound` — empty when the message is shown in isolation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    pub mail_from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id_header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_address: Option<String>,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Per-message listing filters for a mailbox.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ListMessagesQuery {
    /// Which direction to return; defaults to `both`.
    #[serde(default)]
    pub direction: Option<MessageDirection>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[endpoint(
    summary = "List messages of a mailbox (sent and/or received)",
    parameters(
        ("mailbox_id" = String, Path, description = "Full address, e.g. user@example.com, URL-encoded"),
        ("direction" = Option<String>, Query, description = "both | inbound | outbound (default: both)"),
        ("limit" = Option<usize>, Query, description = "Max items per page (server-enforced default and cap)"),
        ("offset" = Option<usize>, Query, description = "Number of items to skip"),
    ),
    responses(
        (status_code = 200, description = "Messages of the mailbox", body = ApiResponse<Page<MessageView>>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 404, description = "Mailbox not found", body = ApiProblem)
    )
)]
pub async fn list_mailbox_messages(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let mailbox_id = req.param::<String>("mailbox_id").unwrap_or_default();
    let query = req.parse_queries::<ListMessagesQuery>().ok();
    let (limit, offset) = if let Some(q) = query {
        (q.limit.unwrap_or(DEFAULT_PAGE_LIMIT), q.offset.unwrap_or(0))
    } else {
        page_params(req)
    };
    // TODO: join `Inbound`/`Outbound` (filtered by `direction`) against
    // `Message` for `mailbox_id`, then map to `MessageView`.
    let items: Vec<MessageView> = todo!("load messages for mailbox");
    let _ = (mailbox_id, limit, offset);
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(Page::from_all(items, limit, offset))));
}

#[endpoint(
    summary = "List received messages of a mailbox",
    parameters(
        ("mailbox_id" = String, Path, description = "Full address, e.g. user@example.com, URL-encoded"),
        ("limit" = Option<usize>, Query, description = "Max items per page (server-enforced default and cap)"),
        ("offset" = Option<usize>, Query, description = "Number of items to skip"),
    ),
    responses(
        (status_code = 200, description = "Inbound messages of the mailbox", body = ApiResponse<Page<MessageView>>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 404, description = "Mailbox not found", body = ApiProblem)
    )
)]
pub async fn list_mailbox_inbox(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let mailbox_id = req.param::<String>("mailbox_id").unwrap_or_default();
    let (limit, offset) = page_params(req);
    // TODO: join `Inbound` against `Message` for `mailbox_id`.
    let items: Vec<MessageView> = todo!("load inbound messages for mailbox");
    let _ = (mailbox_id, limit, offset);
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(Page::from_all(items, limit, offset))));
}

#[endpoint(
    summary = "List sent messages of a mailbox",
    parameters(
        ("mailbox_id" = String, Path, description = "Full address, e.g. user@example.com, URL-encoded"),
        ("limit" = Option<usize>, Query, description = "Max items per page (server-enforced default and cap)"),
        ("offset" = Option<usize>, Query, description = "Number of items to skip"),
    ),
    responses(
        (status_code = 200, description = "Outbound messages of the mailbox", body = ApiResponse<Page<MessageView>>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 404, description = "Mailbox not found", body = ApiProblem)
    )
)]
pub async fn list_mailbox_sent(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let mailbox_id = req.param::<String>("mailbox_id").unwrap_or_default();
    let (limit, offset) = page_params(req);
    // TODO: join `Outbound` against `Message` for `mailbox_id`.
    let items: Vec<MessageView> = todo!("load outbound messages for mailbox");
    let _ = (mailbox_id, limit, offset);
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(Page::from_all(items, limit, offset))));
}

#[endpoint(
    summary = "Get a single message",
    parameters(
        ("message_id" = u64, Path, description = "Numeric message id")
    ),
    responses(
        (status_code = 200, description = "The message", body = ApiResponse<MessageView>),
        (status_code = 404, description = "Message not found", body = ApiProblem)
    )
)]
pub async fn get_message(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let message_id = req.param::<usize>("message_id").unwrap_or_default();
    // TODO: fetch `Message` by `message_id` and map to `MessageView`.
    let view: MessageView = todo!("fetch message by id");
    let _ = message_id;
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(view)));
}

#[endpoint(
    summary = "Delete a message",
    parameters(
        ("message_id" = u64, Path, description = "Numeric message id")
    ),
    responses(
        (status_code = 200, description = "Message deleted successfully", body = ApiResponse<()>),
        (status_code = 404, description = "Message not found", body = ApiProblem)
    )
)]
pub async fn delete_message(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let message_id = req.param::<usize>("message_id").unwrap_or_default();
    // TODO: delete `Message` by `message_id` (and its `Inbound`/`Outbound` links).
    todo!("delete message by id");
}

// ===========================================================================
// Root / health
// ===========================================================================

#[endpoint(
    summary = "Liveness probe",
    responses(
        (status_code = 200, description = "API is running", body = String)
    )
)]
async fn hello(res: &mut Response) {
    res.render(Text::Plain("Zonemail API is running!"));
}

// ===========================================================================
// Router assembly
// ===========================================================================

pub fn create_router() -> Router {
    Router::new()
        // Health
        .get(hello)
        // Domain CRUD
        .push(
            Router::with_path("domains")
                .get(list_domains)
                .post(create_domain)
                .push(
                    Router::with_path("{domain_id}")
                        .get(get_domain)
                        .patch(patch_domain)
                        .delete(delete_domain),
                ),
        )
        // Mailbox (email address) CRUD
        .push(
            Router::with_path("mailboxes")
                .get(list_mailboxes)
                .post(create_mailbox)
                .push(
                    Router::with_path("{mailbox_id}")
                        .get(get_mailbox)
                        .patch(patch_mailbox)
                        .delete(delete_mailbox)
                        // Messages of an email address (send or receive)
                        .push(
                            Router::with_path("messages")
                                .get(list_mailbox_messages)
                                .push(Router::with_path("inbox").get(list_mailbox_inbox))
                                .push(Router::with_path("sent").get(list_mailbox_sent)),
                        ),
                ),
        )
        // Send email
        .push(Router::with_path("mail").post(send_email))
        // Message CRUD (by id, resource-agnostic)
        .push(
            Router::with_path("messages").push(
                Router::with_path("{message_id}")
                    .get(get_message)
                    .delete(delete_message),
            ),
        )
}

/// The API router wrapped with a generated OpenAPI document and a Scalar UI.
///
/// Mirrors `janux::router::api_with_doc`: the same `api_router` is first merged
/// into an [`OpenApi`] spec, which is then (1) exposed as raw JSON at
/// `/doc/openapi.json`, (2) rendered as a browser UI by Scalar at
/// `/doc/scalar`, and (3) the underlying router is mounted for live requests.
/// See [`create_router`] for the endpoint inventory.
pub fn api_with_doc() -> Router {
    let api_router = create_router();
    let doc = OpenApi::new("Zonemail API", "1.0.0").merge_router(&api_router);

    Router::new()
        .push(doc.into_router("/doc/openapi.json"))
        .push(Scalar::new("/doc/openapi.json").into_router("/doc"))
        .push(api_router)
}
