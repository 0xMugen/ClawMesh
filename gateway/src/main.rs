use std::{env, net::SocketAddr};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use clawmesh_p2p::{
    AnnounceMessage, AuthChallenge, AuthHello, AuthResponse, FederationEnvelope, GossipMessage,
    MatchRequest, MeshLink, MeshNode, PeerEntry, PeerOffer, PeerQuery, PeerStatus, RoomInfo,
    ScheduleSlot, SignalMessage, SkillRange, StateDelta, StateEntry, StateSnapshot, TimeWindow,
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
    /// Optional Ed25519 public key of the signer (base64-encoded).
    #[serde(skip_serializing_if = "Option::is_none")]
    signing_key: Option<String>,
    /// Optional expiry timestamp — envelopes past this time are rejected.
    #[serde(skip_serializing_if = "Option::is_none")]
    expiry: Option<DateTime<Utc>>,
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

fn build_router(state: AppState) -> Router {
    Router::new()
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
        .route("/v0/rooms/:room_id/join", post(join_room))
        .route("/v0/rooms/:room_id/leave", post(leave_room))
        .route("/v0/rooms/:room_id/members", get(room_members))
        // Room state
        .route("/v0/rooms/:room_id/state", get(list_state).post(set_state))
        .route("/v0/rooms/:room_id/state/:key", get(get_state))
        .route("/v0/rooms/:room_id/state/delta", post(apply_delta))
        .route(
            "/v0/rooms/:room_id/state/snapshot",
            get(get_snapshot).post(restore_snapshot),
        )
        // Matchmaking
        .route(
            "/v0/matchmaking/requests",
            get(list_match_requests).post(submit_match_request),
        )
        .route(
            "/v0/matchmaking/requests/:request_id",
            get(get_match_request),
        )
        .route(
            "/v0/matchmaking/requests/:request_id/matches",
            get(find_matches),
        )
        .route(
            "/v0/matchmaking/requests/:request_id/cancel",
            post(cancel_match_request),
        )
        // Scheduling
        .route(
            "/v0/matchmaking/schedules",
            get(list_schedules).post(propose_schedule),
        )
        .route(
            "/v0/matchmaking/schedules/:schedule_id/confirm",
            post(confirm_schedule),
        )
        .route(
            "/v0/matchmaking/schedules/:schedule_id/decline",
            post(decline_schedule),
        )
        // Offers
        .route(
            "/v0/matchmaking/offers",
            get(list_offers).post(submit_offer),
        )
        .route("/v0/matchmaking/offers/:offer_id", get(get_offer))
        .route(
            "/v0/matchmaking/offers/:offer_id/withdraw",
            post(withdraw_offer),
        )
        // Federation
        .route(
            "/v0/federation/links",
            get(list_federation_links).post(create_federation_link),
        )
        .route(
            "/v0/federation/links/:mesh_id",
            axum::routing::delete(remove_federation_link),
        )
        .route("/v0/federation/relay", post(federation_relay))
        .route("/v0/federation/status", get(federation_status))
        // Auth handshake
        .route("/v0/auth/hello", post(auth_hello))
        .route("/v0/auth/respond", post(auth_respond))
        .route("/v0/auth/peers", get(list_authenticated_peers))
        // Maintenance
        .route("/v0/maintenance/purge", post(purge_expired))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
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

    let app = build_router(state);

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
    let fmt_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_target(false);

    if let Ok(endpoint) = env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
        let provider = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_trace_config(opentelemetry_sdk::trace::Config::default().with_resource(
                Resource::new(vec![KeyValue::new("service.name", service_name.to_owned())]),
            ))
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

    // Reject expired envelopes.
    if let Some(expiry) = envelope.expiry {
        if expiry < Utc::now() {
            let detail = format!(
                "envelope expired at {}",
                expiry.to_rfc3339()
            );
            warn!(%detail, "expired envelope rejected");
            return Err((
                StatusCode::GONE,
                Json(ErrorResponse {
                    error: "envelope_expired",
                    detail,
                }),
            ));
        }
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
        .submit_offer(
            &body.from_agent,
            &body.offer_id,
            &body.summary,
            body.expires_at,
        )
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

// ---------------------------------------------------------------------------
// Federation endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct FederationStatusResponse {
    local_mesh_id: String,
    linked_meshes: usize,
}

#[derive(Debug, Deserialize)]
struct CreateLinkRequest {
    mesh_id: String,
    label: Option<String>,
}

#[derive(Debug, Serialize)]
struct LinkResponse {
    status: &'static str,
    mesh_id: String,
}

/// GET /v0/federation/status — Return federation status.
async fn federation_status(State(state): State<AppState>) -> Json<FederationStatusResponse> {
    Json(FederationStatusResponse {
        local_mesh_id: state.node.federation().local_mesh_id().to_string(),
        linked_meshes: state.node.federation().link_count().await,
    })
}

/// GET /v0/federation/links — List all linked remote meshes.
async fn list_federation_links(State(state): State<AppState>) -> Json<Vec<MeshLink>> {
    Json(state.node.federation().linked_meshes().await)
}

/// POST /v0/federation/links — Establish a link to a remote mesh.
#[instrument(skip(state, body))]
async fn create_federation_link(
    State(state): State<AppState>,
    Json(body): Json<CreateLinkRequest>,
) -> Result<(StatusCode, Json<LinkResponse>), (StatusCode, Json<ErrorResponse>)> {
    // We create the link but the receiver is used internally by the gateway
    // to actually deliver envelopes. For now, we drop the receiver — a real
    // deployment would spawn a forwarding task to the remote mesh.
    let _rx = state
        .node
        .federation()
        .link_mesh(&body.mesh_id, body.label.as_deref(), 64)
        .await
        .map_err(|e| {
            let detail = e.to_string();
            let status = match &e {
                clawmesh_p2p::FederationError::LinkExists(_) => StatusCode::CONFLICT,
                clawmesh_p2p::FederationError::SelfRelay => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (
                status,
                Json(ErrorResponse {
                    error: "federation_link_error",
                    detail,
                }),
            )
        })?;

    info!(mesh_id = %body.mesh_id, "federation link created");

    Ok((
        StatusCode::CREATED,
        Json(LinkResponse {
            status: "linked",
            mesh_id: body.mesh_id,
        }),
    ))
}

/// DELETE /v0/federation/links/:mesh_id — Remove a link to a remote mesh.
#[instrument(skip(state))]
async fn remove_federation_link(
    State(state): State<AppState>,
    Path(mesh_id): Path<String>,
) -> Result<Json<LinkResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .node
        .federation()
        .unlink_mesh(&mesh_id)
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "federation_unlink_error",
                    detail: e.to_string(),
                }),
            )
        })?;

    info!(mesh_id = %mesh_id, "federation link removed");

    Ok(Json(LinkResponse {
        status: "unlinked",
        mesh_id,
    }))
}

