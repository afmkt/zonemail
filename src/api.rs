//! HTTP API surface for Zonemail.
//!
//! This module defines the *shape* of the REST API: request/response DTOs, the
//! handler signatures, the status-code + envelope conventions, and the route
//! wiring. Each handler wires the request to a `toasty` DB read/write (or SMTP
//! handoff).
//!
//! The response envelope types ([`ApiProblem`], [`ApiResponse`], [`Page`]) are
//! borrowed verbatim from the `janux` project so the two services speak the same
//! wire contract.
//!
//! ## Resources
#![allow(clippy::missing_errors_description)]
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

use crate::app::AppState;
use crate::db::{Domain, Inbound, Mailbox, MailboxError, Message, Outbound};
use crate::send::{enqueue_forward, enqueue_outbound};
use lettre::message::{header::ContentType, Mailbox as LettreMailbox, Message as LettreMessage};

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

/// API DTO / wire projection of a [`Domain`](crate::db::Domain).
///
/// This is an internal DTO name with no bearing on the JSON wire shape (which is
/// fixed by `Serialize` + field names). Only the response *envelope* is shared
/// with janux; the payload semantics are zonemail's own.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct DomainDTO {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&Domain> for DomainDTO {
    fn from(domain: &Domain) -> Self {
        DomainDTO {
            id: domain.id.clone(),
            created_at: domain.created_at.to_string(),
            updated_at: domain.updated_at.to_string(),
          }
      }
}
impl From<Domain> for DomainDTO {
    fn from(domain: Domain) -> Self {
        DomainDTO {
            id: domain.id,
            created_at: domain.created_at.to_string(),
            updated_at: domain.updated_at.to_string(),
          }
      }
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
        (status_code = 200, description = "All domains", body = ApiResponse<Page<DomainDTO>>),
        (status_code = 400, description = "Bad request", body = ApiProblem)
    )
)]
pub async fn list_domains(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let (limit, offset) = page_params(req);
    // Load every domain and project each row into its API DTO.
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    match toasty::query!(Domain).exec(&mut db).await {
        Ok(domains) => {
            let items: Vec<DomainDTO> = domains.into_iter().map(DomainDTO::from).collect();
            res.status_code(StatusCode::OK);
            res.render(Json(ApiResponse::ok(Page::from_all(items, limit, offset))));
          }
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(ApiProblem::server_error(&e.to_string())));
          }
      }
}

#[endpoint(
    summary = "Create a new domain",
    request_body = NewDomain,
    responses(
        (status_code = 200, description = "Domain created successfully", body = ApiResponse<DomainDTO>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 409, description = "Domain already exists", body = ApiProblem)
    )
)]
pub async fn create_domain(req: &mut Request, depot: &mut Depot, res: &mut Response) {
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
    // Create the domain. A duplicate is a 409: the `Domain` model has no
    // mutable business fields, so a create is the only meaningful transition.
    let domain_id = body.id.trim().to_string();
    if domain_id.is_empty() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(ApiProblem::bad_request(
              "domain id must not be empty",
         )));
        return;
      }
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    if Domain::get_by_id(&mut db, &domain_id).await.is_ok() {
        res.status_code(StatusCode::CONFLICT);
        res.render(Json(ApiProblem::conflict(&format!(
              "domain '{domain_id}' already exists",
         ))));
        return;
      }
    match toasty::create!(Domain { id: domain_id.clone() }).exec(&mut db).await {
        Ok(domain) => {
            res.status_code(StatusCode::OK);
            res.render(Json(ApiResponse::ok(DomainDTO::from(&domain))));
          }
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(ApiProblem::server_error(&e.to_string())));
          }
      }
}

#[endpoint(
    summary = "Get a single domain",
    parameters(
        ("domain_id" = String, Path, description = "Domain name, e.g. example.com")
    ),
    responses(
        (status_code = 200, description = "The domain", body = ApiResponse<DomainDTO>),
        (status_code = 404, description = "Domain not found", body = ApiProblem)
    )
)]
pub async fn get_domain(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let domain_id = req.param::<String>("domain_id").unwrap_or_default();
    // Load the domain by id, surfacing a 404 when it is absent.
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    let domain = match Domain::get_by_id(&mut db, &domain_id).await {
        Ok(domain) => domain,
        Err(_) => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(Json(ApiProblem::not_found(&format!(
                  "domain '{domain_id}' not found",
             ))));
            return;
          }
      };
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(DomainDTO::from(&domain))));
}

#[endpoint(
    summary = "Update a domain",
    request_body = PatchDomain,
    parameters(
        ("domain_id" = String, Path, description = "Domain name, e.g. example.com")
    ),
    responses(
        (status_code = 200, description = "Domain updated", body = ApiResponse<DomainDTO>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 404, description = "Domain not found", body = ApiProblem)
    )
)]
pub async fn patch_domain(req: &mut Request, depot: &mut Depot, res: &mut Response) {
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
    // The current `Domain` model exposes no mutable business columns, so a patch
    // is a fetch-and-echo of the live row. `body` is parsed for wire
    // compatibility but not yet acted upon; apply future fields here.
    let _ = &body;
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    let domain = match Domain::get_by_id(&mut db, &domain_id).await {
        Ok(domain) => domain,
        Err(_) => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(Json(ApiProblem::not_found(&format!(
                  "domain '{domain_id}' not found",
             ))));
            return;
          }
      };
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(DomainDTO::from(&domain))));
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
pub async fn delete_domain(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let domain_id = req.param::<String>("domain_id").unwrap_or_default();
    // Fetch first so a missing domain surfaces as a 404: a bare filtered
    // delete would silently remove zero rows and report success.
    //
    // NOTE: this removes the `Domain` row only. Dependents (`Mailbox`,
    // `Record`, and inbound/outbound links referencing this domain) are not
    // cascade-deleted here; add that before exposing multi-resource teardown.
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    if Domain::get_by_id(&mut db, &domain_id).await.is_err() {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Json(ApiProblem::not_found(&format!(
              "domain '{domain_id}' not found",
         ))));
        return;
      }
    match toasty::query!(Domain filter .id == #domain_id)
          .delete()
          .exec(&mut db)
          .await
      {
        Ok(()) => {
            res.status_code(StatusCode::OK);
            res.render(Json(ApiResponse::ok(())));
          }
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(ApiProblem::server_error(&e.to_string())));
          }
      }
}

