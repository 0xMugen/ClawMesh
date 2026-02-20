//! Multi-mesh federation — cross-mesh envelope relay and peer mesh discovery.
//!
//! Federation enables meshes to discover each other and relay envelopes across
//! mesh boundaries. Each mesh node can register links to remote meshes. When an
//! envelope targets a non-local mesh, the federation router forwards it to the
//! appropriate linked mesh.
//!
//! # Concepts
//!
//! - **MeshLink**: a connection to a remote mesh, identified by its mesh ID and
//!   backed by a channel for relaying envelopes.
//! - **FederationEnvelope**: a wrapper around a protocol envelope that carries
//!   the original envelope plus routing metadata (source mesh, target mesh).
//! - **FederationRouter**: manages mesh links and routes envelopes to the correct
//!   mesh. Local envelopes are handled by the node's own subsystems; remote
//!   envelopes are forwarded through the appropriate mesh link.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};

use crate::events::{Event, EventBus};

/// A federation envelope wrapping a protocol message for cross-mesh relay.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FederationEnvelope {
    /// The mesh that originated this envelope.
    pub source_mesh: String,
    /// The mesh this envelope should be delivered to.
    pub target_mesh: String,
    /// The original protocol envelope payload (opaque JSON).
    pub envelope: serde_json::Value,
    /// Timestamp when the federation relay occurred.
    pub relayed_at: DateTime<Utc>,
    /// TTL counter — decremented on each relay hop to prevent loops.
    pub ttl: u8,
}

/// Information about a linked remote mesh.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MeshLink {
    /// The remote mesh's identifier.
    pub mesh_id: String,
    /// Optional human-readable label.
    pub label: Option<String>,
    /// When the link was established.
    pub linked_at: DateTime<Utc>,
}

/// Errors that can occur during federation operations.
#[derive(Debug, thiserror::Error)]
pub enum FederationError {
    #[error("mesh link not found: {0}")]
    LinkNotFound(String),
    #[error("failed to relay envelope to mesh {0}: channel closed")]
    RelayFailed(String),
    #[error("envelope TTL expired (loops prevented)")]
    TtlExpired,
    #[error("cannot relay to self mesh")]
    SelfRelay,
    #[error("mesh link already exists: {0}")]
    LinkExists(String),
}

/// Internal state for a connected mesh link.
struct MeshLinkState {
    info: MeshLink,
    tx: mpsc::Sender<FederationEnvelope>,
}

/// Routes envelopes across mesh boundaries.
///
/// Each node has one `FederationRouter`. It maintains links to remote meshes
/// and can relay envelopes to them. When a remote mesh sends an envelope to
/// this node's mesh, it arrives on the receiver channel returned by `link_mesh`.
#[derive(Clone)]
pub struct FederationRouter {
    local_mesh_id: String,
    links: Arc<RwLock<HashMap<String, MeshLinkState>>>,
    bus: EventBus,
}

impl FederationRouter {
    /// Default TTL for federation envelopes.
    pub const DEFAULT_TTL: u8 = 8;

    /// Create a new federation router for the given local mesh.
    pub fn new(local_mesh_id: &str, bus: EventBus) -> Self {
        tracing::info!(mesh_id = %local_mesh_id, "federation router created");
        Self {
            local_mesh_id: local_mesh_id.to_string(),
            links: Arc::new(RwLock::new(HashMap::new())),
            bus,
        }
    }

    /// Establish a link to a remote mesh.
    ///
    /// Returns a receiver for envelopes relayed to that mesh. The caller should
    /// spawn a task that reads from this receiver and actually delivers envelopes
    /// to the remote mesh (e.g. via HTTP, WebSocket, or in-process for testing).
    pub async fn link_mesh(
        &self,
        remote_mesh_id: &str,
        label: Option<&str>,
        buffer: usize,
    ) -> Result<mpsc::Receiver<FederationEnvelope>, FederationError> {
        if remote_mesh_id == self.local_mesh_id {
            return Err(FederationError::SelfRelay);
        }

        let mut links = self.links.write().await;
        if links.contains_key(remote_mesh_id) {
            return Err(FederationError::LinkExists(remote_mesh_id.to_string()));
        }

        let (tx, rx) = mpsc::channel(buffer);
        let info = MeshLink {
            mesh_id: remote_mesh_id.to_string(),
            label: label.map(String::from),
            linked_at: Utc::now(),
        };

        links.insert(
            remote_mesh_id.to_string(),
            MeshLinkState {
                info,
                tx,
            },
        );
        drop(links);

        tracing::info!(
            local = %self.local_mesh_id,
            remote = %remote_mesh_id,
            "federation link established"
        );

        self.bus.emit(Event::PatchReady {
            mesh_id: self.local_mesh_id.clone(),
            description: format!("federation link established: {remote_mesh_id}"),
        });

        Ok(rx)
    }