/// POST /v0/federation/relay — Receive a federation envelope from a remote mesh.
///
/// When a remote mesh sends an envelope targeting this mesh, it arrives here.
/// The gateway checks if the envelope is for the local mesh and, if so,
/// processes the inner protocol envelope through the normal dispatch pipeline.
#[instrument(skip(state, body))]
async fn federation_relay(
    State(state): State<AppState>,
    Json(body): Json<FederationEnvelope>,
) -> Result<(StatusCode, Json<RouteResponse>), (StatusCode, Json<ErrorResponse>)> {
    // Check if this envelope is for us
    if !state.node.federation().should_accept(&body) {
        let target_mesh = body.target_mesh.clone();

        // Not for us — if we have a link to the target mesh, forward it
        if state
            .node
            .federation()
            .is_linked(&target_mesh)
            .await
        {
            state
                .node
                .federation()
                .forward(&target_mesh, body)
                .await
                .map_err(|e| {
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(ErrorResponse {
                            error: "federation_forward_error",
                            detail: e.to_string(),
                        }),
                    )
                })?;

            return Ok((
                StatusCode::ACCEPTED,
                Json(RouteResponse {
                    status: "forwarded",
                    intent: Intent::Announce, // placeholder — forwarded envelopes don't have a parsed intent
                    route: "federation_forward",
                }),
            ));
        }

        return Err((
            StatusCode::MISDIRECTED_REQUEST,
            Json(ErrorResponse {
                error: "wrong_mesh",
                detail: format!(
                    "envelope targets mesh '{}', this node serves '{}'",
                    target_mesh,
                    state.node.mesh_id()
                ),
            }),
        ));
    }

    info!(
        source_mesh = %body.source_mesh,
        ttl = body.ttl,
        "federation envelope accepted"
    );

    // Deserialize the inner envelope and process it normally.
    let envelope: Envelope = serde_json::from_value(body.envelope).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "invalid_federated_envelope",
                detail: format!("inner envelope parse error: {e}"),
            }),
        )
    })?;

    if envelope.schema_version != PROTOCOL_VERSION {
        let detail = format!(
            "unsupported schema_version '{}', expected '{}'",
            envelope.schema_version, PROTOCOL_VERSION
        );
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "invalid_schema_version",
                detail,
            }),
        ));
    }

    // Skip policy enforcement for federated envelopes — the source mesh
    // already enforced policy. We trust the federation link.
    let response = dispatch_intent(&state, &envelope).await;
    Ok((StatusCode::ACCEPTED, Json(response)))
}

// ---------------------------------------------------------------------------
// Auth handshake endpoints
// ---------------------------------------------------------------------------

/// POST /v0/auth/hello — Initiate a mutual authentication handshake.
///
/// The caller sends an `AuthHello` containing their agent_id and public key.
/// The gateway responds with an `AuthChallenge` containing a random nonce.
#[instrument(skip(state, body))]
async fn auth_hello(
    State(state): State<AppState>,
    Json(body): Json<AuthHello>,
) -> Result<Json<AuthChallenge>, (StatusCode, Json<ErrorResponse>)> {
    let challenge = state.node.auth().handle_hello(body).await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "auth_error",
                detail: e.to_string(),
            }),
        )
    })?;
    info!(agent_id = %challenge.agent_id, "auth challenge issued");
    Ok(Json(challenge))
}

/// POST /v0/auth/respond — Complete the authentication handshake.
///
/// The caller sends an `AuthResponse` (their signature over the challenge nonce)
/// along with the public key they originally sent in the hello.
#[derive(Debug, Deserialize)]
struct AuthRespondRequest {
    response: AuthResponse,
    public_key: clawmesh_p2p::PublicKeyBytes,
}

#[derive(Debug, Serialize)]
struct AuthRespondResult {
    status: &'static str,
    agent_id: String,
}