// ===========================================================================
// Mailbox (email address) CRUD
// ===========================================================================

/// API DTO / wire projection of a [`Mailbox`](crate::db::Mailbox) (a provisioned
/// email address).
///
/// As with [`DomainDTO`], this is an internal name with no bearing on the JSON
/// wire shape (which is fixed by `Serialize` + field names); only the response
/// *envelope* is shared with janux.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct MailboxDTO {
    /// Full address, e.g. `user@example.com`.
    pub id: String,
    pub domain_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub forward_to: Option<String>,
}

impl From<&Mailbox> for MailboxDTO {
    fn from(mailbox: &Mailbox) -> Self {
        MailboxDTO {
            id: mailbox.id.clone(),
            domain_id: mailbox.domain_id.clone(),
            created_at: mailbox.created_at.to_string(),
            updated_at: mailbox.updated_at.to_string(),
            forward_to: mailbox.forward_to.clone(),
        }
    }
}
impl From<Mailbox> for MailboxDTO {
    fn from(mailbox: Mailbox) -> Self {
        MailboxDTO {
            id: mailbox.id,
            domain_id: mailbox.domain_id,
            created_at: mailbox.created_at.to_string(),
            updated_at: mailbox.updated_at.to_string(),
            forward_to: mailbox.forward_to,
        }
    }
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
     #[serde(default)]
    pub forward_to: Option<String>,
}

/// Body for patching a mailbox.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PatchMailbox {
    #[serde(default)]
    pub domain_id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
     #[serde(default)]
    pub forward_to: Option<String>,
}

#[endpoint(
    summary = "List all mailboxes",
    parameters(
        ("limit" = Option<usize>, Query, description = "Max items per page (server-enforced default and cap)"),
        ("offset" = Option<usize>, Query, description = "Number of items to skip"),
        ("domain_id" = Option<String>, Query, description = "Optional filter by owning domain"),
    ),
    responses(
        (status_code = 200, description = "All mailboxes", body = ApiResponse<Page<MailboxDTO>>),
        (status_code = 400, description = "Bad request", body = ApiProblem)
    )
)]
pub async fn list_mailboxes(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let (limit, offset) = page_params(req);
    let domain_filter = req.query::<String>("domain_id");
    // Load every mailbox, optionally narrowed to a single owning domain, then
    // project each row into its DTO.
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    let rows: Result<Vec<Mailbox>, _> = match domain_filter {
        Some(domain_id) => toasty::query!(Mailbox filter .domain_id == #domain_id)
           .exec(&mut db)
           .await,
        None => toasty::query!(Mailbox).exec(&mut db).await,
    };
    let items: Vec<MailboxDTO> = match rows {
        Ok(rows) => rows.into_iter().map(MailboxDTO::from).collect(),
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(ApiProblem::server_error(&e.to_string())));
            return;
        }
    };
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(Page::from_all(items, limit, offset))));
}

#[endpoint(
    summary = "Create a new mailbox (email address)",
    request_body = NewMailbox,
    responses(
        (status_code = 200, description = "Mailbox created successfully", body = ApiResponse<MailboxDTO>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 409, description = "Mailbox already exists", body = ApiProblem)
    )
)]
pub async fn create_mailbox(req: &mut Request, depot: &mut Depot, res: &mut Response) {
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
    // Build and validate the mailbox. The id is the full address; normalize it
    // to lowercase (matching the seeding path) so open-relay lookups, which
    // also lowercase, stay consistent. An explicit `domain_id` must agree with
    // the address's own domain part, otherwise fall back to the derived one.
    let mailbox_id = body.id.trim().to_lowercase();
    if mailbox_id.is_empty() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(ApiProblem::bad_request(
                "email address must not be empty",
        )));
        return;
    }
    let mut mailbox = match Mailbox::try_from(mailbox_id) {
        Ok(mailbox) => mailbox,
        Err(MailboxError::InvalidFormat) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ApiProblem::bad_request(
                "invalid email address (missing domain part)",
            )));
            return;
        }
    };
    if let Some(domain_id) = body.domain_id {
        let provided = domain_id.trim().to_lowercase();
        // Reject a contradictory explicit domain rather than trusting it: the
        // stored domain must be the address's own domain part.
        if provided != mailbox.domain_id {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ApiProblem::bad_request(
                "domain_id does not match the domain part of the address",
            )));
            return;
        }
        mailbox.domain_id = provided;
    }
    mailbox.forward_to = body.forward_to;
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    // Fail on a duplicate address rather than silently upserting an existing mailbox.
    if Mailbox::get_by_id(&mut db, &mailbox.id).await.is_ok() {
        res.status_code(StatusCode::CONFLICT);
        res.render(Json(ApiProblem::conflict(
                &format!("mailbox '{}' already exists", mailbox.id),
        )));
        return;
    }
    match toasty::create!(Mailbox {
        id: mailbox.id.clone(),
        domain_id: mailbox.domain_id.clone(),
        forward_to: mailbox.forward_to.clone(),
     })
     .exec(&mut db)
     .await
     {
        Ok(created) => {
            res.status_code(StatusCode::OK);
            res.render(Json(ApiResponse::ok(MailboxDTO::from(&created))));
        }
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(ApiProblem::server_error(&e.to_string())));
        }
    }
}

