use std::{env, net::SocketAddr};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use clawmesh_p2p::{
    AnnounceMessage, GossipMessage, MeshNode, PeerEntry, PeerStatus, RoomInfo, SignalMessage,
    StateDelta, StateEntry, StateSnapshot,
};
use opentelemetry::{global, trace::TracerProvider, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_http::trace::TraceLayer;
use tracing::{error, info, instrument};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

const PROTOCOL_VERSION: &str = "0.1";
const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8080";

#[derive(Clone)]
struct AppState {
    node: MeshNode,
}

#[derive(Debug, Deserialize, Serialize)]
struct Envelope {
    schema_version: String,
    intent: Intent,
    capability: String,
    policy: Policy,
    timestamp: DateTime<Utc>,
    sender: Sender,
    payload: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Intent {
    Announce,
    RequestMatch,
    Schedule,
    Offer,
}

#[derive(Debug, Deserialize, Serialize)]
enum Policy {
    #[serde(rename = "public")]
    Public,
    #[serde(rename = "mesh-only")]
    MeshOnly,
    #[serde(rename = "private")]
    Private,
}

#[derive(Debug, Deserialize, Serialize)]
struct Sender {
    agent_id: String,
    mesh_id: String,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
}

#[derive(Debug, Serialize)]
struct RouteResponse {
    status: &'static str,
    intent: Intent,
    route: &'static str,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: &'static str,
    detail: String,
}

#[derive(Debug, Serialize)]
struct RelayStatusResponse {
    status: &'static str,
    registered_peers: usize,
}

#[derive(Debug, Serialize)]
struct GossipStatusResponse {
    node_id: String,
    connected_peers: usize,
    registry_size: usize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing("clawmesh-gateway");

    let bind_addr = env::var("GATEWAY_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string());
    let mesh_id = env::var("MESH_ID").unwrap_or_else(|_| "mesh:default".to_string());
    let node_id = env::var("NODE_ID").unwrap_or_else(|_| "node:gateway-0".to_string());
    let socket_addr: SocketAddr = bind_addr.parse()?;

    let node = MeshNode::new(&node_id, &mesh_id)
        .expect("failed to generate mesh identity");
    let state = AppState { node };

    let app = Router::new()
        .route("/healthz", get(health))
        // Envelope dispatch
        .route("/v0/envelope", post(route_envelope))
        .route("/v0/peers", get(list_peers))
        // Signal relay
        .route("/v0/signal/relay", post(relay_signal))
        .route("/v0/signal/status", get(relay_status))
        // Gossip
        .route("/v0/gossip/exchange", post(gossip_exchange))
        .route("/v0/gossip/status", get(gossip_status))
        // Rooms
        .route("/v0/rooms", get(list_rooms).post(create_room))
        .route("/v0/rooms/{room_id}/join", post(join_room))
        .route("/v0/rooms/{room_id}/leave", post(leave_room))
        .route("/v0/rooms/{room_id}/members", get(room_members))
        // Room state
        .route("/v0/rooms/{room_id}/state", get(list_state).post(set_state))
        .route("/v0/rooms/{room_id}/state/{key}", get(get_state))
        .route("/v0/rooms/{room_id}/state/delta", post(apply_delta))
        .route("/v0/rooms/{room_id}/state/snapshot", get(get_snapshot).post(restore_snapshot))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(socket_addr).await?;
    info!(%socket_addr, %mesh_id, %node_id, "gateway listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    global::shutdown_tracer_provider();
    Ok(())
}

fn init_tracing(service_name: &str) {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("gateway=info,tower_http=info"));
    let fmt_layer = tracing_subscriber::fmt::layer().compact().with_target(false);

    if let Ok(endpoint) = env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
        let provider = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_trace_config(
                opentelemetry_sdk::trace::Config::default()
                    .with_resource(Resource::new(vec![KeyValue::new(
                        "service.name",
                        service_name.to_owned(),
                    )])),
            )
            .with_exporter(opentelemetry_otlp::new_exporter().tonic().with_endpoint(endpoint))
            .install_batch(opentelemetry_sdk::runtime::Tokio);

        match provider {
            Ok(provider) => {
                let tracer = provider.tracer(service_name.to_owned());
                global::set_tracer_provider(provider);
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(fmt_layer)
                    .with(tracing_opentelemetry::layer().with_tracer(tracer))
                    .init();
                info!("tracing initialized with OpenTelemetry OTLP exporter");
                return;
            }
            Err(error) => {
                eprintln!("failed to initialize OTLP exporter: {error}");
            }
        }
    }

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .init();
    info!("tracing initialized (stdout only)");
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "clawmesh-gateway",
    })
}

