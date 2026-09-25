//! Port pool API: port numbers Strom administers and hands out.
//!
//! The pool knows nothing about who is asking or what they do with the
//! numbers. An owner reserves some, renews while it is alive, and optionally
//! tells the pool which of them a given flow uses so they are not reclaimed
//! under a running pipeline.
//!
//! The pool is off until an operator configures ports. A server with none says
//! so rather than hiding: the reservation routes answer `503` with the setting
//! to change in the body, and `GET /api/ports` answers `200` either way with
//! `enabled: false`. A client can tell a Strom that will never hand out ports
//! from one that is briefly unhappy, and from an older Strom with no pool
//! routes at all, which answers `404`.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use strom_types::api::ErrorResponse;
use strom_types::ports::{
    AssignPortsRequest, CreateReservationRequest, PortPoolStatus, PortReservation,
    RenewReservationRequest,
};
use strom_types::FlowId;
use tracing::error;
use uuid::Uuid;

use crate::json_rejection::JsonBody;
use crate::ports::{PortPoolError, Reserved};
use crate::state::AppState;

type ApiError = (StatusCode, Json<ErrorResponse>);

fn pool_error(err: PortPoolError) -> ApiError {
    let status = match err {
        PortPoolError::EmptyOwnerId
        | PortPoolError::BadCount(_)
        | PortPoolError::BadTtl(_)
        | PortPoolError::NotInReservation(_) => StatusCode::BAD_REQUEST,
        PortPoolError::Exhausted { .. } | PortPoolError::AlreadyAssigned { .. } => {
            StatusCode::CONFLICT
        }
        PortPoolError::NotFound => StatusCode::NOT_FOUND,
        // Configuration is unavailable, distinct from allocation conflicts.
        PortPoolError::NotConfigured => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, Json(ErrorResponse::new(err.to_string())))
}

fn storage_error(err: anyhow::Error) -> ApiError {
    error!("Failed to persist port reservations: {}", err);
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::with_details(
            "Failed to persist port reservations",
            err.to_string(),
        )),
    )
}

/// Report the port pool.
#[utoipa::path(
    get,
    path = "/api/ports",
    tag = "ports",
    description = "The configured ports as runs, how many are free, and every port that is \
                   not free with the reservation and flow holding it. Only ports that are \
                   reserved, assigned or blocked are listed, so the response is proportional \
                   to what is interesting rather than to pool size. Unlike the reservation \
                   routes this answers on a server with no pool configured, reporting \
                   `enabled: false`.",
    responses(
        (status = 200, description = "The pool, configured or not", body = PortPoolStatus)
    )
)]
pub async fn get_pool(State(state): State<AppState>) -> Json<PortPoolStatus> {
    Json(state.port_pool_status().await)
}

/// List live reservations.
#[utoipa::path(
    get,
    path = "/api/ports/reservations",
    tag = "ports",
    responses(
        (status = 200, description = "Every reservation that has not lapsed", body = Vec<PortReservation>),
        (status = 503, description = "No port pool is configured on this server", body = ErrorResponse)
    )
)]
pub async fn list_reservations(
    State(state): State<AppState>,
) -> Result<Json<Vec<PortReservation>>, ApiError> {
    state
        .list_port_reservations()
        .await
        .map(Json)
        .map_err(pool_error)
}

/// Reserve ports.
#[utoipa::path(
    post,
    path = "/api/ports/reservations",
    tag = "ports",
    description = "Reserves ports for `owner_id` until `ttl_secs` after the last request or \
                   renewal. The call is idempotent per owner: an owner that already holds a \
                   reservation gets it back, renewed, with a 200 instead of a 201, and its \
                   existing ports are never moved. A `count` larger than it holds adds ports, \
                   preferring ones that continue the block it already has; equal or smaller \
                   changes nothing but the expiry, since ports are given back by deleting the \
                   reservation rather than by shrinking it. `ports` is an explicit list and \
                   callers must not assume it is contiguous.",
    request_body = CreateReservationRequest,
    responses(
        (status = 201, description = "A new reservation was granted", body = PortReservation),
        (status = 200, description = "The owner's existing reservation, renewed", body = PortReservation),
        (status = 400, description = "Invalid owner_id, count or ttl_secs", body = ErrorResponse),
        (status = 409, description = "Not enough free ports", body = ErrorResponse),
        (status = 503, description = "No port pool is configured on this server", body = ErrorResponse),
        (status = 500, description = "Reservation could not be persisted", body = ErrorResponse)
    )
)]
pub async fn create_reservation(
    State(state): State<AppState>,
    JsonBody(req): JsonBody<CreateReservationRequest>,
) -> Result<(StatusCode, Json<PortReservation>), ApiError> {
    let outcome = state
        .reserve_ports(&req.owner_id, req.count, req.ttl_secs)
        .await
        .map_err(storage_error)?
        .map_err(pool_error)?;
    let status = match outcome.how {
        Reserved::Created => StatusCode::CREATED,
        Reserved::Renewed => StatusCode::OK,
    };
    Ok((status, Json(outcome.reservation)))
}