#[endpoint(
    summary = "Get a single mailbox",
    parameters(
        ("mailbox_id" = String, Path, description = "Full address, e.g. user@example.com, URL-encoded")
    ),
    responses(
        (status_code = 200, description = "The mailbox", body = ApiResponse<MailboxDTO>),
        (status_code = 404, description = "Mailbox not found", body = ApiProblem)
    )
)]
pub async fn get_mailbox(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let mailbox_id = req.param::<String>("mailbox_id").unwrap_or_default();
    // Load the mailbox by its full address, 404 when absent.
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    let mailbox = match Mailbox::get_by_id(&mut db, &mailbox_id).await {
        Ok(mailbox) => mailbox,
        Err(_) => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(Json(ApiProblem::not_found(
                    &format!("mailbox '{mailbox_id}' not found"),
            )));
            return;
        }
    };
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(MailboxDTO::from(&mailbox))));
}

#[endpoint(
    summary = "Update a mailbox",
    request_body = PatchMailbox,
    parameters(
        ("mailbox_id" = String, Path, description = "Full address, e.g. user@example.com, URL-encoded")
    ),
    responses(
        (status_code = 200, description = "Mailbox updated", body = ApiResponse<MailboxDTO>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 404, description = "Mailbox not found", body = ApiProblem)
    )
)]
pub async fn patch_mailbox(req: &mut Request, depot: &mut Depot, res: &mut Response) {
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
    // Fetch the target (404 when absent) and apply the optional patch fields:
    // `forward_to` (partial — `None` leaves it untouched) and an optional
    // `domain_id` override that must keep matching the address's domain part.
    // `note` has no backing column and is ignored, as in the `PatchDomain`
    // placeholder. After persisting we re-read the row so the response carries
    // the DB-updated timestamps.
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    let mut mailbox = match Mailbox::get_by_id(&mut db, &mailbox_id).await {
        Ok(mailbox) => mailbox,
        Err(_) => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(Json(ApiProblem::not_found(
                    &format!("mailbox '{mailbox_id}' not found"),
            )));
            return;
        }
    };
    if let Some(domain_id) = body.domain_id {
        let provided = domain_id.trim().to_lowercase();
        if provided != mailbox.domain_id {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ApiProblem::bad_request(
                "domain_id does not match the domain part of the address",
            )));
            return;
        }
        mailbox.domain_id = provided;
    }
    mailbox.forward_to = body.forward_to.or_else(|| mailbox.forward_to.clone());
    // Snapshot the post-patch values so the update target (which needs `&mut
    // mailbox` for its primary key) and the assignment values don't contend for
    // the same binding.
    let new_domain_id = mailbox.domain_id.clone();
    let new_forward_to = mailbox.forward_to.clone();
    match toasty::update! {
            mailbox {
                domain_id: new_domain_id,
                forward_to: new_forward_to,
            }
        }
        .exec(&mut db)
        .await
        {
        Ok(_) => {}
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(ApiProblem::server_error(&e.to_string())));
            return;
        }
    }
    // Re-read so the response reflects the stored value, including timestamps.
    let updated = match Mailbox::get_by_id(&mut db, &mailbox_id).await {
        Ok(updated) => updated,
        Err(_) => {
                // We just wrote this row, so a second miss is a real error.
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(ApiProblem::server_error(
                    "mailbox vanished after update",
            )));
            return;
        }
    };
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(MailboxDTO::from(&updated))));
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
pub async fn delete_mailbox(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let mailbox_id = req.param::<String>("mailbox_id").unwrap_or_default();
    // Fetch first so a missing mailbox surfaces as a 404 (a bare filtered delete
    // would silently remove zero rows and still report success).
    //
    // NOTE: this removes the `Mailbox` row only. Its `Inbound`/`Outbound` link
    // rows are not cascade-deleted here; add that before real teardown.
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    if Mailbox::get_by_id(&mut db, &mailbox_id).await.is_err() {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Json(ApiProblem::not_found(
                &format!("mailbox '{mailbox_id}' not found"),
        )));
        return;
    }
    match toasty::query!(Mailbox filter .id == #mailbox_id)
             .delete()
             .exec(&mut db)
             .await
        {
        Ok(()) => {
            res.status_code(StatusCode::OK);
            res.render(Json(ApiResponse::ok(())));
        }
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(ApiProblem::server_error(&e.to_string())));
        }
    }
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

/// Result of a successful send: the stored message id plus the enqueued delivery
/// job ids (one [`Outbound`](crate::db::Outbound) queue entry per recipient).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SendResult {
    pub message_id: u64,
    pub outbound_ids: Vec<u64>,
}

