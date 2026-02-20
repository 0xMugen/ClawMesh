use std::{env, net::SocketAddr};

use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use clawmesh_p2p::{
    AnnounceMessage, EventBus, GossipMessage, GossipRouter, MeshRegistry, PeerEntry, PeerStatus,
    SignalMessage, SignalRelay,
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
    registry: MeshRegistry,
    relay: SignalRelay,
    gossip: GossipRouter,
    #[allow(dead_code)]
    bus: EventBus,
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

    let bus = EventBus::default();
    let registry = MeshRegistry::new(&mesh_id, bus.clone());
    let relay = SignalRelay::new(bus.clone());
    let gossip = GossipRouter::new(&node_id, registry.clone(), bus.clone());
    let state = AppState {
        registry,
        relay,
        gossip,
        bus,
    };

    let app = Router::new()
        .route("/healthz", get(health))
        .route("/v0/envelope", post(route_envelope))
        .route("/v0/peers", get(list_peers))
        .route("/v0/signal/relay", post(relay_signal))
        .route("/v0/signal/status", get(relay_status))
        .route("/v0/gossip/exchange", post(gossip_exchange))
        .route("/v0/gossip/status", get(gossip_status))
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

    state.registry.handle_announce(msg).await;

    // Auto-register the announcing peer with the signal relay
    state
        .relay
        .register(&envelope.sender.agent_id, 64)
        .await;

    RouteResponse {
        status: "accepted",
        intent: Intent::Announce,
        route: "announce_router",
    }
}

#[instrument(skip(_state, envelope))]
async fn route_request_match(_state: &AppState, envelope: &Envelope) -> RouteResponse {
    info!(capability = %envelope.capability, "routing request_match envelope");
    RouteResponse {
        status: "stub",
        intent: Intent::RequestMatch,
        route: "request_match_router",
    }
}

#[instrument(skip(_state, envelope))]
async fn route_schedule(_state: &AppState, envelope: &Envelope) -> RouteResponse {
    info!(capability = %envelope.capability, "routing schedule envelope");
    RouteResponse {
        status: "stub",
        intent: Intent::Schedule,
        route: "schedule_router",
    }
}

#[instrument(skip(_state, envelope))]
async fn route_offer(_state: &AppState, envelope: &Envelope) -> RouteResponse {
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
    Json(state.registry.list_peers().await)
}

// ---------------------------------------------------------------------------
// Signal relay endpoints
// ---------------------------------------------------------------------------

/// POST /v0/signal/relay — Forward a signaling message to the target peer.
#[instrument(skip(state, msg))]
async fn relay_signal(
    State(state): State<AppState>,
    Json(msg): Json<SignalMessage>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    state.relay.relay(msg).await.map_err(|e| {
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

/// GET /v0/signal/status — Show relay registration count.
async fn relay_status(State(state): State<AppState>) -> Json<RelayStatusResponse> {
    Json(RelayStatusResponse {
        status: "ok",
        registered_peers: state.relay.peer_count().await,
    })
}

// ---------------------------------------------------------------------------
// Gossip exchange endpoints
// ---------------------------------------------------------------------------

/// POST /v0/gossip/exchange — Accept a gossip message from a remote node.
/// The remote node sends either a Pull (its digest) or a Push (entries we need).
/// For Pull messages, the response contains a Push with our delta.
#[instrument(skip(state, body))]
async fn gossip_exchange(
    State(state): State<AppState>,
    Json(body): Json<GossipExchangeRequest>,
) -> Result<Json<GossipExchangeResponse>, (StatusCode, Json<ErrorResponse>)> {
    match body.message {
        GossipMessage::Pull(pull) => {
            let from = pull.from.clone();
            let push = state.registry.handle_gossip_pull(pull).await;
            let entry_count = push.entries.len();
            info!(%from, entries = entry_count, "handled gossip pull from remote node");
            Ok(Json(GossipExchangeResponse {
                status: "ok",
                message: Some(GossipMessage::Push(push)),
            }))
        }
        GossipMessage::Push(push) => {
            let entry_count = push.entries.len();
            state.registry.merge_gossip_push(push).await;
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

/// GET /v0/gossip/status — Show gossip router status.
async fn gossip_status(State(state): State<AppState>) -> Json<GossipStatusResponse> {
    Json(GossipStatusResponse {
        node_id: state.gossip.node_id().to_string(),
        connected_peers: state.gossip.peer_count().await,
        registry_size: state.registry.peer_count().await,
    })
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_ok() {
        info!("shutdown signal received");
    }
}
