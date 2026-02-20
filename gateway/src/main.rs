use std::{env, net::SocketAddr};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use clawmesh_p2p::{
    AnnounceMessage, GossipMessage, MatchRequest, MeshNode, PeerEntry, PeerOffer, PeerQuery,
    PeerStatus, RoomInfo, ScheduleSlot, SignalMessage, SkillRange, StateDelta, StateEntry,
    StateSnapshot, TimeWindow,
};
use opentelemetry::{global, trace::TracerProvider, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_http::trace::TraceLayer;
use tracing::{error, info, instrument, warn};
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
    /// Optional Ed25519 signature over the canonical JSON payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Intent {
    Announce,
    RequestMatch,
    Schedule,
    Offer,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
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

    let node = MeshNode::new(&node_id, &mesh_id).expect("failed to generate mesh identity");
    let state = AppState { node };

    let app = Router::new()
        .route("/healthz", get(health))
        // Envelope dispatch
        .route("/v0/envelope", post(route_envelope))
        .route("/v0/peers", get(list_peers))
        .route("/v0/peers/search", get(search_peers))
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
        .route(
            "/v0/rooms/{room_id}/state",
            get(list_state).post(set_state),
        )
        .route("/v0/rooms/{room_id}/state/{key}", get(get_state))
        .route("/v0/rooms/{room_id}/state/delta", post(apply_delta))
        .route(
            "/v0/rooms/{room_id}/state/snapshot",
            get(get_snapshot).post(restore_snapshot),
        )
        // Matchmaking
        .route(
            "/v0/matchmaking/requests",
            get(list_match_requests).post(submit_match_request),
        )
        .route(
            "/v0/matchmaking/requests/{request_id}",
            get(get_match_request),
        )
        .route(
            "/v0/matchmaking/requests/{request_id}/matches",
            get(find_matches),
        )
        .route(
            "/v0/matchmaking/requests/{request_id}/cancel",
            post(cancel_match_request),
        )
        // Scheduling
        .route(
            "/v0/matchmaking/schedules",
            get(list_schedules).post(propose_schedule),
        )
        .route(
            "/v0/matchmaking/schedules/{schedule_id}/confirm",
            post(confirm_schedule),
        )
        .route(
            "/v0/matchmaking/schedules/{schedule_id}/decline",
            post(decline_schedule),
        )
        // Offers
        .route(
            "/v0/matchmaking/offers",
            get(list_offers).post(submit_offer),
        )
        .route("/v0/matchmaking/offers/{offer_id}", get(get_offer))
        .route(
            "/v0/matchmaking/offers/{offer_id}/withdraw",
            post(withdraw_offer),
        )
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
                opentelemetry_sdk::trace::Config::default().with_resource(Resource::new(vec![
                    KeyValue::new("service.name", service_name.to_owned()),
                ])),
            )
            .with_exporter(
                opentelemetry_otlp::new_exporter()
                    .tonic()
                    .with_endpoint(endpoint),
            )
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
// Policy enforcement
// ---------------------------------------------------------------------------