// ---------------------------------------------------------------------------
// Envelope dispatch
// ---------------------------------------------------------------------------

#[instrument(skip(state, envelope))]
async fn dispatch_intent(state: &AppState, envelope: &Envelope) -> RouteResponse {
    match envelope.intent {
        Intent::Announce => route_announce(state, envelope).await,
        Intent::RequestMatch => route_request_match(envelope).await,
        Intent::Schedule => route_schedule(envelope).await,
        Intent::Offer => route_offer(envelope).await,
    }
}

#[instrument(skip(state, envelope))]
async fn route_announce(state: &AppState, envelope: &Envelope) -> RouteResponse {
    info!(capability = %envelope.capability, "routing announce envelope");

    let display_name = envelope
        .payload
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let status_str = envelope
        .payload
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("available");

    let status = match status_str {
        "busy" => PeerStatus::Busy,
        "away" => PeerStatus::Away,
        _ => PeerStatus::Available,
    };

    let supports = envelope
        .payload
        .get("supports")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let msg = AnnounceMessage {
        agent_id: envelope.sender.agent_id.clone(),
        mesh_id: envelope.sender.mesh_id.clone(),
        display_name,
        status,
        capabilities: supports,
    };

    state.node.registry().handle_announce(msg).await;

    // Auto-register the announcing peer with the signal relay
    state
        .node
        .relay()
        .register(&envelope.sender.agent_id, 64)
        .await;

    RouteResponse {
        status: "accepted",
        intent: Intent::Announce,
        route: "announce_router",
    }
}

#[instrument(skip(envelope))]
async fn route_request_match(envelope: &Envelope) -> RouteResponse {
    info!(capability = %envelope.capability, "routing request_match envelope");
    RouteResponse {
        status: "stub",
        intent: Intent::RequestMatch,
        route: "request_match_router",
    }
}

#[instrument(skip(envelope))]
async fn route_schedule(envelope: &Envelope) -> RouteResponse {
    info!(capability = %envelope.capability, "routing schedule envelope");
    RouteResponse {
        status: "stub",
        intent: Intent::Schedule,
        route: "schedule_router",
    }
}

#[instrument(skip(envelope))]
async fn route_offer(envelope: &Envelope) -> RouteResponse {
    info!(capability = %envelope.capability, "routing offer envelope");
    RouteResponse {
        status: "stub",
        intent: Intent::Offer,
        route: "offer_router",
    }
}

#[instrument(skip(state, envelope))]
async fn route_envelope(
    State(state): State<AppState>,
    Json(envelope): Json<Envelope>,
) -> Result<(StatusCode, Json<RouteResponse>), (StatusCode, Json<ErrorResponse>)> {
    if envelope.schema_version != PROTOCOL_VERSION {
        let detail = format!(
            "unsupported schema_version '{}', expected '{}'",
            envelope.schema_version, PROTOCOL_VERSION
        );
        error!(%detail, "envelope rejected");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "invalid_schema_version",
                detail,
            }),
        ));
    }

    let response = dispatch_intent(&state, &envelope).await;
    Ok((StatusCode::ACCEPTED, Json(response)))
}

async fn list_peers(State(state): State<AppState>) -> Json<Vec<PeerEntry>> {
    Json(state.node.registry().list_peers().await)
}

// ---------------------------------------------------------------------------
// Signal relay endpoints
// ---------------------------------------------------------------------------