#[endpoint(
    summary = "Send an email",
    request_body = SendEmail,
    responses(
        (status_code = 200, description = "Message enqueued for delivery", body = ApiResponse<SendResult>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 409, description = "Sender is not a provisioned mailbox", body = ApiProblem),
        (status_code = 500, description = "Internal server error", body = ApiProblem)
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

     // Recipients: `to`, then `cc`, then `bcc`, de-duplicated in first-seen
     // order. An empty set has nowhere to deliver, so it is a bad request. Since
     // `Outbound.rcpt_to` carries a single address, each recipient gets its own
     // `Outbound` job sharing one stored `Message`.
    let mut recipients: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in body.to.iter().chain(&body.cc).chain(&body.bcc) {
        if seen.insert(r.to_string()) {
            recipients.push(r.to_string());
        }
     }
    if recipients.is_empty() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(ApiProblem::bad_request(
             "no recipients: at least one of to, cc, or bcc is required",
          )));
        return;
     }

     // The envelope sender must be a provisioned mailbox because
     // `Outbound.sender_id` is a non-nullable foreign key.
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    let sender = body.from.to_string();
    if Mailbox::get_by_id(&mut db, &sender).await.is_err() {
        res.status_code(StatusCode::CONFLICT);
        res.render(Json(ApiProblem::conflict(
             &format!("sender '{sender}' is not a provisioned mailbox"),
          )));
        return;
     }

     // Build the message once; it is shared by every recipient's delivery job.
    let message = match build_send_message(
          &sender,
          &body.to,
          &body.cc,
          &body.bcc,
         body.subject.as_deref(),
          &body.body,
         body.content_type.as_deref(),
      ) {
        Ok(m) => m,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ApiProblem::bad_request(&e.to_string())));
            return;
         }
     };

     // The first recipient persists the `Message` and enqueues its job; the rest
     // reuse that `message_id` via the forward path (no duplicate `Message` row).
    let (message_id, first_outbound) =
        match enqueue_outbound(&mut db, &sender, &recipients[0], message).await {
            Ok(pair) => pair,
            Err(e) => {
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                res.render(Json(ApiProblem::server_error(&e.to_string())));
                return;
             }
          };
    let mut outbound_ids = vec![first_outbound];
    for recipient in &recipients[1..] {
        match enqueue_forward(&mut db, message_id, &sender, recipient).await {
            Ok(outbound_id) => outbound_ids.push(outbound_id),
            Err(e) => {
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                res.render(Json(ApiProblem::server_error(&e.to_string())));
                return;
             }
          }
      }

    let result = SendResult {
        message_id,
        outbound_ids,
      };
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(result)));
}

// Build a `lettre` message from the `SendEmail` fields. Recipient strings are
// `Mailbox`-parsed up front so a malformed address surfaces as a `400` rather than
// failing later in the delivery worker. `content_type` is optional; a missing one
// keeps the `text/plain` default `lettre` picks for a plain `String` body.
fn build_send_message(
    from: &str,
    to: &[String],
    cc: &[String],
    bcc: &[String],
    subject: Option<&str>,
    body: &str,
    content_type: Option<&str>,
) -> Result<LettreMessage, Box<dyn std::error::Error + Send + Sync>> {
    let from = from
         .parse::<LettreMailbox>()
         .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(format!("invalid From address '{from}': {e}")))?;
    let to_addrs = parse_addrs(to)?;
    let cc_addrs = parse_addrs(cc)?;
    let bcc_addrs = parse_addrs(bcc)?;

    let mut builder = LettreMessage::builder().from(from);
    for a in to_addrs {
        builder = builder.to(a);
     }
    for a in cc_addrs {
        builder = builder.cc(a);
     }
    for a in bcc_addrs {
        builder = builder.bcc(a);
     }

     // Only override the content type when one is supplied; otherwise let `lettre`
     // apply its `text/plain` default.
    if let Some(ct) = content_type {
        let ct_header = ContentType::parse(ct)
              .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(format!("invalid content_type '{ct}': {e}")))?;
        builder = builder.header(ct_header);
     }
    if let Some(s) = subject {
        builder = builder.subject(s.to_string());
     }
    builder
         .body(body.to_string())
         .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(format!("failed to build message: {e}")))
}

// Parse a slice of recipient strings into `lettre` addresses, mapping any parse
// error to a `Send + Sync` box so a bad address yields a `400`.
fn parse_addrs(addrs: &[String]) -> Result<Vec<LettreMailbox>, Box<dyn std::error::Error + Send + Sync>> {
    addrs
         .iter()
         .map(|s| s.parse::<LettreMailbox>().map_err(Box::<dyn std::error::Error + Send + Sync>::from))
         .collect()
}

// ===========================================================================
// Messages of an email address (send or receive)
// ===========================================================================