/// Enforce envelope policy constraints.
///
/// - `public`: accepted from any sender.
/// - `mesh-only`: sender.mesh_id must match the gateway's mesh_id.
/// - `private`: sender must be a registered (announced) peer in the registry.
async fn enforce_policy(
    state: &AppState,
    envelope: &Envelope,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    match envelope.policy {
        Policy::Public => Ok(()),
        Policy::MeshOnly => {
            if envelope.sender.mesh_id != state.node.mesh_id() {
                let detail = format!(
                    "mesh-only policy: sender mesh '{}' does not match gateway mesh '{}'",
                    envelope.sender.mesh_id,
                    state.node.mesh_id()
                );
                warn!(%detail, "policy rejected");
                return Err((
                    StatusCode::FORBIDDEN,
                    Json(ErrorResponse {
                        error: "policy_violation",
                        detail,
                    }),
                ));
            }
            Ok(())
        }
        Policy::Private => {
            let peer = state
                .node
                .registry()
                .get_peer(&envelope.sender.agent_id)
                .await;
            if peer.is_none() {
                let detail = format!(
                    "private policy: sender '{}' is not a registered peer",
                    envelope.sender.agent_id
                );
                warn!(%detail, "policy rejected");
                return Err((
                    StatusCode::FORBIDDEN,
                    Json(ErrorResponse {
                        error: "policy_violation",
                        detail,
                    }),
                ));
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Envelope dispatch
// ---------------------------------------------------------------------------

#[instrument(skip(state, envelope))]
async fn dispatch_intent(state: &AppState, envelope: &Envelope) -> RouteResponse {
    match envelope.intent {
        Intent::Announce => route_announce(state, envelope).await,
        Intent::RequestMatch => route_request_match(state, envelope).await,
        Intent::Schedule => route_schedule(state, envelope).await,
        Intent::Offer => route_offer(state, envelope).await,
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

#[instrument(skip(state, envelope))]
async fn route_request_match(state: &AppState, envelope: &Envelope) -> RouteResponse {
    info!(capability = %envelope.capability, "routing request_match envelope");

    let game = envelope
        .payload
        .get("game")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let skill_min = envelope
        .payload
        .get("skill_range")
        .and_then(|v| v.get("min"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let skill_max = envelope
        .payload
        .get("skill_range")
        .and_then(|v| v.get("max"))
        .and_then(|v| v.as_f64())
        .unwrap_or(100.0);

    let time_start = envelope
        .payload
        .get("time_window")
        .and_then(|v| v.get("start"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(Utc::now);

    let time_end = envelope
        .payload
        .get("time_window")
        .and_then(|v| v.get("end"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(Utc::now);

    state
        .node
        .matchmaker()
        .submit_request(
            &envelope.sender.agent_id,
            &game,
            SkillRange {
                min: skill_min,
                max: skill_max,
            },
            TimeWindow {
                start: time_start,
                end: time_end,
            },
        )
        .await;

    RouteResponse {
        status: "accepted",
        intent: Intent::RequestMatch,
        route: "request_match_router",
    }
}

#[instrument(skip(state, envelope))]
async fn route_schedule(state: &AppState, envelope: &Envelope) -> RouteResponse {
    info!(capability = %envelope.capability, "routing schedule envelope");

    let slot_start = envelope
        .payload
        .get("slot_start")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(Utc::now);

    let slot_end = envelope
        .payload
        .get("slot_end")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(Utc::now);

    let slot_state = envelope
        .payload
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("proposed");

    let slot = state
        .node
        .matchmaker()
        .propose_slot(&envelope.sender.agent_id, slot_start, slot_end)
        .await;

    // If the envelope says "confirmed" or "declined", update accordingly.
    match slot_state {
        "confirmed" => {
            let _ = state
                .node
                .matchmaker()
                .confirm_slot(&slot.schedule_id)
                .await;
        }
        "declined" => {
            let _ = state
                .node
                .matchmaker()
                .decline_slot(&slot.schedule_id)
                .await;
        }
        _ => {}
    }

    RouteResponse {
        status: "accepted",
        intent: Intent::Schedule,
        route: "schedule_router",
    }
}

#[instrument(skip(state, envelope))]
async fn route_offer(state: &AppState, envelope: &Envelope) -> RouteResponse {
    info!(capability = %envelope.capability, "routing offer envelope");

    let offer_id = envelope
        .payload
        .get("offer_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let summary = envelope
        .payload
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let expires_at = envelope
        .payload
        .get("expires_at")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(Utc::now);

    state
        .node
        .matchmaker()
        .submit_offer(&envelope.sender.agent_id, &offer_id, &summary, expires_at)
        .await;

    RouteResponse {
        status: "accepted",
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

    // Enforce policy before dispatching.
    enforce_policy(&state, &envelope).await?;

    // Verify signature if provided.
    if let Some(ref sig_str) = envelope.signature {
        let peer = state
            .node
            .auth()
            .get_authenticated(&envelope.sender.agent_id)
            .await;
        if let Some(authenticated_peer) = peer {
            let payload_bytes = serde_json::to_vec(&envelope.payload).unwrap_or_default();
            let sig = clawmesh_p2p::Signature {
                sig: sig_str.clone(),
            };
            if clawmesh_p2p::crypto::verify(&authenticated_peer.public_key, &payload_bytes, &sig)
                .is_err()
            {
                let detail = "envelope signature verification failed".to_string();
                error!(%detail, agent_id = %envelope.sender.agent_id, "signature rejected");
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(ErrorResponse {
                        error: "signature_invalid",
                        detail,
                    }),
                ));
            }
            info!(agent_id = %envelope.sender.agent_id, "envelope signature verified");
        } else {
            // Signature provided but peer not authenticated — log warning, accept anyway.
            // Full enforcement would reject here, but for v0.1 we accept unsigned/unverified.
            warn!(
                agent_id = %envelope.sender.agent_id,
                "signature provided but peer not authenticated; skipping verification"
            );
        }
    }

    let response = dispatch_intent(&state, &envelope).await;
    Ok((StatusCode::ACCEPTED, Json(response)))
}

async fn list_peers(State(state): State<AppState>) -> Json<Vec<PeerEntry>> {
    Json(state.node.registry().list_peers().await)
}

/// GET /v0/peers/search — Search peers by capability, status, or mesh_id.
#[derive(Debug, Deserialize)]
struct PeerSearchQuery {
    capability: Option<String>,
    status: Option<String>,
    mesh_id: Option<String>,
}

async fn search_peers(
    State(state): State<AppState>,
    Query(params): Query<PeerSearchQuery>,
) -> Json<Vec<PeerEntry>> {
    let status = params.status.as_deref().map(|s| match s {
        "busy" => PeerStatus::Busy,
        "away" => PeerStatus::Away,
        _ => PeerStatus::Available,
    });

    let query = PeerQuery {
        status,
        capability: params.capability,
        mesh_id: params.mesh_id,
    };

    Json(state.node.registry().search_peers(&query).await)
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

// ---------------------------------------------------------------------------
// Matchmaking endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SubmitMatchRequestBody {
    agent_id: String,
    game: String,
    skill_range: SkillRangeBody,
    time_window: TimeWindowBody,
}

#[derive(Debug, Deserialize)]
struct SkillRangeBody {
    min: f64,
    max: f64,
}

#[derive(Debug, Deserialize)]
struct TimeWindowBody {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

/// POST /v0/matchmaking/requests — Submit a match request.
#[instrument(skip(state, body))]
async fn submit_match_request(
    State(state): State<AppState>,
    Json(body): Json<SubmitMatchRequestBody>,
) -> (StatusCode, Json<MatchRequest>) {
    let req = state
        .node
        .matchmaker()
        .submit_request(
            &body.agent_id,
            &body.game,
            SkillRange {
                min: body.skill_range.min,
                max: body.skill_range.max,
            },
            TimeWindow {
                start: body.time_window.start,
                end: body.time_window.end,
            },
        )
        .await;
    info!(request_id = %req.request_id, agent_id = %body.agent_id, "match request submitted");
    (StatusCode::CREATED, Json(req))
}

/// GET /v0/matchmaking/requests — List all match requests.
async fn list_match_requests(State(state): State<AppState>) -> Json<Vec<MatchRequest>> {
    Json(state.node.matchmaker().list_requests().await)
}

/// GET /v0/matchmaking/requests/:request_id — Get a match request.
async fn get_match_request(
    State(state): State<AppState>,
    Path(request_id): Path<String>,
) -> Result<Json<Vec<MatchRequest>>, (StatusCode, Json<ErrorResponse>)> {
    // Return the request itself plus any matches
    let requests = state.node.matchmaker().list_requests().await;
    let found: Vec<_> = requests
        .into_iter()
        .filter(|r| r.request_id == request_id)
        .collect();
    if found.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "request_not_found",
                detail: format!("match request {} not found", request_id),
            }),
        ));
    }
    Ok(Json(found))
}

/// GET /v0/matchmaking/requests/:request_id/matches — Find matching requests.
async fn find_matches(
    State(state): State<AppState>,
    Path(request_id): Path<String>,
) -> Result<Json<Vec<MatchRequest>>, (StatusCode, Json<ErrorResponse>)> {
    state
        .node
        .matchmaker()
        .find_matches(&request_id)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "match_error",
                    detail: e.to_string(),
                }),
            )
        })
}

/// POST /v0/matchmaking/requests/:request_id/cancel — Cancel a match request.
async fn cancel_match_request(
    State(state): State<AppState>,
    Path(request_id): Path<String>,
) -> Result<Json<MatchRequest>, (StatusCode, Json<ErrorResponse>)> {
    state
        .node
        .matchmaker()
        .cancel_request(&request_id)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "cancel_error",
                    detail: e.to_string(),
                }),
            )
        })
}

