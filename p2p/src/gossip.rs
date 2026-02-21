use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};

use crate::discovery::{GossipPull, GossipPush, MeshRegistry};
use crate::events::EventBus;

/// Envelope for gossip protocol messages exchanged between nodes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum GossipMessage {
    /// Initiate a gossip round: "here is my digest"
    Pull(GossipPull),
    /// Response with entries the requester is missing
    Push(GossipPush),
}

/// A connected gossip peer (another node in the mesh).
#[derive(Clone)]
struct GossipPeer {
    tx: mpsc::Sender<GossipMessage>,
}

/// Manages gossip protocol communication between mesh nodes.
///
/// Each node maintains a set of connected gossip peers. Periodically (or on
/// demand), the node initiates a gossip round with each peer: it sends a Pull
/// containing its local digest, receives a Push with missing/stale entries,
/// and merges them into the local registry.
#[derive(Clone)]
pub struct GossipRouter {
    node_id: String,
    registry: MeshRegistry,
    peers: Arc<RwLock<HashMap<String, GossipPeer>>>,
    #[allow(dead_code)]
    bus: EventBus,
}

impl GossipRouter {
    pub fn new(node_id: &str, registry: MeshRegistry, bus: EventBus) -> Self {
        Self {
            node_id: node_id.to_string(),
            registry,
            peers: Arc::new(RwLock::new(HashMap::new())),
            bus,
        }
    }

    /// Connect to a gossip peer. Returns a receiver for incoming messages from
    /// this node that the peer should process.
    pub async fn add_peer(
        &self,
        peer_node_id: &str,
        buffer: usize,
    ) -> mpsc::Receiver<GossipMessage> {
        let (tx, rx) = mpsc::channel(buffer);
        self.peers
            .write()
            .await
            .insert(peer_node_id.to_string(), GossipPeer { tx });
        tracing::info!(node_id = %self.node_id, peer = %peer_node_id, "gossip peer added");
        rx
    }

    /// Remove a gossip peer.
    pub async fn remove_peer(&self, peer_node_id: &str) -> bool {
        let removed = self.peers.write().await.remove(peer_node_id).is_some();
        if removed {
            tracing::info!(node_id = %self.node_id, peer = %peer_node_id, "gossip peer removed");
        }
        removed
    }

    /// Initiate a gossip pull round: send our digest to a specific peer.
    pub async fn initiate_pull(&self, peer_node_id: &str) -> Result<(), GossipError> {
        let digest = self.registry.build_digest().await;
        let pull = GossipPull {
            from: self.node_id.clone(),
            digests: digest,
        };

        let peers = self.peers.read().await;
        let peer = peers
            .get(peer_node_id)
            .ok_or_else(|| GossipError::PeerNotConnected(peer_node_id.to_string()))?;

        peer.tx
            .send(GossipMessage::Pull(pull))
            .await
            .map_err(|_| GossipError::SendFailed(peer_node_id.to_string()))?;

        tracing::debug!(
            node_id = %self.node_id,
            peer = %peer_node_id,
            "gossip pull initiated"
        );
        Ok(())
    }

    /// Initiate a gossip round with all connected peers.
    pub async fn initiate_pull_all(&self) -> Vec<(String, Result<(), GossipError>)> {
        let peer_ids: Vec<String> = self.peers.read().await.keys().cloned().collect();
        let mut results = Vec::with_capacity(peer_ids.len());
        for peer_id in peer_ids {
            let result = self.initiate_pull(&peer_id).await;
            results.push((peer_id, result));
        }
        results
    }

    /// Handle an incoming gossip message from a peer.
    /// - Pull: respond with a Push containing entries they're missing
    /// - Push: merge the entries into our local registry
    pub async fn handle_message(
        &self,
        from_peer: &str,
        msg: GossipMessage,
    ) -> Result<(), GossipError> {
        match msg {
            GossipMessage::Pull(pull) => {
                let push = self.registry.handle_gossip_pull(pull).await;
                let entry_count = push.entries.len();

                let peers = self.peers.read().await;
                let peer = peers
                    .get(from_peer)
                    .ok_or_else(|| GossipError::PeerNotConnected(from_peer.to_string()))?;

                peer.tx
                    .send(GossipMessage::Push(push))
                    .await
                    .map_err(|_| GossipError::SendFailed(from_peer.to_string()))?;

                tracing::debug!(
                    node_id = %self.node_id,
                    peer = %from_peer,
                    entries = entry_count,
                    "gossip push sent in response to pull"
                );
            }
            GossipMessage::Push(push) => {
                let entry_count = push.entries.len();
                self.registry.merge_gossip_push(push).await;

                tracing::debug!(
                    node_id = %self.node_id,
                    peer = %from_peer,
                    entries = entry_count,
                    "gossip push merged"
                );
            }
        }
        Ok(())
    }

