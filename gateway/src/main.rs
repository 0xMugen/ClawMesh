use std::{env, net::SocketAddr};

use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use opentelemetry::{global, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_http::trace::TraceLayer;
use tracing::{error, info, instrument};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

const PROTOCOL_VERSION: &str = "0.1";
const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8080";

#[derive(Clone, Debug, Default)]
struct AppState;

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing("clawmesh-gateway");

    let bind_addr = env::var("GATEWAY_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string());
    let socket_addr: SocketAddr = bind_addr.parse()?;

    let app = Router::new()
        .route("/healthz", get(health))
        .route("/v0/envelope", post(route_envelope))
        .with_state(AppState)
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(socket_addr).await?;
    info!(%socket_addr, "gateway listening");

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
        let tracer = opentelemetry_otlp::new_pipeline()
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

        match tracer {
            Ok(tracer) => {
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
async fn route_announce(_state: &AppState, envelope: &Envelope) -> RouteResponse {
    info!(capability = %envelope.capability, "routing announce envelope");
    RouteResponse {
        status: "stub",
        intent: Intent::Announce,
        route: "announce_router",
    }
}

#[instrument(skip(state, envelope))]
async fn route_request_match(_state: &AppState, envelope: &Envelope) -> RouteResponse {
    info!(capability = %envelope.capability, "routing request_match envelope");
    RouteResponse {
        status: "stub",
        intent: Intent::RequestMatch,
        route: "request_match_router",
    }
}

#[instrument(skip(state, envelope))]
async fn route_schedule(_state: &AppState, envelope: &Envelope) -> RouteResponse {
    info!(capability = %envelope.capability, "routing schedule envelope");
    RouteResponse {
        status: "stub",
        intent: Intent::Schedule,
        route: "schedule_router",
    }
}

#[instrument(skip(state, envelope))]
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

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_ok() {
        info!("shutdown signal received");
    }
}