// ---------------------------------------------------------------------------
// Schedule endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ProposeScheduleBody {
    agent_id: String,
    slot_start: DateTime<Utc>,
    slot_end: DateTime<Utc>,
}

/// POST /v0/matchmaking/schedules — Propose a schedule slot.
#[instrument(skip(state, body))]
async fn propose_schedule(
    State(state): State<AppState>,
    Json(body): Json<ProposeScheduleBody>,
) -> (StatusCode, Json<ScheduleSlot>) {
    let slot = state
        .node
        .matchmaker()
        .propose_slot(&body.agent_id, body.slot_start, body.slot_end)
        .await;
    info!(schedule_id = %slot.schedule_id, agent_id = %body.agent_id, "schedule proposed");
    (StatusCode::CREATED, Json(slot))
}

/// GET /v0/matchmaking/schedules — List all schedules.
#[derive(Debug, Deserialize)]
struct ScheduleQuery {
    agent_id: Option<String>,
}

async fn list_schedules(
    State(state): State<AppState>,
    Query(params): Query<ScheduleQuery>,
) -> Json<Vec<ScheduleSlot>> {
    if let Some(agent_id) = params.agent_id {
        Json(state.node.matchmaker().slots_for_agent(&agent_id).await)
    } else {
        Json(state.node.matchmaker().list_schedules().await)
    }
}