    /// Return the count of connected gossip peers.
    pub async fn peer_count(&self) -> usize {
        self.peers.read().await.len()
    }

    /// Return the node ID of this gossip router.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Spawn a background task that periodically initiates gossip rounds.
    /// Returns a handle that can be used to cancel the task.
    pub fn spawn_periodic_gossip(&self, interval: Duration) -> tokio::task::JoinHandle<()> {
        let router = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                let results = router.initiate_pull_all().await;
                for (peer_id, result) in &results {
                    if let Err(e) = result {
                        tracing::warn!(
                            node_id = %router.node_id,
                            peer = %peer_id,
                            error = %e,
                            "periodic gossip pull failed"
                        );
                    }
                }
            }
        })
    }
}

/// Create a linked pair of gossip routers for testing, each pre-connected to the other.
pub async fn gossip_pair(
    node_a_id: &str,
    registry_a: MeshRegistry,
    node_b_id: &str,
    registry_b: MeshRegistry,
    bus: EventBus,
) -> (
    GossipRouter,
    mpsc::Receiver<GossipMessage>,
    GossipRouter,
    mpsc::Receiver<GossipMessage>,
) {
    let router_a = GossipRouter::new(node_a_id, registry_a, bus.clone());
    let router_b = GossipRouter::new(node_b_id, registry_b, bus);

    let rx_from_a = router_b.add_peer(node_a_id, 32).await;
    let rx_from_b = router_a.add_peer(node_b_id, 32).await;

    (router_a, rx_from_b, router_b, rx_from_a)
}

#[derive(Debug, thiserror::Error)]
pub enum GossipError {
    #[error("gossip peer not connected: {0}")]
    PeerNotConnected(String),
    #[error("failed to send gossip message to peer: {0}")]
    SendFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{AnnounceMessage, PeerStatus};

    fn make_registry(bus: &EventBus) -> MeshRegistry {
        MeshRegistry::new("mesh:test", bus.clone())
    }

    fn announce(id: &str) -> AnnounceMessage {
        AnnounceMessage {
            agent_id: format!("agent:{id}"),
            mesh_id: "mesh:test".into(),
            display_name: id.to_string(),
            status: PeerStatus::Available,
            capabilities: vec!["announce".into()],
        }
    }

    #[tokio::test]
    async fn gossip_pair_sync() {
        let bus = EventBus::default();
        let reg_a = make_registry(&bus);
        let reg_b = make_registry(&bus);

        // Node A knows about alice and bob
        reg_a.handle_announce(announce("alice")).await;
        reg_a.handle_announce(announce("bob")).await;

        // Node B knows about alice only
        reg_b.handle_announce(announce("alice")).await;

        let (router_a, mut rx_from_b, router_b, mut rx_from_a) =
            gossip_pair("node-a", reg_a.clone(), "node-b", reg_b.clone(), bus).await;

        // Node B initiates a pull (sends its digest to Node A)
        router_b.initiate_pull("node-a").await.unwrap();

        // Node A receives the pull
        let msg = rx_from_a.recv().await.unwrap();
        assert!(matches!(msg, GossipMessage::Pull(_)));

        // Node A handles it (responds with push containing bob)
        router_a.handle_message("node-b", msg).await.unwrap();

        // Node B receives the push
        let msg = rx_from_b.recv().await.unwrap();
        assert!(matches!(msg, GossipMessage::Push(_)));

        // Node B merges the push
        router_b.handle_message("node-a", msg).await.unwrap();

        // Now B should know about bob too
        assert_eq!(reg_b.peer_count().await, 2);
        assert!(reg_b.get_peer("agent:bob").await.is_some());
    }