/// Direction filter for listing a mailbox's messages.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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
pub struct MessageDTO {
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

impl From<&Message> for MessageDTO {
    fn from(message: &Message) -> Self {
        MessageDTO {
            id: message.id,
            direction: None,
            mail_from: message.mail_from.clone(),
            subject: message.subject.clone(),
            message_id_header: message.message_id_header.clone(),
            content_type: message.content_type.clone(),
            from_address: message.from_address.clone(),
            to: message.to.clone(),
            cc: message.cc.clone(),
            bcc: message.bcc.clone(),
            created_at: message.created_at.to_string(),
            updated_at: message.updated_at.to_string(),
        }
    }
}

impl From<Message> for MessageDTO {
    fn from(message: Message) -> Self {
        MessageDTO {
            id: message.id,
            direction: None,
            mail_from: message.mail_from,
            subject: message.subject,
            message_id_header: message.message_id_header,
            content_type: message.content_type,
            from_address: message.from_address,
            to: message.to,
            cc: message.cc,
            bcc: message.bcc,
            created_at: message.created_at.to_string(),
            updated_at: message.updated_at.to_string(),
        }
    }
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
        (status_code = 200, description = "Messages of the mailbox", body = ApiResponse<Page<MessageDTO>>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 404, description = "Mailbox not found", body = ApiProblem)
    )
)]
pub async fn list_mailbox_messages(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let mailbox_id = req.param::<String>("mailbox_id").unwrap_or_default();
    let query = req.parse_queries::<ListMessagesQuery>().ok();
        // Capture the direction filter before `query` is consumed below.
    let direction = query.as_ref().and_then(|q| q.direction.clone());
    let (limit, offset) = if let Some(q) = query {
        (q.limit.unwrap_or(DEFAULT_PAGE_LIMIT), q.offset.unwrap_or(0))
    } else {
        page_params(req)
    };
        // A 404 for an unprovisioned mailbox rather than a misleading empty list.
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    if !ensure_mailbox_exists(&mut db, &mailbox_id, res).await {
        return;
      }
    let items = match load_mailbox_messages(&mut db, &mailbox_id, direction).await {
        Ok(items) => items,
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(ApiProblem::server_error(&e.to_string())));
            return;
          }
      };
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
        (status_code = 200, description = "Inbound messages of the mailbox", body = ApiResponse<Page<MessageDTO>>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 404, description = "Mailbox not found", body = ApiProblem)
    )
)]
pub async fn list_mailbox_inbox(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let mailbox_id = req.param::<String>("mailbox_id").unwrap_or_default();
    let (limit, offset) = page_params(req);
        // 404 for an unprovisioned mailbox, then load its received (inbox) messages.
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    if !ensure_mailbox_exists(&mut db, &mailbox_id, res).await {
        return;
      }
    let items = match load_mailbox_messages(&mut db, &mailbox_id, Some(MessageDirection::Inbound)).await {
        Ok(items) => items,
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(ApiProblem::server_error(&e.to_string())));
            return;
          }
      };
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
        (status_code = 200, description = "Outbound messages of the mailbox", body = ApiResponse<Page<MessageDTO>>),
        (status_code = 400, description = "Bad request", body = ApiProblem),
        (status_code = 404, description = "Mailbox not found", body = ApiProblem)
    )
)]
pub async fn list_mailbox_sent(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let mailbox_id = req.param::<String>("mailbox_id").unwrap_or_default();
    let (limit, offset) = page_params(req);
        // 404 for an unprovisioned mailbox, then load its sent (outbox) messages.
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    if !ensure_mailbox_exists(&mut db, &mailbox_id, res).await {
        return;
      }
    let items = match load_mailbox_messages(&mut db, &mailbox_id, Some(MessageDirection::Outbound)).await {
        Ok(items) => items,
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(ApiProblem::server_error(&e.to_string())));
            return;
          }
      };
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(Page::from_all(items, limit, offset))));
}

#[endpoint(
    summary = "Get a single message",
    parameters(
        ("message_id" = u64, Path, description = "Numeric message id")
    ),
    responses(
        (status_code = 200, description = "The message", body = ApiResponse<MessageDTO>),
        (status_code = 404, description = "Message not found", body = ApiProblem)
    )
)]
pub async fn get_message(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let message_id: u64 = req.param::<u64>("message_id").unwrap_or_default();
        // Load the message by id, 404 when absent.
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    let message = match Message::get_by_id(&mut db, &message_id).await {
        Ok(message) => message,
        Err(_) => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(Json(ApiProblem::not_found(&format!("message '{message_id}' not found"))));
            return;
          }
      };
    res.status_code(StatusCode::OK);
    res.render(Json(ApiResponse::ok(MessageDTO::from(&message))));
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
pub async fn delete_message(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let message_id: u64 = req.param::<u64>("message_id").unwrap_or_default();
        // Fetch-first for a proper 404 (a bare filtered delete would silently
        // remove zero rows and report success), then delete the `Inbound`/
        // `Outbound` link rows before the `Message` itself. Each `Outbound`
        // deletion cascades to its `DeliveryStatus` children automatically
        // (toasty engine-level cascade via the `#[has_many]` relation).
    let state = depot.get_typed_mut::<AppState>().expect("AppState not found");
    let mut db = state.db.clone();
    if Message::get_by_id(&mut db, &message_id).await.is_err() {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Json(ApiProblem::not_found(&format!("message '{message_id}' not found"))));
        return;
      }
        // Remove link rows first so no dangling references remain.
    if let Err(e) = toasty::query!(Inbound filter .message_id == #message_id).delete().exec(&mut db).await {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(ApiProblem::server_error(&e.to_string())));
        return;
      }
    if let Err(e) = toasty::query!(Outbound filter .message_id == #message_id).delete().exec(&mut db).await {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(ApiProblem::server_error(&e.to_string())));
        return;
      }
    match toasty::query!(Message filter .id == #message_id).delete().exec(&mut db).await {
        Ok(()) => {
            res.status_code(StatusCode::OK);
            res.render(Json(ApiResponse::ok(())));
          }
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(ApiProblem::server_error(&e.to_string())));
          }
      }
}

// ===========================================================================
// Per-mailbox message loading (shared by the message-listing handlers)
// ===========================================================================