/// Get one reservation.
#[utoipa::path(
    get,
    path = "/api/ports/reservations/{id}",
    tag = "ports",
    params(("id" = Uuid, Path, description = "Reservation id")),
    responses(
        (status = 200, description = "The reservation", body = PortReservation),
        (status = 404, description = "No live reservation with that id", body = ErrorResponse),
        (status = 503, description = "No port pool is configured on this server", body = ErrorResponse)
    )
)]
pub async fn get_reservation(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<PortReservation>, ApiError> {
    state
        .get_port_reservation(id)
        .await
        .map(Json)
        .map_err(pool_error)
}

/// Extend a reservation.
#[utoipa::path(
    post,
    path = "/api/ports/reservations/{id}/renew",
    tag = "ports",
    description = "Moves the reservation's expiry to now plus `ttl_secs`. A reservation that \
                   has already lapsed is gone: request it again with the same `owner_id` \
                   through `POST /api/ports/reservations` instead.",
    params(("id" = Uuid, Path, description = "Reservation id")),
    request_body(content = RenewReservationRequest, description = "Optional new lifetime"),
    responses(
        (status = 200, description = "The renewed reservation", body = PortReservation),
        (status = 400, description = "Invalid ttl_secs", body = ErrorResponse),
        (status = 404, description = "No live reservation with that id", body = ErrorResponse),
        (status = 503, description = "No port pool is configured on this server", body = ErrorResponse),
        (status = 500, description = "Reservation could not be persisted", body = ErrorResponse)
    )
)]
pub async fn renew_reservation(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Bytes,
) -> Result<Json<PortReservation>, ApiError> {
    // The body is optional: an empty request means the configured lifetime.
    let req: RenewReservationRequest = if body.iter().all(u8::is_ascii_whitespace) {
        RenewReservationRequest::default()
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
    state
        .renew_port_reservation(id, req.ttl_secs)
        .await
        .map_err(storage_error)?
        .map_err(pool_error)
        .map(Json)
}

/// Release a reservation.
#[utoipa::path(
    delete,
    path = "/api/ports/reservations/{id}",
    tag = "ports",
    description = "Gives the ports back. Ports the owner has declared in use by a flow that \
                   still exists stay held until that flow is gone — a port returns to the \
                   pool only when no live reservation and no existing flow holds it.",
    params(("id" = Uuid, Path, description = "Reservation id")),
    responses(
        (status = 204, description = "Reservation released"),
        (status = 404, description = "No live reservation with that id", body = ErrorResponse),
        (status = 503, description = "No port pool is configured on this server", body = ErrorResponse),
        (status = 500, description = "Reservation could not be persisted", body = ErrorResponse)
    )
)]
pub async fn delete_reservation(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    state
        .release_port_reservation(id)
        .await
        .map_err(storage_error)?
        .map_err(pool_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Declare which ports a flow uses.
#[utoipa::path(
    post,
    path = "/api/ports/reservations/{id}/assign",
    tag = "ports",
    description = "Records that these ports of this reservation are used by that flow. The \
                   pool will not return them while the flow exists, so a reservation that \
                   lapses under a running pipeline releases only its idle ports. Replaces \
                   whatever was recorded for the same flow, so a caller can correct itself \
                   by assigning again. Nothing verifies that the flow actually binds them.",
    params(("id" = Uuid, Path, description = "Reservation id")),
    request_body = AssignPortsRequest,
    responses(
        (status = 200, description = "The reservation", body = PortReservation),
        (status = 400, description = "A port does not belong to this reservation", body = ErrorResponse),
        (status = 404, description = "No live reservation with that id", body = ErrorResponse),
        (status = 409, description = "A port is already assigned to another flow", body = ErrorResponse),
        (status = 503, description = "No port pool is configured on this server", body = ErrorResponse),
        (status = 500, description = "Reservation could not be persisted", body = ErrorResponse)
    )
)]
pub async fn assign_ports(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<AssignPortsRequest>,
) -> Result<Json<PortReservation>, ApiError> {
    state
        .assign_ports(id, req.flow_id, &req.ports)
        .await
        .map_err(storage_error)?
        .map_err(pool_error)
        .map(Json)
}

/// Drop a flow's declaration.
#[utoipa::path(
    delete,
    path = "/api/ports/reservations/{id}/assign/{flow_id}",
    tag = "ports",
    description = "The ports go back to the reservation, never to the pool: an owner keeps \
                   its ports across any amount of flow churn.",
    params(
        ("id" = Uuid, Path, description = "Reservation id"),
        ("flow_id" = Uuid, Path, description = "Flow id")
    ),
    responses(
        (status = 204, description = "Declaration dropped"),
        (status = 404, description = "No such reservation, or no ports declared for that flow", body = ErrorResponse),
        (status = 503, description = "No port pool is configured on this server", body = ErrorResponse),
        (status = 500, description = "Reservation could not be persisted", body = ErrorResponse)
    )
)]
pub async fn unassign_ports(
    State(state): State<AppState>,
    Path((id, flow_id)): Path<(Uuid, FlowId)>,
) -> Result<StatusCode, ApiError> {
    state
        .unassign_ports(id, flow_id)
        .await
        .map_err(storage_error)?
        .map_err(pool_error)?;
    Ok(StatusCode::NO_CONTENT)
}