    #[tokio::test]
    async fn initiate_pull_all() {
        let bus = EventBus::default();
        let reg = make_registry(&bus);
        let router = GossipRouter::new("node-a", reg, bus.clone());

        // Add two peers
        let _rx1 = router.add_peer("node-b", 16).await;
        let _rx2 = router.add_peer("node-c", 16).await;

        let results = router.initiate_pull_all().await;
        assert_eq!(results.len(), 2);
        for (_, result) in &results {
            assert!(result.is_ok());
        }
    }

    #[tokio::test]
    async fn pull_to_unknown_peer_fails() {
        let bus = EventBus::default();
        let reg = make_registry(&bus);
        let router = GossipRouter::new("node-a", reg, bus);

        let err = router.initiate_pull("node-unknown").await.unwrap_err();
        assert!(matches!(err, GossipError::PeerNotConnected(_)));
    }

    #[tokio::test]
    async fn add_and_remove_peer() {
        let bus = EventBus::default();
        let reg = make_registry(&bus);
        let router = GossipRouter::new("node-a", reg, bus);

        let _rx = router.add_peer("node-b", 16).await;
        assert_eq!(router.peer_count().await, 1);

        assert!(router.remove_peer("node-b").await);
        assert_eq!(router.peer_count().await, 0);

        assert!(!router.remove_peer("node-b").await);
    }

    #[tokio::test]
    async fn bidirectional_gossip_sync() {
        let bus = EventBus::default();
        let reg_a = make_registry(&bus);
        let reg_b = make_registry(&bus);

        // A knows alice, B knows bob
        reg_a.handle_announce(announce("alice")).await;
        reg_b.handle_announce(announce("bob")).await;

        let (router_a, mut rx_from_b, router_b, mut rx_from_a) =
            gossip_pair("node-a", reg_a.clone(), "node-b", reg_b.clone(), bus).await;

        // Round 1: B pulls from A → B learns about alice
        router_b.initiate_pull("node-a").await.unwrap();
        let msg = rx_from_a.recv().await.unwrap();
        router_a.handle_message("node-b", msg).await.unwrap();
        let msg = rx_from_b.recv().await.unwrap();
        router_b.handle_message("node-a", msg).await.unwrap();

        assert_eq!(reg_b.peer_count().await, 2);

        // Round 2: A pulls from B → A learns about bob
        router_a.initiate_pull("node-b").await.unwrap();
        let msg = rx_from_b.recv().await.unwrap();
        router_b.handle_message("node-a", msg).await.unwrap();
        let msg = rx_from_a.recv().await.unwrap();
        router_a.handle_message("node-b", msg).await.unwrap();

        assert_eq!(reg_a.peer_count().await, 2);

        // Both nodes now know about both peers
        assert!(reg_a.get_peer("agent:bob").await.is_some());
        assert!(reg_b.get_peer("agent:alice").await.is_some());
    }

    #[tokio::test]
    async fn empty_registries_gossip_noop() {
        let bus = EventBus::default();
        let reg_a = make_registry(&bus);
        let reg_b = make_registry(&bus);

        let (router_a, mut rx_from_b, _router_b, mut rx_from_a) =
            gossip_pair("node-a", reg_a.clone(), "node-b", reg_b.clone(), bus).await;

        // A pulls from B (both empty)
        router_a.initiate_pull("node-b").await.unwrap();
        let msg = rx_from_b.recv().await.unwrap();
        assert!(matches!(msg, GossipMessage::Pull(ref p) if p.digests.is_empty()));

        // B handles pull from A — push should be empty
        _router_b.handle_message("node-a", msg).await.unwrap();
        let msg = rx_from_a.recv().await.unwrap();
        match &msg {
            GossipMessage::Push(push) => assert!(push.entries.is_empty()),
            _ => panic!("expected Push"),
        }
    }

    #[tokio::test]
    async fn periodic_gossip_runs() {
        let bus = EventBus::default();
        let reg = make_registry(&bus);
        let router = GossipRouter::new("node-a", reg, bus);

        let mut rx = router.add_peer("node-b", 32).await;
        let handle = router.spawn_periodic_gossip(Duration::from_millis(50));

        // Should receive at least one pull within 200ms
        let msg = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("should receive gossip pull within timeout")
            .expect("channel should not be closed");

        assert!(matches!(msg, GossipMessage::Pull(_)));

        handle.abort();
    }
}