/// Render a 404 and return `false` when `mailbox_id` is not provisioned, so an
/// unknown mailbox never masquerades as an empty inbox/outbox.
async fn ensure_mailbox_exists(db: &mut toasty::Db, mailbox_id: &str, res: &mut Response) -> bool {
    match Mailbox::get_by_id(db, &mailbox_id.to_lowercase()).await {
        Ok(_) => true,
        Err(_) => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(Json(ApiProblem::not_found(&format!("mailbox '{mailbox_id}' not found"))));
            false
          }
      }
}

/// Load the [`MessageDTO`]s associated with `mailbox_id` for the requested
/// [`MessageDirection`]. `Inbound` lists received mail (the mailbox as the
/// `rcpt_to` of an [`Inbound`] link); `Outbound` lists sent mail (the mailbox as
/// the `sender_id` of an [`Outbound`] link); `None`/`Both` unions the two, de-
/// duplicated by message id. Each returned DTO carries its `direction`.
async fn load_mailbox_messages(
    db: &mut toasty::Db, mailbox_id: &str, direction: Option<MessageDirection>,
) -> Result<Vec<MessageDTO>, toasty::Error> {
    let want_in = !matches!(direction, Some(MessageDirection::Outbound));
    let want_out = !matches!(direction, Some(MessageDirection::Inbound));
    let mut ids: Vec<u64> = Vec::new();
    let mut dirs: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    if want_in {
        let key = mailbox_id.to_lowercase();
        let inbounds = toasty::query!(Inbound filter .rcpt_to == #key).exec(db).await?;
        for row in inbounds {
            ids.push(row.message_id);
            dirs.insert(row.message_id, "inbound".to_string());
          }
       }
    if want_out {
        let key = mailbox_id.to_lowercase();
        let outbounds = toasty::query!(Outbound filter .sender_id == #key).exec(db).await?;
        for row in outbounds {
            ids.push(row.message_id);
            dirs.insert(row.message_id, "outbound".to_string());
          }
       }
    // A message can be both received and sent; collapse its duplicate id.
    ids.sort_unstable();
    ids.dedup();
    let mut items: Vec<MessageDTO> = Vec::new();
    for id in ids {
        // Skip link rows whose `Message` has been removed.
        if let Ok(message) = Message::get_by_id(db, &id).await {
            let mut dto = MessageDTO::from(&message);
            dto.direction = dirs.get(&id).cloned();
            items.push(dto);
          }
       }
     Ok(items)
}

// ===========================================================================
// Root / health
// ===========================================================================

#[endpoint(
    summary = "Health check",
    responses(
        (status_code = 200, description = "API is running", body = ApiResponse<HealthInfo>)
    )
)]
async fn health() -> Json<ApiResponse<HealthInfo>> {
        Json(ApiResponse::ok(HealthInfo {
            service: "zonemail".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            status: "healthy".to_string(),
        }))
    }

/// Payload returned by the health probe, mirrored into the uniform
/// [`ApiResponse`] success envelope like every other endpoint.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct HealthInfo {
      /// Service name.
    pub service: String,
      /// Semantic version of the running build.
    pub version: String,
      /// Probe outcome (e.g. `"healthy"`).
    pub status: String,
}

// ===========================================================================
// Router assembly
// ===========================================================================