#[instrument(skip(state, msg))]
async fn relay_signal(
    State(state): State<AppState>,
    Json(msg): Json<SignalMessage>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    state.node.relay().relay(msg).await.map_err(|e| {
        let detail = e.to_string();
        error!(%detail, "signal relay failed");
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "relay_failed",
                detail,
            }),
        )
    })?;
    Ok(StatusCode::ACCEPTED)
}

async fn relay_status(State(state): State<AppState>) -> Json<RelayStatusResponse> {
    Json(RelayStatusResponse {
        status: "ok",
        registered_peers: state.node.relay().peer_count().await,
    })
}

// ---------------------------------------------------------------------------
// Gossip exchange endpoints
// ---------------------------------------------------------------------------

#[instrument(skip(state, body))]
async fn gossip_exchange(
    State(state): State<AppState>,
    Json(body): Json<GossipExchangeRequest>,
) -> Result<Json<GossipExchangeResponse>, (StatusCode, Json<ErrorResponse>)> {
    match body.message {
        GossipMessage::Pull(pull) => {
            let from = pull.from.clone();
            let push = state.node.registry().handle_gossip_pull(pull).await;
            let entry_count = push.entries.len();
            info!(%from, entries = entry_count, "handled gossip pull from remote node");
            Ok(Json(GossipExchangeResponse {
                status: "ok",
                message: Some(GossipMessage::Push(push)),
            }))
        }
        GossipMessage::Push(push) => {
            let entry_count = push.entries.len();
            state.node.registry().merge_gossip_push(push).await;
            info!(entries = entry_count, "merged gossip push from remote node");
            Ok(Json(GossipExchangeResponse {
                status: "ok",
                message: None,
            }))
        }
    }
}

#[derive(Debug, Deserialize)]
struct GossipExchangeRequest {
    message: GossipMessage,
}

#[derive(Debug, Serialize)]
struct GossipExchangeResponse {
    status: &'static str,
    message: Option<GossipMessage>,
}

async fn gossip_status(State(state): State<AppState>) -> Json<GossipStatusResponse> {
    Json(GossipStatusResponse {
        node_id: state.node.gossip().node_id().to_string(),
        connected_peers: state.node.gossip().peer_count().await,
        registry_size: state.node.registry().peer_count().await,
    })
}

// ---------------------------------------------------------------------------
// Room endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CreateRoomRequest {
    room_id: String,
    name: String,
    max_members: Option<usize>,
}

#[derive(Debug, Serialize)]
struct RoomResponse {
    status: &'static str,
    room: Option<RoomInfo>,
}

/// POST /v0/rooms — Create a new room.
#[instrument(skip(state, body))]
async fn create_room(
    State(state): State<AppState>,
    Json(body): Json<CreateRoomRequest>,
) -> Result<(StatusCode, Json<RoomResponse>), (StatusCode, Json<ErrorResponse>)> {
    let room = state
        .node
        .rooms()
        .create_room(&body.room_id, &body.name, body.max_members)
        .await;

    match room {
        Some(info) => {
            // Auto-init state tracking for the new room.
            state.node.state().init_room(&body.room_id).await;
            info!(room_id = %body.room_id, "room created");
            Ok((
                StatusCode::CREATED,
                Json(RoomResponse {
                    status: "created",
                    room: Some(info),
                }),
            ))
        }
        None => Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: "room_exists",
                detail: format!("room {} already exists", body.room_id),
            }),
        )),
    }
}

/// GET /v0/rooms — List all rooms.
async fn list_rooms(State(state): State<AppState>) -> Json<Vec<RoomInfo>> {
    Json(state.node.rooms().list_rooms().await)
}

#[derive(Debug, Deserialize)]
struct JoinLeaveRequest {
    agent_id: String,
}

/// POST /v0/rooms/:room_id/join — Join a room.
#[instrument(skip(state, body))]
async fn join_room(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    Json(body): Json<JoinLeaveRequest>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    state
        .node
        .rooms()
        .join_room(&room_id, &body.agent_id)
        .await
        .map_err(|e| {
            let detail = e.to_string();
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "join_failed",
                    detail,
                }),
            )
        })?;
    Ok(StatusCode::OK)
}