    /// Remove a link to a remote mesh.
    pub async fn unlink_mesh(&self, remote_mesh_id: &str) -> Result<MeshLink, FederationError> {
        let state = self
            .links
            .write()
            .await
            .remove(remote_mesh_id)
            .ok_or_else(|| FederationError::LinkNotFound(remote_mesh_id.to_string()))?;

        tracing::info!(
            local = %self.local_mesh_id,
            remote = %remote_mesh_id,
            "federation link removed"
        );

        self.bus.emit(Event::PatchReady {
            mesh_id: self.local_mesh_id.clone(),
            description: format!("federation link removed: {remote_mesh_id}"),
        });

        Ok(state.info)
    }

    /// Relay an envelope to a specific remote mesh.
    ///
    /// Wraps the envelope in a `FederationEnvelope` with routing metadata and
    /// sends it through the linked mesh's channel.
    pub async fn relay(
        &self,
        target_mesh: &str,
        envelope: serde_json::Value,
    ) -> Result<(), FederationError> {
        if target_mesh == self.local_mesh_id {
            return Err(FederationError::SelfRelay);
        }

        let fed_envelope = FederationEnvelope {
            source_mesh: self.local_mesh_id.clone(),
            target_mesh: target_mesh.to_string(),
            envelope,
            relayed_at: Utc::now(),
            ttl: Self::DEFAULT_TTL,
        };

        self.send_federation_envelope(target_mesh, fed_envelope).await
    }

    /// Forward a received federation envelope to another mesh (multi-hop relay).
    ///
    /// Decrements the TTL and forwards the envelope. Returns `TtlExpired` if
    /// the TTL has reached zero (loop prevention).
    pub async fn forward(
        &self,
        target_mesh: &str,
        mut envelope: FederationEnvelope,
    ) -> Result<(), FederationError> {
        if envelope.ttl == 0 {
            tracing::warn!(
                source = %envelope.source_mesh,
                target = %target_mesh,
                "federation envelope TTL expired"
            );
            return Err(FederationError::TtlExpired);
        }

        envelope.ttl -= 1;
        envelope.relayed_at = Utc::now();

        self.send_federation_envelope(target_mesh, envelope).await
    }

    /// Check whether this envelope should be accepted by the local mesh.
    ///
    /// Returns `true` if the target mesh matches the local mesh and the TTL
    /// is still positive.
    pub fn should_accept(&self, envelope: &FederationEnvelope) -> bool {
        envelope.target_mesh == self.local_mesh_id && envelope.ttl > 0
    }

    /// List all linked meshes.
    pub async fn linked_meshes(&self) -> Vec<MeshLink> {
        self.links
            .read()
            .await
            .values()
            .map(|s| s.info.clone())
            .collect()
    }

    /// Check whether a link exists to a remote mesh.
    pub async fn is_linked(&self, remote_mesh_id: &str) -> bool {
        self.links.read().await.contains_key(remote_mesh_id)
    }

    /// Return the number of linked meshes.
    pub async fn link_count(&self) -> usize {
        self.links.read().await.len()
    }

    /// Return the local mesh ID.
    pub fn local_mesh_id(&self) -> &str {
        &self.local_mesh_id
    }

    /// Internal: send a federation envelope through the appropriate link.
    async fn send_federation_envelope(
        &self,
        target_mesh: &str,
        envelope: FederationEnvelope,
    ) -> Result<(), FederationError> {
        let links = self.links.read().await;
        let link = links
            .get(target_mesh)
            .ok_or_else(|| FederationError::LinkNotFound(target_mesh.to_string()))?;

        link.tx
            .send(envelope)
            .await
            .map_err(|_| FederationError::RelayFailed(target_mesh.to_string()))?;

        tracing::debug!(
            local = %self.local_mesh_id,
            target = %target_mesh,
            "federation envelope relayed"
        );

        Ok(())
    }
}

/// Create a bidirectional federation link between two routers (for testing).
///
/// Returns receivers for each side: the first receiver gets envelopes sent to
/// mesh A, the second gets envelopes sent to mesh B.
pub async fn federation_pair(
    router_a: &FederationRouter,
    router_b: &FederationRouter,
    buffer: usize,
) -> Result<
    (
        mpsc::Receiver<FederationEnvelope>,
        mpsc::Receiver<FederationEnvelope>,
    ),
    FederationError,