pub fn create_router() -> Router {
    Router::new()
        // Health
        .push(Router::with_path("health").get(health))
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


// ===========================================================================
// Tests
// ===========================================================================
//
// The HTTP handlers are exercised through salvo's in-process `TestClient`, which
// drives a real `Request` through the router and inspects the resulting
// `Response` without opening a socket. Every test gets its own isolated in-memory
// database via `AppState::connect_in_memory`, so suites are parallel-safe and
// self-contained (no fixtures, no on-disk state).
//
// Request bodies are built with `serde_json::json!` because the request DTOs
// (`NewDomain`, `NewMailbox`, `PatchMailbox`, `SendEmail`) are `Deserialize`-
// only (inbound bodies are never serialized on our side), while the response DTOs
// all derive `Deserialize`, letting tests deserialize `take_json` results.
#[cfg(test)]
mod tests {
    use super::*;
    use salvo::test::{ResponseExt, TestClient};

    use crate::app::AppState;

      /// A fresh API service backed by its own in-memory database, with the
      /// `AppState` injected into the router's depot for every request. The state
      /// is injected *by value* (not `Arc<..>`): the handlers call
      /// `depot.get_typed_mut::<AppState>()`, and affix-state keys the depot by
      /// the exact `TypeId` of the injected value, so the two must agree.
    async fn api_service() -> salvo::Service {
        let state = AppState::connect_in_memory().await.expect("in-memory state");
        let router = create_router().hoop(salvo::affix_state::inject(state));
        salvo::Service::new(router)
      }

     // --- Uniform API envelope ----------------------------------------------

     #[test]
    fn api_problem_constructions_carry_status_and_detail() {
        let bad = ApiProblem::bad_request("nope");
        assert_eq!(bad.status, 400);
        assert_eq!(bad.r#type, "bad request");
        assert_eq!(bad.detail.as_deref(), Some("nope"));

        assert_eq!(ApiProblem::not_found("x").status, 404);
        assert_eq!(ApiProblem::unauthorized().status, 401);
        assert_eq!(ApiProblem::forbidden().status, 403);
        assert_eq!(ApiProblem::conflict("x").status, 409);
        assert_eq!(ApiProblem::server_error("x").status, 500);
        assert_eq!(ApiProblem::validation_error("x").status, 422);

          // `unauthorized`/`forbidden` carry no detail; the others do.
        assert!(ApiProblem::unauthorized().detail.is_none());
        assert!(ApiProblem::forbidden().detail.is_none());
        assert!(ApiProblem::conflict("x").detail.is_some());
      }

      #[test]
    fn api_response_ok_envelope() {
        let resp = ApiResponse::ok(42);
        assert!(resp.ok);
        assert_eq!(resp.data, 42);
      }

      #[test]
    fn page_from_all_slices_and_marks_more() {
        let items: Vec<i32> = (1..=5).collect();

          // Offset 1, limit 2 -> [2, 3]; end (3) < total (5) so `next_offset` set.
        let page = Page::from_all(items.clone(), 2, 1);
        assert_eq!(page.items, vec![2, 3]);
        assert_eq!(page.limit, 2);
        assert_eq!(page.offset, 1);
        assert_eq!(page.next_offset, Some(3));

          // A full page leaves no next offset.
        let page = Page::from_all(items.clone(), 10, 0);
        assert_eq!(page.items.len(), 5);
        assert_eq!(page.next_offset, None);

          // An offset past the end yields an empty page, not a panic.
        let empty = Page::from_all(items, 10, 100);
        assert!(empty.items.is_empty());
      }

     // --- Health ------------------------------------------------------------

     #[tokio::test]
    async fn health_probe_and_missing_route() {
        let service = api_service().await;

          // The health probe returns 200.
        let res = TestClient::get("http://localhost/health").send(&service).await;
        assert_eq!(res.status_code, Some(StatusCode::OK));

          // An unregistered path surfaces as 404 (no root handler is mounted on
          // this router), so the probe is not swallowed by a catch-all.
        let res = TestClient::get("http://localhost/nope").send(&service).await;
        assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
      }

     // --- Domain CRUD -------------------------------------------------------

     #[tokio::test]
    async fn domain_crud_roundtrip() {
        let service = api_service().await;

          // Create.
        let mut res = TestClient::post("http://localhost/domains")
             .json(&serde_json::json!({"id": "example.com"}))
             .send(&service)
             .await;
        assert_eq!(res.status_code, Some(StatusCode::OK));
        let body: ApiResponse<DomainDTO> = res.take_json().await.expect("json body");
        assert!(body.ok);
        let created = body.data;
        assert_eq!(created.id, "example.com");
          // DB-stamped timestamps are present on the projected DTO.
        assert!(!created.created_at.is_empty());
        assert!(!created.updated_at.is_empty());

          // Get the created domain.
        let res = TestClient::get("http://localhost/domains/example.com").send(&service).await;
        assert_eq!(res.status_code, Some(StatusCode::OK));

          // A missing domain 404s.
        let res = TestClient::get("http://localhost/domains/gone.example").send(&service).await;
        assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));

          // Duplicate create 409s.
        let res = TestClient::post("http://localhost/domains")
             .json(&serde_json::json!({"id": "example.com"}))
             .send(&service)
             .await;
        assert_eq!(res.status_code, Some(StatusCode::CONFLICT));

          // Delete succeeds, then the domain is gone.
        let res = TestClient::delete("http://localhost/domains/example.com").send(&service).await;
        assert_eq!(res.status_code, Some(StatusCode::OK));
        let res = TestClient::get("http://localhost/domains/example.com").send(&service).await;
        assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
          // Deleting an already-absent domain still 404s.
        let res = TestClient::delete("http://localhost/domains/example.com").send(&service).await;
        assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
      }

     // --- Mailbox CRUD ------------------------------------------------------

     #[tokio::test]
    async fn mailbox_crud_roundtrip_normalizes_case() {
        let service = api_service().await;

          // Create with mixed case; the stored id is normalized to lowercase.
        let mut res = TestClient::post("http://localhost/mailboxes")
             .json(&serde_json::json!({"id": "alice@example.com"}))
             .send(&service)
             .await;
        assert_eq!(res.status_code, Some(StatusCode::OK));
        let body: ApiResponse<MailboxDTO> = res.take_json().await.expect("json body");
        assert!(body.ok);
        assert_eq!(body.data.id, "alice@example.com");

          // Fetch by the normalized (lowercase) address.
        let res = TestClient::get("http://localhost/mailboxes/alice@example.com").send(&service).await;
        assert_eq!(res.status_code, Some(StatusCode::OK));

          // A duplicate address 409s.
        let res = TestClient::post("http://localhost/mailboxes")
             .json(&serde_json::json!({"id": "alice@example.com"}))
             .send(&service)
             .await;
        assert_eq!(res.status_code, Some(StatusCode::CONFLICT));

          // Patch forward_to.
        let res = TestClient::patch("http://localhost/mailboxes/alice@example.com")
             .json(&serde_json::json!({"forward_to": "external@host.com"}))
             .send(&service)
             .await;
        assert_eq!(res.status_code, Some(StatusCode::OK));

          // Delete, then it is gone.
        let res = TestClient::delete("http://localhost/mailboxes/alice@example.com")
             .send(&service)
             .await;
        assert_eq!(res.status_code, Some(StatusCode::OK));
        let res = TestClient::get("http://localhost/mailboxes/alice@example.com").send(&service).await;
        assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
      }

      #[tokio::test]
    async fn create_mailbox_rejects_address_without_domain() {
        let service = api_service().await;
        let res = TestClient::post("http://localhost/mailboxes")
             .json(&serde_json::json!({"id": "no-domain-part"}))
             .send(&service)
             .await;
        assert_eq!(res.status_code, Some(StatusCode::BAD_REQUEST));
      }

     // --- Send + messages ---------------------------------------------------

      // A helper that provisions a sender mailbox so the send endpoints reach the
      // code paths beyond the "sender must be provisioned" 409 gate.
    async fn provision_sender(service: &salvo::Service, sender: &str) {
        let res = TestClient::post("http://localhost/mailboxes")
             .json(&serde_json::json!({"id": sender}))
             .send(service)
             .await;
        assert_eq!(res.status_code, Some(StatusCode::OK));
      }

      #[tokio::test]
    async fn send_email_enqueues_one_job_per_recipient() {
        let service = api_service().await;
        provision_sender(&service, "sender@example.com").await;

          // One Outbound job per distinct recipient (to + cc).
        let mut res = TestClient::post("http://localhost/mail")
             .json(&serde_json::json!({
                   "from": "sender@example.com",
                   "to": ["bob@ext.com"],
                   "cc": ["carol@ext.com"],
                   "subject": "Hi there",
                   "body": "Hello world"
               }))
             .send(&service)
             .await;
        assert_eq!(res.status_code, Some(StatusCode::OK));
        let result: ApiResponse<SendResult> = res.take_json().await.expect("json body");
        assert!(result.ok);
        let data = result.data;
        assert_eq!(data.outbound_ids.len(), 2, "one Outbound per recipient");

          // The shared Message row is addressable by its id.
        let mut res =
            TestClient::get(&format!("http://localhost/messages/{}", data.message_id)).send(&service).await;
        assert_eq!(res.status_code, Some(StatusCode::OK));
        let msg: ApiResponse<MessageDTO> = res.take_json().await.expect("json body");
        assert_eq!(msg.data.mail_from, "sender@example.com");
      }

      #[tokio::test]
    async fn send_email_requires_a_provisioned_sender() {
        let service = api_service().await;
        // `ghost@example.com` was never provisioned, so the send is rejected even
        // though the recipient is well-formed.
        let res = TestClient::post("http://localhost/mail")
             .json(&serde_json::json!({
                   "from": "ghost@example.com",
                   "to": ["bob@ext.com"],
                   "body": "x"
               }))
             .send(&service)
             .await;
        assert_eq!(res.status_code, Some(StatusCode::CONFLICT));
      }

      #[tokio::test]
    async fn send_email_rejects_missing_recipients() {
        let service = api_service().await;
        let res = TestClient::post("http://localhost/mail")
             .json(&serde_json::json!({
                   "from": "sender@example.com",
                   "to": [],
                   "body": "x"
               }))
             .send(&service)
             .await;
        assert_eq!(res.status_code, Some(StatusCode::BAD_REQUEST));
      }

      #[tokio::test]
    async fn send_email_rejects_malformed_recipient() {
        let service = api_service().await;
          // Provision the sender so we reach recipient parsing, not the 409 gate.
        provision_sender(&service, "sender@example.com").await;
        let res = TestClient::post("http://localhost/mail")
             .json(&serde_json::json!({
                   "from": "sender@example.com",
                   "to": ["not-an-email"],
                   "body": "x"
               }))
             .send(&service)
             .await;
        assert_eq!(res.status_code, Some(StatusCode::BAD_REQUEST));
      }

     // --- Pagination --------------------------------------------------------

     #[tokio::test]
    async fn pagination_limit_query_param_is_honored() {
        let service = api_service().await;
          // `?limit=5` is reflected in the returned page envelope.
        let mut res = TestClient::get("http://localhost/domains?limit=5").send(&service).await;
        assert_eq!(res.status_code, Some(StatusCode::OK));
        let page: ApiResponse<Page<DomainDTO>> = res.take_json().await.expect("json page");
        assert_eq!(page.data.limit, 5);
      }

      #[tokio::test]
    async fn pagination_limit_is_capped_at_max() {
        let service = api_service().await;
        let mut res =
            TestClient::get(&format!("http://localhost/domains?limit={}", MAX_PAGE_LIMIT + 500))
                  .send(&service)
                  .await;
        assert_eq!(res.status_code, Some(StatusCode::OK));
        let page: ApiResponse<Page<DomainDTO>> = res.take_json().await.expect("json page");
        assert_eq!(page.data.limit, MAX_PAGE_LIMIT);
      }


       #[tokio::test]
    async fn message_lifecycle_get_and_delete_by_id() {
        let service = api_service().await;
        provision_sender(&service, "sender@example.com").await;

          // Send one message, capturing its stored id.
        let mut res = TestClient::post("http://localhost/mail")
              .json(&serde_json::json!({
                    "from": "sender@example.com",
                    "to": ["bob@ext.com"],
                    "subject": "Lifecycle",
                    "body": "hello"
                 }))
              .send(&service)
              .await;
        assert_eq!(res.status_code, Some(StatusCode::OK));
        let result: ApiResponse<SendResult> = res.take_json().await.expect("json body");
        let message_id = result.data.message_id;

          // Fetch the message by id.
        let res =
             TestClient::get(&format!("http://localhost/messages/{message_id}")).send(&service).await;
        assert_eq!(res.status_code, Some(StatusCode::OK));

          // An id that was never created 404s.
        let res =
             TestClient::get("http://localhost/messages/999999").send(&service).await;
        assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));

          // Delete it.
        let res =
             TestClient::delete(&format!("http://localhost/messages/{message_id}")).send(&service).await;
        assert_eq!(res.status_code, Some(StatusCode::OK));

          // ...and now it is gone.
        let res =
             TestClient::get(&format!("http://localhost/messages/{message_id}")).send(&service).await;
        assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
       }
}