#[instrument(skip(state, body))]
async fn auth_respond(
    State(state): State<AppState>,
    Json(body): Json<AuthRespondRequest>,
) -> Result<Json<AuthRespondResult>, (StatusCode, Json<ErrorResponse>)> {
    let agent_id = body.response.agent_id.clone();
    let confirm = state
        .node
        .auth()
        .handle_response(body.response, &body.public_key)
        .await
        .map_err(|e| {
            let status = match &e {
                clawmesh_p2p::AuthError::VerificationFailed => StatusCode::UNAUTHORIZED,
                clawmesh_p2p::AuthError::NoPendingChallenge(_) => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (
                status,
                Json(ErrorResponse {
                    error: "auth_error",
                    detail: e.to_string(),
                }),
            )
        })?;

    info!(agent_id = %confirm.agent_id, peer = %agent_id, "peer authenticated");

    Ok(Json(AuthRespondResult {
        status: "authenticated",
        agent_id,
    }))
}

/// GET /v0/auth/peers — List all authenticated peers.
async fn list_authenticated_peers(State(state): State<AppState>) -> Json<Vec<String>> {
    let peers = state.node.auth().authenticated_peers().await;
    Json(peers.into_iter().map(|p| p.agent_id).collect())
}

// ---------------------------------------------------------------------------
// Maintenance endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct PurgeResponse {
    status: &'static str,
}