> {
    let rx_a = router_b
        .link_mesh(router_a.local_mesh_id(), None, buffer)
        .await?;
    let rx_b = router_a
        .link_mesh(router_b.local_mesh_id(), None, buffer)
        .await?;
    Ok((rx_a, rx_b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_router(mesh_id: &str) -> FederationRouter {
        FederationRouter::new(mesh_id, EventBus::default())
    }

    #[tokio::test]
    async fn link_and_relay_envelope() {
        let router_a = make_router("mesh:alpha");
        let mut rx = router_a.link_mesh("mesh:beta", Some("Beta Mesh"), 16).await.unwrap();

        let envelope = serde_json::json!({
            "schema_version": "0.1",
            "intent": "announce",
            "capability": "matchmaking",
            "sender": { "agent_id": "agent:alice", "mesh_id": "mesh:alpha" },
            "payload": { "display_name": "Alice" }
        });

        router_a.relay("mesh:beta", envelope.clone()).await.unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received.source_mesh, "mesh:alpha");
        assert_eq!(received.target_mesh, "mesh:beta");
        assert_eq!(received.envelope, envelope);
        assert_eq!(received.ttl, FederationRouter::DEFAULT_TTL);
    }

    #[tokio::test]
    async fn unlink_mesh() {
        let router = make_router("mesh:alpha");
        router.link_mesh("mesh:beta", None, 16).await.unwrap();

        assert!(router.is_linked("mesh:beta").await);
        assert_eq!(router.link_count().await, 1);

        let info = router.unlink_mesh("mesh:beta").await.unwrap();
        assert_eq!(info.mesh_id, "mesh:beta");
        assert!(!router.is_linked("mesh:beta").await);
        assert_eq!(router.link_count().await, 0);
    }

    #[tokio::test]
    async fn unlink_nonexistent_returns_error() {
        let router = make_router("mesh:alpha");
        let err = router.unlink_mesh("mesh:nowhere").await.unwrap_err();
        assert!(matches!(err, FederationError::LinkNotFound(_)));
    }

    #[tokio::test]
    async fn self_relay_rejected() {
        let router = make_router("mesh:alpha");
        let err = router
            .relay("mesh:alpha", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, FederationError::SelfRelay));
    }

    #[tokio::test]
    async fn self_link_rejected() {
        let router = make_router("mesh:alpha");
        let err = router.link_mesh("mesh:alpha", None, 16).await.unwrap_err();
        assert!(matches!(err, FederationError::SelfRelay));
    }

    #[tokio::test]
    async fn duplicate_link_rejected() {
        let router = make_router("mesh:alpha");
        router.link_mesh("mesh:beta", None, 16).await.unwrap();
        let err = router.link_mesh("mesh:beta", None, 16).await.unwrap_err();
        assert!(matches!(err, FederationError::LinkExists(_)));
    }

    #[tokio::test]
    async fn relay_to_unlinked_mesh_fails() {
        let router = make_router("mesh:alpha");
        let err = router
            .relay("mesh:beta", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, FederationError::LinkNotFound(_)));
    }

    #[tokio::test]
    async fn forward_decrements_ttl() {
        let router = make_router("mesh:alpha");
        let mut rx = router.link_mesh("mesh:gamma", None, 16).await.unwrap();

        let fed_env = FederationEnvelope {
            source_mesh: "mesh:beta".into(),
            target_mesh: "mesh:gamma".into(),
            envelope: serde_json::json!({"intent": "announce"}),
            relayed_at: Utc::now(),
            ttl: 5,
        };

        router.forward("mesh:gamma", fed_env).await.unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received.ttl, 4);
    }

    #[tokio::test]
    async fn forward_ttl_zero_rejected() {
        let router = make_router("mesh:alpha");
        router.link_mesh("mesh:gamma", None, 16).await.unwrap();

        let fed_env = FederationEnvelope {
            source_mesh: "mesh:beta".into(),
            target_mesh: "mesh:gamma".into(),
            envelope: serde_json::json!({}),
            relayed_at: Utc::now(),
            ttl: 0,
        };

        let err = router.forward("mesh:gamma", fed_env).await.unwrap_err();
        assert!(matches!(err, FederationError::TtlExpired));
    }

    #[tokio::test]
    async fn should_accept_checks_target_mesh() {
        let router = make_router("mesh:alpha");

        let local_env = FederationEnvelope {
            source_mesh: "mesh:beta".into(),
            target_mesh: "mesh:alpha".into(),
            envelope: serde_json::json!({}),
            relayed_at: Utc::now(),
            ttl: 5,
        };
        assert!(router.should_accept(&local_env));

        let remote_env = FederationEnvelope {
            source_mesh: "mesh:beta".into(),
            target_mesh: "mesh:gamma".into(),
            envelope: serde_json::json!({}),
            relayed_at: Utc::now(),
            ttl: 5,
        };
        assert!(!router.should_accept(&remote_env));

        let expired_env = FederationEnvelope {
            source_mesh: "mesh:beta".into(),
            target_mesh: "mesh:alpha".into(),
            envelope: serde_json::json!({}),
            relayed_at: Utc::now(),
            ttl: 0,
        };
        assert!(!router.should_accept(&expired_env));
    }

    #[tokio::test]
    async fn list_linked_meshes() {
        let router = make_router("mesh:alpha");
        router
            .link_mesh("mesh:beta", Some("Beta"), 16)
            .await
            .unwrap();
        router
            .link_mesh("mesh:gamma", Some("Gamma"), 16)
            .await
            .unwrap();

        let mut links = router.linked_meshes().await;
        links.sort_by(|a, b| a.mesh_id.cmp(&b.mesh_id));

        assert_eq!(links.len(), 2);
        assert_eq!(links[0].mesh_id, "mesh:beta");
        assert_eq!(links[0].label.as_deref(), Some("Beta"));
        assert_eq!(links[1].mesh_id, "mesh:gamma");
    }

    #[tokio::test]
    async fn bidirectional_federation_pair() {
        let router_a = make_router("mesh:alpha");
        let router_b = make_router("mesh:beta");

        let (mut rx_a, mut rx_b) = federation_pair(&router_a, &router_b, 16).await.unwrap();

        // Alpha sends to Beta
        let env = serde_json::json!({"intent": "announce", "from": "alpha"});
        router_a.relay("mesh:beta", env.clone()).await.unwrap();
        let received = rx_b.recv().await.unwrap();
        assert_eq!(received.source_mesh, "mesh:alpha");
        assert_eq!(received.envelope, env);

        // Beta sends to Alpha
        let env = serde_json::json!({"intent": "offer", "from": "beta"});
        router_b.relay("mesh:alpha", env.clone()).await.unwrap();
        let received = rx_a.recv().await.unwrap();
        assert_eq!(received.source_mesh, "mesh:beta");
        assert_eq!(received.envelope, env);
    }

    #[tokio::test]
    async fn multi_hop_relay() {
        // Three meshes: alpha → beta → gamma
        let router_a = make_router("mesh:alpha");
        let router_b = make_router("mesh:beta");
        let router_c = make_router("mesh:gamma");

        // A links to B, B links to C
        let mut rx_ab = router_a.link_mesh("mesh:beta", None, 16).await.unwrap();
        let mut _rx_ba = router_b.link_mesh("mesh:alpha", None, 16).await.unwrap();
        let mut rx_bc = router_b.link_mesh("mesh:gamma", None, 16).await.unwrap();
        let mut _rx_cb = router_c.link_mesh("mesh:beta", None, 16).await.unwrap();

        // Alpha sends to Beta
        let env = serde_json::json!({"intent": "announce", "target": "mesh:gamma"});
        router_a.relay("mesh:beta", env).await.unwrap();

        // Beta receives it
        let received = rx_ab.recv().await.unwrap();
        assert_eq!(received.source_mesh, "mesh:alpha");
        assert_eq!(received.ttl, FederationRouter::DEFAULT_TTL);

        // Beta forwards to Gamma
        let forwarded = FederationEnvelope {
            target_mesh: "mesh:gamma".into(),
            ..received
        };
        router_b.forward("mesh:gamma", forwarded).await.unwrap();

        // Gamma receives it with decremented TTL
        let received = rx_bc.recv().await.unwrap();
        assert_eq!(received.source_mesh, "mesh:alpha");
        assert_eq!(received.ttl, FederationRouter::DEFAULT_TTL - 1);
        assert!(router_c.should_accept(&received));
    }

    #[tokio::test]
    async fn events_emitted_on_link_and_unlink() {
        let bus = EventBus::default();
        let mut rx_events = bus.subscribe();
        let router = FederationRouter::new("mesh:alpha", bus);

        router.link_mesh("mesh:beta", None, 16).await.unwrap();
        let event = rx_events.recv().await.unwrap();
        match event {
            Event::PatchReady { mesh_id, description } => {
                assert_eq!(mesh_id, "mesh:alpha");
                assert!(description.contains("mesh:beta"));
                assert!(description.contains("established"));
            }
            _ => panic!("expected PatchReady"),
        }

        router.unlink_mesh("mesh:beta").await.unwrap();
        let event = rx_events.recv().await.unwrap();
        match event {
            Event::PatchReady { mesh_id, description } => {
                assert_eq!(mesh_id, "mesh:alpha");
                assert!(description.contains("removed"));
            }
            _ => panic!("expected PatchReady"),
        }
    }
}