/// POST /v0/matchmaking/schedules/:schedule_id/confirm — Confirm a schedule.
async fn confirm_schedule(
    State(state): State<AppState>,
    Path(schedule_id): Path<String>,
) -> Result<Json<ScheduleSlot>, (StatusCode, Json<ErrorResponse>)> {
    state
        .node
        .matchmaker()
        .confirm_slot(&schedule_id)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "schedule_error",
                    detail: e.to_string(),
                }),
            )
        })
}

/// POST /v0/matchmaking/schedules/:schedule_id/decline — Decline a schedule.
async fn decline_schedule(
    State(state): State<AppState>,
    Path(schedule_id): Path<String>,
) -> Result<Json<ScheduleSlot>, (StatusCode, Json<ErrorResponse>)> {
    state
        .node
        .matchmaker()
        .decline_slot(&schedule_id)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "schedule_error",
                    detail: e.to_string(),
                }),
            )
        })
}

// ---------------------------------------------------------------------------
// Offer endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SubmitOfferBody {
    from_agent: String,
    offer_id: String,
    summary: String,
    expires_at: DateTime<Utc>,
}

/// POST /v0/matchmaking/offers — Submit an offer.
#[instrument(skip(state, body))]
async fn submit_offer(
    State(state): State<AppState>,
    Json(body): Json<SubmitOfferBody>,
) -> (StatusCode, Json<PeerOffer>) {
    let offer = state
        .node
        .matchmaker()
        .submit_offer(&body.from_agent, &body.offer_id, &body.summary, body.expires_at)
        .await;
    info!(offer_id = %offer.offer_id, from = %body.from_agent, "offer submitted");
    (StatusCode::CREATED, Json(offer))
}

/// GET /v0/matchmaking/offers — List active offers.
async fn list_offers(State(state): State<AppState>) -> Json<Vec<PeerOffer>> {
    Json(state.node.matchmaker().active_offers().await)
}

/// GET /v0/matchmaking/offers/:offer_id — Get an offer.
async fn get_offer(
    State(state): State<AppState>,
    Path(offer_id): Path<String>,
) -> Result<Json<PeerOffer>, (StatusCode, Json<ErrorResponse>)> {
    state
        .node
        .matchmaker()
        .get_offer(&offer_id)
        .await
        .map(Json)
        .map_err(|e| {
            let status = match &e {
                clawmesh_p2p::MatchError::OfferExpired => StatusCode::GONE,
                _ => StatusCode::NOT_FOUND,
            };
            (
                status,
                Json(ErrorResponse {
                    error: "offer_error",
                    detail: e.to_string(),
                }),
            )
        })
}

/// POST /v0/matchmaking/offers/:offer_id/withdraw — Withdraw an offer.
async fn withdraw_offer(
    State(state): State<AppState>,
    Path(offer_id): Path<String>,
) -> Result<Json<PeerOffer>, (StatusCode, Json<ErrorResponse>)> {
    state
        .node
        .matchmaker()
        .withdraw_offer(&offer_id)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "offer_error",
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
