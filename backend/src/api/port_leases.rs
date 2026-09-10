//! Port lease API: blocks of UDP ports reserved for external orchestrators.
//!
//! A Strom shared by several Open Live instances is the only party that can
//! keep their SRT listener ports apart. Each instance asks for a block under a
//! stable `client_id`, renews it while alive, and lets it lapse when gone.
//! Strom does not bind the ports; it promises not to hand the block to anyone
//! else, and never allocates a port an existing flow already listens on.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use strom_types::api::ErrorResponse;
use strom_types::port_lease::{PortLease, PortLeaseRequest, RenewPortLeaseRequest};
use tracing::error;
use uuid::Uuid;

use crate::json_rejection::JsonBody;
use crate::port_lease::{Acquired, PortLeaseError};
use crate::state::AppState;

type ApiError = (StatusCode, Json<ErrorResponse>);

fn lease_error(err: PortLeaseError) -> ApiError {
    let status = match err {
        PortLeaseError::EmptyClientId | PortLeaseError::BadSize(_) | PortLeaseError::BadTtl(_) => {
            StatusCode::BAD_REQUEST
        }
        PortLeaseError::Exhausted { .. } => StatusCode::CONFLICT,
        PortLeaseError::NotFound => StatusCode::NOT_FOUND,
    };
    (status, Json(ErrorResponse::new(err.to_string())))
}

fn storage_error(err: anyhow::Error) -> ApiError {
    error!("Failed to persist port leases: {}", err);
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::with_details(
            "Failed to persist port leases",
            err.to_string(),
        )),
    )
}

/// List live port leases.
#[utoipa::path(
    get,
    path = "/api/port-leases",
    tag = "port-leases",
    responses(
        (status = 200, description = "Every lease that has not expired", body = Vec<PortLease>)
    )
)]
pub async fn list_port_leases(State(state): State<AppState>) -> Json<Vec<PortLease>> {
    Json(state.list_port_leases().await)
}

/// Request a block of ports.
#[utoipa::path(
    post,
    path = "/api/port-leases",
    tag = "port-leases",
    description = "Reserves `size` consecutive UDP ports from the server's lease pool for \
                   `client_id`. The call is idempotent per client: a client that already \
                   holds a lease gets it back, renewed, with a 200 instead of a 201, and \
                   a changed `size` resizes the block in place when the neighbouring ports \
                   are free. Ports an existing flow listens on are never handed out, even \
                   when no lease covers them. The lease lapses `ttl_secs` after the last \
                   request or renewal (default 600).",
    request_body = PortLeaseRequest,
    responses(
        (status = 201, description = "A new lease was granted", body = PortLease),
        (status = 200, description = "The client's existing lease, renewed", body = PortLease),
        (status = 400, description = "Invalid client_id, size or ttl_secs", body = ErrorResponse),
        (status = 409, description = "No block of that size is free in the pool", body = ErrorResponse),
        (status = 500, description = "Lease could not be persisted", body = ErrorResponse)
    )
)]
pub async fn create_port_lease(
    State(state): State<AppState>,
    JsonBody(req): JsonBody<PortLeaseRequest>,
) -> Result<(StatusCode, Json<PortLease>), ApiError> {
    let (lease, how) = state
        .acquire_port_lease(&req.client_id, req.size, req.ttl_secs)
        .await
        .map_err(storage_error)?
        .map_err(lease_error)?;
    let status = match how {
        Acquired::Created => StatusCode::CREATED,
        Acquired::Renewed => StatusCode::OK,
    };
    Ok((status, Json(lease)))
}

/// Get one port lease.
#[utoipa::path(
    get,
    path = "/api/port-leases/{id}",
    tag = "port-leases",
    params(("id" = Uuid, Path, description = "Lease id")),
    responses(
        (status = 200, description = "The lease", body = PortLease),
        (status = 404, description = "No live lease with that id", body = ErrorResponse)
    )
)]
pub async fn get_port_lease(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<PortLease>, ApiError> {
    state
        .get_port_lease(id)
        .await
        .map(Json)
        .ok_or_else(|| lease_error(PortLeaseError::NotFound))
}

/// Extend a port lease.
#[utoipa::path(
    post,
    path = "/api/port-leases/{id}/renew",
    tag = "port-leases",
    description = "Moves the lease's expiry to now plus `ttl_secs` (default 600). A lease \
                   that has already lapsed is gone: re-request it with the same `client_id` \
                   through `POST /api/port-leases` instead.",
    params(("id" = Uuid, Path, description = "Lease id")),
    request_body(content = RenewPortLeaseRequest, description = "Optional new lifetime"),
    responses(
        (status = 200, description = "The renewed lease", body = PortLease),
        (status = 400, description = "Invalid ttl_secs", body = ErrorResponse),
        (status = 404, description = "No live lease with that id", body = ErrorResponse),
        (status = 500, description = "Lease could not be persisted", body = ErrorResponse)
    )
)]
pub async fn renew_port_lease(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Bytes,
) -> Result<Json<PortLease>, ApiError> {
    // The body is optional: an empty request means "the default lifetime".
    let req: RenewPortLeaseRequest = if body.iter().all(u8::is_ascii_whitespace) {
        RenewPortLeaseRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Invalid JSON body",
                    e.to_string(),
                )),
            )
        })?
    };
    let ttl_secs = req.ttl_secs;
    state
        .renew_port_lease(id, ttl_secs)
        .await
        .map_err(storage_error)?
        .map_err(lease_error)
        .map(Json)
}

/// Release a port lease.
#[utoipa::path(
    delete,
    path = "/api/port-leases/{id}",
    tag = "port-leases",
    params(("id" = Uuid, Path, description = "Lease id")),
    responses(
        (status = 204, description = "Lease released"),
        (status = 404, description = "No live lease with that id", body = ErrorResponse),
        (status = 500, description = "Lease could not be persisted", body = ErrorResponse)
    )
)]
pub async fn delete_port_lease(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    state
        .release_port_lease(id)
        .await
        .map_err(storage_error)?
        .map_err(lease_error)?;
    Ok(StatusCode::NO_CONTENT)
}
