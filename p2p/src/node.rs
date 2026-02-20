use std::sync::Arc;
use std::time::Duration;

use crate::auth::PeerAuthenticator;
use crate::crypto::PeerIdentity;
use crate::discovery::MeshRegistry;
use crate::events::EventBus;
use crate::gossip::GossipRouter;
use crate::matchmaking::Matchmaker;
use crate::relay::SignalRelay;
use crate::room::RoomManager;
use crate::room_state::RoomStateSync;

/// A unified mesh participant that wires together all subsystems.
///
/// `MeshNode` owns the identity, registry, authenticator, rooms, state sync,
/// gossip router, signal relay, matchmaker, and event bus for a single node
/// in the mesh. It provides a single entry point for higher-level code
/// (e.g. the gateway) to interact with the mesh without manually threading
/// shared state.
#[derive(Clone)]
pub struct MeshNode {
    agent_id: String,
    mesh_id: String,
    identity: Arc<PeerIdentity>,
    bus: EventBus,
    registry: MeshRegistry,
    auth: PeerAuthenticator,
    rooms: RoomManager,
    state: RoomStateSync,
    gossip: GossipRouter,
    relay: SignalRelay,
    matchmaker: Matchmaker,
}

impl MeshNode {
    /// Create a new mesh node with a fresh identity.
    pub fn new(agent_id: &str, mesh_id: &str) -> Result<Self, crate::crypto::CryptoError> {
        let identity = PeerIdentity::generate()?;
        Ok(Self::with_identity(agent_id, mesh_id, identity))
    }

    /// Create a new mesh node with a provided identity.
    pub fn with_identity(agent_id: &str, mesh_id: &str, identity: PeerIdentity) -> Self {
        let bus = EventBus::default();
        let registry = MeshRegistry::new(mesh_id, bus.clone());
        let auth = PeerAuthenticator::new(agent_id, identity.clone_for_auth());
        let rooms = RoomManager::new(mesh_id, bus.clone());
        let state = RoomStateSync::new(mesh_id, bus.clone());
        let gossip = GossipRouter::new(agent_id, registry.clone(), bus.clone());
        let relay = SignalRelay::new(bus.clone());
        let matchmaker = Matchmaker::new(mesh_id, bus.clone());

        Self {
            agent_id: agent_id.to_string(),
            mesh_id: mesh_id.to_string(),
            identity: Arc::new(identity),
            bus,
            registry,
            auth,
            rooms,
            state,
            gossip,
            relay,
            matchmaker,
        }
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn mesh_id(&self) -> &str {
        &self.mesh_id
    }

    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    pub fn registry(&self) -> &MeshRegistry {
        &self.registry
    }

    pub fn auth(&self) -> &PeerAuthenticator {
        &self.auth
    }

    pub fn rooms(&self) -> &RoomManager {
        &self.rooms
    }

    pub fn state(&self) -> &RoomStateSync {
        &self.state
    }

    pub fn gossip(&self) -> &GossipRouter {
        &self.gossip
    }

    pub fn relay(&self) -> &SignalRelay {
        &self.relay
    }

    pub fn matchmaker(&self) -> &Matchmaker {
        &self.matchmaker
    }

    pub fn public_key(&self) -> crate::crypto::PublicKeyBytes {
        self.identity.public_key()
    }

    /// Sign an arbitrary byte message with this node's identity.
    pub fn sign(&self, message: &[u8]) -> crate::crypto::Signature {
        self.identity.sign(message)
    }

    /// Spawn background gossip at the given interval.
    pub fn start_gossip(&self, interval: Duration) -> tokio::task::JoinHandle<()> {
        self.gossip.spawn_periodic_gossip(interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{AnnounceMessage, PeerStatus};
    use crate::matchmaking::{SkillRange, TimeWindow};
    use chrono::{Duration as ChronoDuration, Utc};
    use serde_json::json;

    #[tokio::test]
    async fn node_subsystem_integration() {
        let node = MeshNode::new("agent:alice", "mesh:test").unwrap();

        // Registry works
        node.registry()
            .handle_announce(AnnounceMessage {
                agent_id: "agent:bob".into(),
                mesh_id: "mesh:test".into(),
                display_name: "Bob".into(),
                status: PeerStatus::Available,
                capabilities: vec!["matchmaking".into()],
            })
            .await;
        assert_eq!(node.registry().peer_count().await, 1);

        // Room works
        node.rooms().create_room("room:lobby", "Lobby", None).await;
        node.rooms()
            .join_room("room:lobby", "agent:alice")
            .await
            .unwrap();
        assert_eq!(node.rooms().member_count("room:lobby").await.unwrap(), 1);

        // State sync works
        node.state().init_room("room:lobby").await;
        node.state()
            .set("room:lobby", "agent:alice", "status", json!("ready"))
            .await
            .unwrap();
        let entry = node.state().get("room:lobby", "status").await.unwrap();
        assert_eq!(entry.value, json!("ready"));

        // Relay works
        let _rx = node.relay().register("agent:alice", 16).await;
        assert!(node.relay().is_registered("agent:alice").await);

        // Matchmaker works
        let req = node
            .matchmaker()
            .submit_request(
                "agent:alice",
                "golf-18",
                SkillRange {
                    min: 50.0,
                    max: 80.0,
                },
                TimeWindow {
                    start: Utc::now(),
                    end: Utc::now() + ChronoDuration::hours(2),
                },
            )
            .await;
        assert!(!req.request_id.is_empty());
    }

    #[test]
    fn node_accessors() {
        let node = MeshNode::new("agent:alice", "mesh:golf-sim").unwrap();
        assert_eq!(node.agent_id(), "agent:alice");
        assert_eq!(node.mesh_id(), "mesh:golf-sim");
        assert!(!node.public_key().key.is_empty());
    }

    #[test]
    fn node_sign() {
        let node = MeshNode::new("agent:alice", "mesh:test").unwrap();
        let sig = node.sign(b"hello mesh");
        assert!(!sig.sig.is_empty());

        // Verify with node's own public key
        let pk = node.public_key();
        crate::crypto::verify(&pk, b"hello mesh", &sig).unwrap();
    }
}