/// POST /v0/maintenance/purge — Purge expired match requests and offers.
#[instrument(skip(state))]
async fn purge_expired(State(state): State<AppState>) -> Json<PurgeResponse> {
    state.node.matchmaker().purge_expired().await;
    info!("purge of expired entries completed");
    Json(PurgeResponse { status: "ok" })
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_ok() {
        info!("shutdown signal received");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use hyper::Request;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let node = MeshNode::new("node-test", "mesh-test").unwrap();
        AppState { node }
    }

    fn test_app() -> Router {
        build_router(test_state())
    }

    async fn body_json(body: Body) -> Value {
        let bytes = body.collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn get(path: &str) -> Request<Body> {
        Request::get(path.parse::<hyper::Uri>().unwrap())
            .body(Body::empty())
            .unwrap()
    }

    fn post_json(path: &str, body: &Value) -> Request<Body> {
        Request::post(path.parse::<hyper::Uri>().unwrap())
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap()
    }

    fn post_empty(path: &str) -> Request<Body> {
        Request::post(path.parse::<hyper::Uri>().unwrap())
            .body(Body::empty())
            .unwrap()
    }

    fn delete(path: &str) -> Request<Body> {
        Request::builder()
            .method("DELETE")
            .uri(path.parse::<hyper::Uri>().unwrap())
            .body(Body::empty())
            .unwrap()
    }

    fn make_envelope(intent: &str, policy: &str, payload: Value) -> Value {
        serde_json::json!({
            "schema_version": "0.1",
            "intent": intent,
            "capability": "matchmaking",
            "policy": policy,
            "timestamp": "2026-02-20T12:00:00Z",
            "sender": {
                "agent_id": "agent-alice",
                "mesh_id": "mesh-test"
            },
            "payload": payload
        })
    }

    // -----------------------------------------------------------------------
    // Health
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn health_check() {
        let app = test_app();
        let resp = app.oneshot(get("/healthz")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["status"], "ok");
        assert_eq!(body["service"], "clawmesh-gateway");
    }

    // -----------------------------------------------------------------------
    // Envelope dispatch — announce
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn envelope_announce_accepted() {
        let app = test_app();
        let envelope = make_envelope(
            "announce",
            "public",
            serde_json::json!({
                "display_name": "Alice",
                "supports": ["matchmaking"],
                "status": "available"
            }),
        );
        let resp = app
            .oneshot(post_json("/v0/envelope", &envelope))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["status"], "accepted");
        assert_eq!(body["intent"], "announce");
    }

    #[tokio::test]
    async fn envelope_registers_peer() {
        let state = test_state();
        let app = build_router(state.clone());
        let envelope = make_envelope(
            "announce",
            "public",
            serde_json::json!({
                "display_name": "Bob",
                "supports": ["matchmaking"],
                "status": "busy"
            }),
        );
        let resp = app
            .oneshot(post_json("/v0/envelope", &envelope))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        let app = build_router(state);
        let resp = app.oneshot(get("/v0/peers")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        let peers = body.as_array().unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0]["agent_id"], "agent-alice");
        assert_eq!(peers[0]["display_name"], "Bob");
        assert_eq!(peers[0]["status"], "busy");
    }

    // -----------------------------------------------------------------------
    // Envelope dispatch — request_match
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn envelope_request_match_accepted() {
        let app = test_app();
        let envelope = make_envelope(
            "request_match",
            "public",
            serde_json::json!({
                "game": "golf-18",
                "skill_range": { "min": 50.0, "max": 80.0 },
                "time_window": {
                    "start": "2026-02-20T14:00:00Z",
                    "end": "2026-02-20T16:00:00Z"
                }
            }),
        );
        let resp = app
            .oneshot(post_json("/v0/envelope", &envelope))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["intent"], "request_match");
    }

    // -----------------------------------------------------------------------
    // Envelope dispatch — schedule
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn envelope_schedule_accepted() {
        let app = test_app();
        let envelope = make_envelope(
            "schedule",
            "public",
            serde_json::json!({
                "slot_start": "2026-02-20T14:00:00Z",
                "slot_end": "2026-02-20T16:00:00Z",
                "state": "proposed"
            }),
        );
        let resp = app
            .oneshot(post_json("/v0/envelope", &envelope))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["intent"], "schedule");
    }

    // -----------------------------------------------------------------------
    // Envelope dispatch — offer
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn envelope_offer_accepted() {
        let app = test_app();
        let envelope = make_envelope(
            "offer",
            "public",
            serde_json::json!({
                "offer_id": "offer-1",
                "summary": "Join my golf round",
                "expires_at": "2026-02-21T00:00:00Z"
            }),
        );
        let resp = app
            .oneshot(post_json("/v0/envelope", &envelope))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["intent"], "offer");
    }

    // -----------------------------------------------------------------------
    // Envelope — invalid schema version
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn envelope_rejects_bad_schema_version() {
        let app = test_app();
        let mut envelope = make_envelope(
            "announce",
            "public",
            serde_json::json!({ "display_name": "X", "supports": [], "status": "available" }),
        );
        envelope["schema_version"] = serde_json::json!("999.0");
        let resp = app
            .oneshot(post_json("/v0/envelope", &envelope))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["error"], "invalid_schema_version");
    }

    // -----------------------------------------------------------------------
    // Policy enforcement — mesh-only
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn policy_mesh_only_wrong_mesh_rejected() {
        let app = test_app();
        let mut envelope = make_envelope(
            "announce",
            "mesh-only",
            serde_json::json!({ "display_name": "X", "supports": [], "status": "available" }),
        );
        envelope["sender"]["mesh_id"] = serde_json::json!("mesh-other");
        let resp = app
            .oneshot(post_json("/v0/envelope", &envelope))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["error"], "policy_violation");
    }

    #[tokio::test]
    async fn policy_mesh_only_correct_mesh_accepted() {
        let app = test_app();
        let envelope = make_envelope(
            "announce",
            "mesh-only",
            serde_json::json!({ "display_name": "X", "supports": [], "status": "available" }),
        );
        let resp = app
            .oneshot(post_json("/v0/envelope", &envelope))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    // -----------------------------------------------------------------------
    // Policy enforcement — private
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn policy_private_unregistered_peer_rejected() {
        let app = test_app();
        let envelope = make_envelope(
            "announce",
            "private",
            serde_json::json!({ "display_name": "X", "supports": [], "status": "available" }),
        );
        let resp = app
            .oneshot(post_json("/v0/envelope", &envelope))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["error"], "policy_violation");
    }

    #[tokio::test]
    async fn policy_private_registered_peer_accepted() {
        let state = test_state();
        // First, register the peer via a public announce
        let app = build_router(state.clone());
        let announce = make_envelope(
            "announce",
            "public",
            serde_json::json!({ "display_name": "Alice", "supports": [], "status": "available" }),
        );
        app.oneshot(post_json("/v0/envelope", &announce))
            .await
            .unwrap();

        // Now send a private envelope from the same peer
        let app = build_router(state);
        let private_env = make_envelope(
            "announce",
            "private",
            serde_json::json!({ "display_name": "Alice v2", "supports": ["chat"], "status": "busy" }),
        );
        let resp = app
            .oneshot(post_json("/v0/envelope", &private_env))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    // -----------------------------------------------------------------------
    // Peer search
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn search_peers_by_status() {
        let state = test_state();
        let app = build_router(state.clone());
        let env1 = make_envelope(
            "announce",
            "public",
            serde_json::json!({ "display_name": "Alice", "supports": [], "status": "available" }),
        );
        app.oneshot(post_json("/v0/envelope", &env1)).await.unwrap();

        let app = build_router(state.clone());
        let mut env2 = make_envelope(
            "announce",
            "public",
            serde_json::json!({ "display_name": "Bob", "supports": [], "status": "busy" }),
        );
        env2["sender"]["agent_id"] = serde_json::json!("agent-bob");
        app.oneshot(post_json("/v0/envelope", &env2)).await.unwrap();

        let app = build_router(state);
        let resp = app
            .oneshot(get("/v0/peers/search?status=busy"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        let peers = body.as_array().unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0]["agent_id"], "agent-bob");
    }

    // -----------------------------------------------------------------------
    // Signal relay
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn signal_relay_status() {
        let app = test_app();
        let resp = app.oneshot(get("/v0/signal/status")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["status"], "ok");
    }

    #[tokio::test]
    async fn signal_relay_to_unknown_peer() {
        let app = test_app();
        let msg = serde_json::json!({
            "type": "Offer",
            "from": "agent-alice",
            "to": "agent-nobody",
            "sdp": "v=0\r\n..."
        });
        let resp = app
            .oneshot(post_json("/v0/signal/relay", &msg))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // -----------------------------------------------------------------------
    // Gossip
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn gossip_status() {
        let app = test_app();
        let resp = app.oneshot(get("/v0/gossip/status")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        assert!(body["node_id"].is_string());
    }

    #[tokio::test]
    async fn gossip_pull_exchange() {
        let app = test_app();
        let pull = serde_json::json!({
            "message": {
                "type": "Pull",
                "from": "node-remote",
                "digests": []
            }
        });
        let resp = app
            .oneshot(post_json("/v0/gossip/exchange", &pull))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["status"], "ok");
    }

    // -----------------------------------------------------------------------
    // Rooms
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_and_list_rooms() {
        let state = test_state();
        let app = build_router(state.clone());
        let body = serde_json::json!({ "room_id": "lobby", "name": "Lobby" });
        let resp = app.oneshot(post_json("/v0/rooms", &body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let app = build_router(state);
        let resp = app.oneshot(get("/v0/rooms")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        let rooms = body.as_array().unwrap();
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0]["room_id"], "lobby");
    }

    #[tokio::test]
    async fn duplicate_room_returns_conflict() {
        let state = test_state();
        let body = serde_json::json!({ "room_id": "lobby", "name": "Lobby" });

        let app = build_router(state.clone());
        app.oneshot(post_json("/v0/rooms", &body)).await.unwrap();

        let app = build_router(state);
        let resp = app.oneshot(post_json("/v0/rooms", &body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn join_and_list_members() {
        let state = test_state();

        let app = build_router(state.clone());
        let create = serde_json::json!({ "room_id": "test-room", "name": "Test" });
        app.oneshot(post_json("/v0/rooms", &create)).await.unwrap();

        let app = build_router(state.clone());
        let join = serde_json::json!({ "agent_id": "agent-alice" });
        let resp = app
            .oneshot(post_json("/v0/rooms/test-room/join", &join))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let app = build_router(state);
        let resp = app
            .oneshot(get("/v0/rooms/test-room/members"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        let members = body.as_array().unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0], "agent-alice");
    }

    #[tokio::test]
    async fn leave_room() {
        let state = test_state();

        let app = build_router(state.clone());
        let create = serde_json::json!({ "room_id": "test-room", "name": "Test" });
        app.oneshot(post_json("/v0/rooms", &create)).await.unwrap();

        let app = build_router(state.clone());
        let join = serde_json::json!({ "agent_id": "agent-alice" });
        app.oneshot(post_json("/v0/rooms/test-room/join", &join))
            .await
            .unwrap();

        let app = build_router(state.clone());
        let leave = serde_json::json!({ "agent_id": "agent-alice" });
        let resp = app
            .oneshot(post_json("/v0/rooms/test-room/leave", &leave))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let app = build_router(state);
        let resp = app
            .oneshot(get("/v0/rooms/test-room/members"))
            .await
            .unwrap();
        let body = body_json(resp.into_body()).await;
        assert!(body.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn join_nonexistent_room() {
        let app = test_app();
        let join = serde_json::json!({ "agent_id": "agent-alice" });
        let resp = app
            .oneshot(post_json("/v0/rooms/no-such-room/join", &join))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // -----------------------------------------------------------------------
    // Room state
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn set_and_get_state() {
        let state = test_state();

        let app = build_router(state.clone());
        let create = serde_json::json!({ "room_id": "state-room", "name": "S" });
        app.oneshot(post_json("/v0/rooms", &create)).await.unwrap();

        let app = build_router(state.clone());
        let set_body = serde_json::json!({
            "agent_id": "agent-alice",
            "key": "score",
            "value": 42
        });
        let resp = app
            .oneshot(post_json("/v0/rooms/state-room/state", &set_body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let app = build_router(state);
        let resp = app
            .oneshot(get("/v0/rooms/state-room/state/score"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["key"], "score");
        assert_eq!(body["value"], 42);
    }

    #[tokio::test]
    async fn list_state_entries() {
        let state = test_state();

        let app = build_router(state.clone());
        let create = serde_json::json!({ "room_id": "state-room", "name": "S" });
        app.oneshot(post_json("/v0/rooms", &create)).await.unwrap();

        for key in ["a", "b"] {
            let app = build_router(state.clone());
            let set_body = serde_json::json!({
                "agent_id": "agent-alice",
                "key": key,
                "value": key
            });
            app.oneshot(post_json("/v0/rooms/state-room/state", &set_body))
                .await
                .unwrap();
        }

        let app = build_router(state);
        let resp = app
            .oneshot(get("/v0/rooms/state-room/state"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn state_snapshot_roundtrip() {
        let state = test_state();

        let app = build_router(state.clone());
        let create = serde_json::json!({ "room_id": "snap-room", "name": "Snap" });
        app.oneshot(post_json("/v0/rooms", &create)).await.unwrap();

        let app = build_router(state.clone());
        let set_body = serde_json::json!({ "agent_id": "agent-alice", "key": "x", "value": 1 });
        app.oneshot(post_json("/v0/rooms/snap-room/state", &set_body))
            .await
            .unwrap();

        let app = build_router(state);
        let resp = app
            .oneshot(get("/v0/rooms/snap-room/state/snapshot"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let snapshot = body_json(resp.into_body()).await;
        assert_eq!(snapshot["room_id"], "snap-room");
        assert!(snapshot["entries"].as_object().unwrap().contains_key("x"));
    }

    // -----------------------------------------------------------------------
    // Matchmaking REST endpoints
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn submit_and_list_match_requests() {
        let state = test_state();
        let app = build_router(state.clone());
        let body = serde_json::json!({
            "agent_id": "agent-alice",
            "game": "golf-18",
            "skill_range": { "min": 50.0, "max": 80.0 },
            "time_window": {
                "start": "2026-02-20T14:00:00Z",
                "end": "2026-02-20T16:00:00Z"
            }
        });
        let resp = app
            .oneshot(post_json("/v0/matchmaking/requests", &body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created = body_json(resp.into_body()).await;
        assert!(!created["request_id"].as_str().unwrap().is_empty());

        let app = build_router(state);
        let resp = app.oneshot(get("/v0/matchmaking/requests")).await.unwrap();
        let body = body_json(resp.into_body()).await;
        assert_eq!(body.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn get_match_request_not_found() {
        let app = test_app();
        let resp = app
            .oneshot(get("/v0/matchmaking/requests/nonexistent"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cancel_match_request() {
        let state = test_state();
        let app = build_router(state.clone());
        let body = serde_json::json!({
            "agent_id": "agent-alice",
            "game": "golf-18",
            "skill_range": { "min": 50.0, "max": 80.0 },
            "time_window": {
                "start": "2026-02-20T14:00:00Z",
                "end": "2026-02-20T16:00:00Z"
            }
        });
        let resp = app
            .oneshot(post_json("/v0/matchmaking/requests", &body))
            .await
            .unwrap();
        let created = body_json(resp.into_body()).await;
        let request_id = created["request_id"].as_str().unwrap();

        let app = build_router(state.clone());
        let cancel_path = format!("/v0/matchmaking/requests/{}/cancel", request_id);
        let resp = app.oneshot(post_empty(&cancel_path)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let app = build_router(state);
        let resp = app.oneshot(get("/v0/matchmaking/requests")).await.unwrap();
        let body = body_json(resp.into_body()).await;
        assert!(body.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn find_matches_between_compatible_requests() {
        let state = test_state();

        for agent in ["agent-alice", "agent-bob"] {
            let app = build_router(state.clone());
            let body = serde_json::json!({
                "agent_id": agent,
                "game": "golf-18",
                "skill_range": { "min": 50.0, "max": 80.0 },
                "time_window": {
                    "start": "2026-02-20T14:00:00Z",
                    "end": "2026-02-20T16:00:00Z"
                }
            });
            app.oneshot(post_json("/v0/matchmaking/requests", &body))
                .await
                .unwrap();
        }

        let app = build_router(state.clone());
        let resp = app.oneshot(get("/v0/matchmaking/requests")).await.unwrap();
        let reqs = body_json(resp.into_body()).await;
        let alice_req = reqs
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["agent_id"] == "agent-alice")
            .unwrap();
        let request_id = alice_req["request_id"].as_str().unwrap();

        let app = build_router(state);
        let matches_path = format!("/v0/matchmaking/requests/{}/matches", request_id);
        let resp = app.oneshot(get(&matches_path)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let matches = body_json(resp.into_body()).await;
        assert!(!matches.as_array().unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // Scheduling REST endpoints
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn propose_confirm_decline_schedule() {
        let state = test_state();

        let app = build_router(state.clone());
        let body = serde_json::json!({
            "agent_id": "agent-alice",
            "slot_start": "2026-02-20T14:00:00Z",
            "slot_end": "2026-02-20T16:00:00Z"
        });
        let resp = app
            .oneshot(post_json("/v0/matchmaking/schedules", &body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let slot = body_json(resp.into_body()).await;
        let schedule_id = slot["schedule_id"].as_str().unwrap().to_string();
        assert_eq!(slot["state"], "proposed");

        let app = build_router(state.clone());
        let confirm_path = format!("/v0/matchmaking/schedules/{}/confirm", schedule_id);
        let resp = app.oneshot(post_empty(&confirm_path)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let slot = body_json(resp.into_body()).await;
        assert_eq!(slot["state"], "confirmed");

        let app = build_router(state.clone());
        let resp = app
            .oneshot(post_json("/v0/matchmaking/schedules", &body))
            .await
            .unwrap();
        let slot2 = body_json(resp.into_body()).await;
        let schedule_id2 = slot2["schedule_id"].as_str().unwrap().to_string();

        let app = build_router(state);
        let decline_path = format!("/v0/matchmaking/schedules/{}/decline", schedule_id2);
        let resp = app.oneshot(post_empty(&decline_path)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let slot = body_json(resp.into_body()).await;
        assert_eq!(slot["state"], "declined");
    }

    #[tokio::test]
    async fn list_schedules_with_agent_filter() {
        let state = test_state();

        let app = build_router(state.clone());
        let body = serde_json::json!({
            "agent_id": "agent-alice",
            "slot_start": "2026-02-20T14:00:00Z",
            "slot_end": "2026-02-20T16:00:00Z"
        });
        app.oneshot(post_json("/v0/matchmaking/schedules", &body))
            .await
            .unwrap();

        let app = build_router(state.clone());
        let resp = app.oneshot(get("/v0/matchmaking/schedules")).await.unwrap();
        let body = body_json(resp.into_body()).await;
        assert_eq!(body.as_array().unwrap().len(), 1);

        let app = build_router(state.clone());
        let resp = app
            .oneshot(get("/v0/matchmaking/schedules?agent_id=agent-alice"))
            .await
            .unwrap();
        let body = body_json(resp.into_body()).await;
        assert_eq!(body.as_array().unwrap().len(), 1);

        let app = build_router(state);
        let resp = app
            .oneshot(get("/v0/matchmaking/schedules?agent_id=agent-nobody"))
            .await
            .unwrap();
        let body = body_json(resp.into_body()).await;
        assert!(body.as_array().unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // Offer REST endpoints
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn submit_list_get_withdraw_offer() {
        let state = test_state();

        let app = build_router(state.clone());
        let body = serde_json::json!({
            "from_agent": "agent-alice",
            "offer_id": "offer-1",
            "summary": "Join my golf round",
            "expires_at": "2030-12-31T23:59:59Z"
        });
        let resp = app
            .oneshot(post_json("/v0/matchmaking/offers", &body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let offer = body_json(resp.into_body()).await;
        assert_eq!(offer["offer_id"], "offer-1");
        assert_eq!(offer["summary"], "Join my golf round");

        let app = build_router(state.clone());
        let resp = app.oneshot(get("/v0/matchmaking/offers")).await.unwrap();
        let body = body_json(resp.into_body()).await;
        assert_eq!(body.as_array().unwrap().len(), 1);

        let app = build_router(state.clone());
        let resp = app
            .oneshot(get("/v0/matchmaking/offers/offer-1"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["offer_id"], "offer-1");

        let app = build_router(state.clone());
        let resp = app
            .oneshot(post_empty("/v0/matchmaking/offers/offer-1/withdraw"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let app = build_router(state);
        let resp = app.oneshot(get("/v0/matchmaking/offers")).await.unwrap();
        let body = body_json(resp.into_body()).await;
        assert!(body.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_nonexistent_offer() {
        let app = test_app();
        let resp = app
            .oneshot(get("/v0/matchmaking/offers/nope"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // -----------------------------------------------------------------------
    // End-to-end scenario: golf-sim workflow
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn e2e_golf_sim_workflow() {
        let state = test_state();

        // 1. Alice announces
        let app = build_router(state.clone());
        let announce = make_envelope(
            "announce",
            "public",
            serde_json::json!({
                "display_name": "Alice",
                "supports": ["matchmaking"],
                "status": "available"
            }),
        );
        let resp = app
            .oneshot(post_json("/v0/envelope", &announce))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        // 2. Create a room
        let app = build_router(state.clone());
        let create_room = serde_json::json!({
            "room_id": "golf-lobby",
            "name": "Golf Sim Lobby",
            "max_members": 4
        });
        let resp = app
            .oneshot(post_json("/v0/rooms", &create_room))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // 3. Alice joins the room
        let app = build_router(state.clone());
        let join = serde_json::json!({ "agent_id": "agent-alice" });
        let resp = app
            .oneshot(post_json("/v0/rooms/golf-lobby/join", &join))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 4. Set room state
        let app = build_router(state.clone());
        let set_state = serde_json::json!({
            "agent_id": "agent-alice",
            "key": "ready",
            "value": true
        });
        let resp = app
            .oneshot(post_json("/v0/rooms/golf-lobby/state", &set_state))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 5. Submit match request via REST
        let app = build_router(state.clone());
        let match_body = serde_json::json!({
            "agent_id": "agent-alice",
            "game": "golf-18",
            "skill_range": { "min": 50.0, "max": 80.0 },
            "time_window": {
                "start": "2026-02-20T14:00:00Z",
                "end": "2026-02-20T16:00:00Z"
            }
        });
        let resp = app
            .oneshot(post_json("/v0/matchmaking/requests", &match_body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // 6. Propose a schedule
        let app = build_router(state.clone());
        let schedule = serde_json::json!({
            "agent_id": "agent-alice",
            "slot_start": "2026-02-20T14:00:00Z",
            "slot_end": "2026-02-20T16:00:00Z"
        });
        let resp = app
            .oneshot(post_json("/v0/matchmaking/schedules", &schedule))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let slot = body_json(resp.into_body()).await;
        let schedule_id = slot["schedule_id"].as_str().unwrap().to_string();

        // 7. Confirm the schedule
        let app = build_router(state.clone());
        let confirm_path = format!("/v0/matchmaking/schedules/{}/confirm", schedule_id);
        let resp = app.oneshot(post_empty(&confirm_path)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 8. Submit an offer
        let app = build_router(state.clone());
        let offer = serde_json::json!({
            "from_agent": "agent-alice",
            "offer_id": "offer-golf-1",
            "summary": "18-hole round at Pebble Beach",
            "expires_at": "2030-12-31T23:59:59Z"
        });
        let resp = app
            .oneshot(post_json("/v0/matchmaking/offers", &offer))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // 9. Verify the full state
        let app = build_router(state.clone());
        let resp = app.oneshot(get("/v0/peers")).await.unwrap();
        let peers = body_json(resp.into_body()).await;
        assert_eq!(peers.as_array().unwrap().len(), 1);

        let app = build_router(state.clone());
        let resp = app.oneshot(get("/v0/rooms")).await.unwrap();
        let rooms = body_json(resp.into_body()).await;
        assert_eq!(rooms.as_array().unwrap().len(), 1);

        let app = build_router(state);
        let resp = app.oneshot(get("/v0/matchmaking/offers")).await.unwrap();
        let offers = body_json(resp.into_body()).await;
        assert_eq!(offers.as_array().unwrap().len(), 1);
        assert_eq!(offers[0]["summary"], "18-hole round at Pebble Beach");
    }

    // -----------------------------------------------------------------------
    // Federation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn federation_status_returns_local_mesh() {
        let app = test_app();
        let resp = app
            .oneshot(get("/v0/federation/status"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["local_mesh_id"], "mesh-test");
        assert_eq!(body["linked_meshes"], 0);
    }

    #[tokio::test]
    async fn create_and_list_federation_links() {
        let state = test_state();

        let app = build_router(state.clone());
        let link = serde_json::json!({ "mesh_id": "mesh-remote", "label": "Remote Mesh" });
        let resp = app
            .oneshot(post_json("/v0/federation/links", &link))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["status"], "linked");
        assert_eq!(body["mesh_id"], "mesh-remote");

        let app = build_router(state.clone());
        let resp = app
            .oneshot(get("/v0/federation/links"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        let links = body.as_array().unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0]["mesh_id"], "mesh-remote");

        // Status should reflect the link
        let app = build_router(state);
        let resp = app
            .oneshot(get("/v0/federation/status"))
            .await
            .unwrap();
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["linked_meshes"], 1);
    }

    #[tokio::test]
    async fn remove_federation_link() {
        let state = test_state();

        let app = build_router(state.clone());
        let link = serde_json::json!({ "mesh_id": "mesh-remote" });
        app.oneshot(post_json("/v0/federation/links", &link))
            .await
            .unwrap();

        let app = build_router(state.clone());
        let resp = app
            .oneshot(delete("/v0/federation/links/mesh-remote"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["status"], "unlinked");

        let app = build_router(state);
        let resp = app
            .oneshot(get("/v0/federation/links"))
            .await
            .unwrap();
        let body = body_json(resp.into_body()).await;
        assert!(body.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn duplicate_federation_link_returns_conflict() {
        let state = test_state();

        let app = build_router(state.clone());
        let link = serde_json::json!({ "mesh_id": "mesh-remote" });
        app.oneshot(post_json("/v0/federation/links", &link))
            .await
            .unwrap();

        let app = build_router(state);
        let link = serde_json::json!({ "mesh_id": "mesh-remote" });
        let resp = app
            .oneshot(post_json("/v0/federation/links", &link))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["error"], "federation_link_error");
    }

    #[tokio::test]
    async fn self_federation_link_returns_bad_request() {
        let app = test_app();
        let link = serde_json::json!({ "mesh_id": "mesh-test" });
        let resp = app
            .oneshot(post_json("/v0/federation/links", &link))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["error"], "federation_link_error");
    }

    #[tokio::test]
    async fn federation_relay_accepts_envelope_for_local_mesh() {
        let state = test_state();

        // Link a source mesh first (so we know federation is set up)
        let app = build_router(state.clone());
        let link = serde_json::json!({ "mesh_id": "mesh-remote" });
        app.oneshot(post_json("/v0/federation/links", &link))
            .await
            .unwrap();

        // Build a federation envelope targeting our local mesh
        let inner = make_envelope(
            "announce",
            "public",
            serde_json::json!({
                "display_name": "RemoteAlice",
                "supports": ["matchmaking"],
                "status": "available"
            }),
        );
        let fed_envelope = serde_json::json!({
            "source_mesh": "mesh-remote",
            "target_mesh": "mesh-test",
            "envelope": inner,
            "relayed_at": "2026-02-20T12:00:00Z",
            "ttl": 8
        });

        let app = build_router(state.clone());
        let resp = app
            .oneshot(post_json("/v0/federation/relay", &fed_envelope))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["status"], "accepted");
        assert_eq!(body["intent"], "announce");

        // The inner envelope should have been processed — peer should be registered
        let app = build_router(state);
        let resp = app.oneshot(get("/v0/peers")).await.unwrap();
        let body = body_json(resp.into_body()).await;
        let peers = body.as_array().unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0]["display_name"], "RemoteAlice");
    }

    #[tokio::test]
    async fn federation_relay_rejects_wrong_mesh() {
        let app = test_app();

        let inner = make_envelope(
            "announce",
            "public",
            serde_json::json!({
                "display_name": "X",
                "supports": [],
                "status": "available"
            }),
        );
        let fed_envelope = serde_json::json!({
            "source_mesh": "mesh-remote",
            "target_mesh": "mesh-other",
            "envelope": inner,
            "relayed_at": "2026-02-20T12:00:00Z",
            "ttl": 8
        });

        let resp = app
            .oneshot(post_json("/v0/federation/relay", &fed_envelope))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::MISDIRECTED_REQUEST);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["error"], "wrong_mesh");
    }

    #[tokio::test]
    async fn remove_nonexistent_federation_link_returns_not_found() {
        let app = test_app();
        let resp = app
            .oneshot(delete("/v0/federation/links/mesh-nope"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["error"], "federation_unlink_error");
    }

    // -----------------------------------------------------------------------
    // Envelope expiry
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn expired_envelope_rejected() {
        let app = test_app();
        let mut envelope = make_envelope(
            "announce",
            "public",
            serde_json::json!({ "display_name": "X", "supports": [], "status": "available" }),
        );
        envelope["expiry"] = serde_json::json!("2020-01-01T00:00:00Z");
        let resp = app
            .oneshot(post_json("/v0/envelope", &envelope))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::GONE);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["error"], "envelope_expired");
    }

    #[tokio::test]
    async fn future_expiry_accepted() {
        let app = test_app();
        let mut envelope = make_envelope(
            "announce",
            "public",
            serde_json::json!({ "display_name": "X", "supports": [], "status": "available" }),
        );
        envelope["expiry"] = serde_json::json!("2030-12-31T23:59:59Z");
        let resp = app
            .oneshot(post_json("/v0/envelope", &envelope))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn no_expiry_accepted() {
        let app = test_app();
        let envelope = make_envelope(
            "announce",
            "public",
            serde_json::json!({ "display_name": "X", "supports": [], "status": "available" }),
        );
        // No expiry field at all — should be accepted
        let resp = app
            .oneshot(post_json("/v0/envelope", &envelope))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    // -----------------------------------------------------------------------
    // Auth handshake
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn auth_hello_returns_challenge() {
        let state = test_state();
        let app = build_router(state.clone());

        let identity = clawmesh_p2p::crypto::PeerIdentity::generate().unwrap();
        let hello = serde_json::json!({
            "agent_id": "agent:carol",
            "public_key": { "key": identity.public_key().key }
        });

        let resp = app
            .oneshot(post_json("/v0/auth/hello", &hello))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        assert!(body["nonce"].is_string());
        assert!(!body["nonce"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn auth_full_handshake() {
        let state = test_state();

        let identity = clawmesh_p2p::crypto::PeerIdentity::generate().unwrap();
        let public_key = identity.public_key();

        // Step 1: Hello
        let app = build_router(state.clone());
        let hello = serde_json::json!({
            "agent_id": "agent:dave",
            "public_key": { "key": public_key.key }
        });
        let resp = app
            .oneshot(post_json("/v0/auth/hello", &hello))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let challenge_body = body_json(resp.into_body()).await;
        let nonce = challenge_body["nonce"].as_str().unwrap();

        // Step 2: Sign the challenge
        let nonce_bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            nonce,
        )
        .unwrap();
        let sig = identity.sign(&nonce_bytes);

        // Step 3: Respond
        let app = build_router(state.clone());
        let respond = serde_json::json!({
            "response": {
                "agent_id": "agent:dave",
                "signature": { "sig": sig.sig }
            },
            "public_key": { "key": public_key.key }
        });
        let resp = app
            .oneshot(post_json("/v0/auth/respond", &respond))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["status"], "authenticated");
        assert_eq!(body["agent_id"], "agent:dave");

        // Step 4: Verify the peer appears in authenticated peers list
        let app = build_router(state);
        let resp = app.oneshot(get("/v0/auth/peers")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        let peers = body.as_array().unwrap();
        assert!(peers.iter().any(|p| p == "agent:dave"));
    }

    // -----------------------------------------------------------------------
    // Maintenance — purge expired
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn purge_expired_returns_ok() {
        let app = test_app();
        let resp = app.oneshot(post_empty("/v0/maintenance/purge")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp.into_body()).await;
        assert_eq!(body["status"], "ok");
    }
}