/// POST /v0/rooms/:room_id/leave — Leave a room.
#[instrument(skip(state, body))]
async fn leave_room(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    Json(body): Json<JoinLeaveRequest>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    state
        .node
        .rooms()
        .leave_room(&room_id, &body.agent_id)
        .await
        .map_err(|e| {
            let detail = e.to_string();
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "leave_failed",
                    detail,
                }),
            )
        })?;
    Ok(StatusCode::OK)
}

/// GET /v0/rooms/:room_id/members — List room members.
async fn room_members(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
) -> Result<Json<Vec<String>>, (StatusCode, Json<ErrorResponse>)> {
    state
        .node
        .rooms()
        .room_members(&room_id)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "room_not_found",
                    detail: e.to_string(),
                }),
            )
        })
}

// ---------------------------------------------------------------------------
// Room state endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SetStateRequest {
    agent_id: String,
    key: String,
    value: Value,
}

/// POST /v0/rooms/:room_id/state — Set a state key.
#[instrument(skip(state, body))]
async fn set_state(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    Json(body): Json<SetStateRequest>,
) -> Result<(StatusCode, Json<StateEntry>), (StatusCode, Json<ErrorResponse>)> {
    state
        .node
        .state()
        .set(&room_id, &body.agent_id, &body.key, body.value)
        .await
        .map(|entry| (StatusCode::OK, Json(entry)))
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "state_error",
                    detail: e.to_string(),
                }),
            )
        })
}

/// GET /v0/rooms/:room_id/state — List all state entries.
async fn list_state(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
) -> Result<Json<Vec<StateEntry>>, (StatusCode, Json<ErrorResponse>)> {
    state
        .node
        .state()
        .list_entries(&room_id)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "state_error",
                    detail: e.to_string(),
                }),
            )
        })
}

/// GET /v0/rooms/:room_id/state/:key — Get a single state entry.
async fn get_state(
    State(state): State<AppState>,
    Path((room_id, key)): Path<(String, String)>,
) -> Result<Json<StateEntry>, (StatusCode, Json<ErrorResponse>)> {
    state
        .node
        .state()
        .get(&room_id, &key)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "state_error",
                    detail: e.to_string(),
                }),
            )
        })
}

/// POST /v0/rooms/:room_id/state/delta — Apply a state delta from a remote peer.
#[instrument(skip(state, body))]
async fn apply_delta(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    Json(body): Json<StateDelta>,
) -> Result<Json<Vec<String>>, (StatusCode, Json<ErrorResponse>)> {
    if body.room_id != room_id {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "room_id_mismatch",
                detail: format!(
                    "path room_id '{}' does not match delta room_id '{}'",
                    room_id, body.room_id
                ),
            }),
        ));
    }

    state
        .node
        .state()
        .apply_delta(body)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "state_error",
                    detail: e.to_string(),
                }),
            )
        })
}

/// GET /v0/rooms/:room_id/state/snapshot — Get a full state snapshot.
async fn get_snapshot(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
) -> Result<Json<StateSnapshot>, (StatusCode, Json<ErrorResponse>)> {
    state
        .node
        .state()
        .snapshot(&room_id)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "state_error",
                    detail: e.to_string(),
                }),
            )
        })
}

/// POST /v0/rooms/:room_id/state/snapshot — Restore state from a snapshot.
#[instrument(skip(state, body))]
async fn restore_snapshot(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    Json(body): Json<StateSnapshot>,
) -> Result<Json<Vec<String>>, (StatusCode, Json<ErrorResponse>)> {
    if body.room_id != room_id {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "room_id_mismatch",
                detail: format!(
                    "path room_id '{}' does not match snapshot room_id '{}'",
                    room_id, body.room_id
                ),
            }),
        ));
    }

    state
        .node
        .state()
        .restore_snapshot(body)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "state_error",
                    detail: e.to_string(),
                }),
            )
        })
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_ok() {
        info!("shutdown signal received");
    }
}
