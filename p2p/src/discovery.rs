use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::events::{Event, EventBus};

/// Status of a peer in the mesh.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerStatus {
    Available,
    Busy,
    Away,
}

/// A registered peer in the mesh.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerEntry {
    pub agent_id: String,
    pub mesh_id: String,
    pub display_name: String,
    pub status: PeerStatus,
    pub capabilities: Vec<String>,
    pub last_seen: DateTime<Utc>,
}

/// Payload when a peer announces/updates its presence.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnnounceMessage {
    pub agent_id: String,
    pub mesh_id: String,
    pub display_name: String,
    pub status: PeerStatus,
    pub capabilities: Vec<String>,
}

/// Query parameters for searching peers.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PeerQuery {
    pub status: Option<PeerStatus>,
    pub capability: Option<String>,
    pub mesh_id: Option<String>,
}

/// Gossip digest entry: agent_id + last_seen timestamp.
/// Peers exchange digests to discover missing or stale entries.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GossipDigest {
    pub agent_id: String,
    pub last_seen: DateTime<Utc>,
}

/// Gossip pull request: "I know about these peers at these timestamps."
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GossipPull {
    pub from: String,
    pub digests: Vec<GossipDigest>,
}

/// Gossip push response: "Here are entries you're missing or that are newer."
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GossipPush {
    pub entries: Vec<PeerEntry>,
}

/// In-memory registry of peers in a single mesh (thread-safe).
#[derive(Clone)]
pub struct MeshRegistry {
    mesh_id: String,
    peers: Arc<RwLock<HashMap<String, PeerEntry>>>,
    bus: EventBus,
}

impl MeshRegistry {
    pub fn new(mesh_id: &str, bus: EventBus) -> Self {
        Self {
            mesh_id: mesh_id.to_string(),
            peers: Arc::new(RwLock::new(HashMap::new())),
            bus,
        }
    }

    /// Handle an announce message: register or update the peer.
    pub async fn handle_announce(&self, msg: AnnounceMessage) -> PeerEntry {
        let entry = PeerEntry {
            agent_id: msg.agent_id.clone(),
            mesh_id: msg.mesh_id.clone(),
            display_name: msg.display_name,
            status: msg.status,
            capabilities: msg.capabilities,
            last_seen: Utc::now(),
        };

        let mut peers = self.peers.write().await;
        peers.insert(msg.agent_id.clone(), entry.clone());
        drop(peers);

        self.bus.emit(Event::PeerAnnounced {
            agent_id: msg.agent_id.clone(),
            mesh_id: msg.mesh_id,
        });
        self.bus
            .emit_patch_ready(&self.mesh_id, &format!("peer {} announced", msg.agent_id));

        entry
    }

    /// Remove a peer from the registry.
    pub async fn remove_peer(&self, agent_id: &str) -> Option<PeerEntry> {
        let mut peers = self.peers.write().await;
        let removed = peers.remove(agent_id);
        drop(peers);

        if removed.is_some() {
            self.bus.emit(Event::PeerDeparted {
                agent_id: agent_id.to_string(),
                mesh_id: self.mesh_id.clone(),
            });
            self.bus
                .emit_patch_ready(&self.mesh_id, &format!("peer {} departed", agent_id));
        }

        removed
    }

    /// Look up a single peer.
    pub async fn get_peer(&self, agent_id: &str) -> Option<PeerEntry> {
        self.peers.read().await.get(agent_id).cloned()
    }

    /// List all peers in the mesh.
    pub async fn list_peers(&self) -> Vec<PeerEntry> {
        self.peers.read().await.values().cloned().collect()
    }

    /// Filter peers by status.
    pub async fn peers_by_status(&self, status: PeerStatus) -> Vec<PeerEntry> {
        self.peers
            .read()
            .await
            .values()
            .filter(|p| p.status == status)
            .cloned()
            .collect()
    }

    /// Count of registered peers.
    pub async fn peer_count(&self) -> usize {
        self.peers.read().await.len()
    }

    /// Find peers that advertise a specific capability.
    pub async fn peers_by_capability(&self, capability: &str) -> Vec<PeerEntry> {
        self.peers
            .read()
            .await
            .values()
            .filter(|p| p.capabilities.iter().any(|c| c == capability))
            .cloned()
            .collect()
    }

    /// Search peers by a combination of filters.
    pub async fn search_peers(&self, query: &PeerQuery) -> Vec<PeerEntry> {
        self.peers
            .read()
            .await
            .values()
            .filter(|p| {
                // Filter by status if specified
                if let Some(status) = query.status {
                    if p.status != status {
                        return false;
                    }
                }
                // Filter by capability if specified
                if let Some(ref cap) = query.capability {
                    if !p.capabilities.iter().any(|c| c == cap) {
                        return false;
                    }
                }
                // Filter by mesh_id if specified
                if let Some(ref mesh) = query.mesh_id {
                    if p.mesh_id != *mesh {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect()
    }

    // -----------------------------------------------------------------------
    // Gossip protocol helpers
    // -----------------------------------------------------------------------

    /// Build a gossip digest of our current peer table.
    pub async fn build_digest(&self) -> Vec<GossipDigest> {
        self.peers
            .read()
            .await
            .values()
            .map(|p| GossipDigest {
                agent_id: p.agent_id.clone(),
                last_seen: p.last_seen,
            })
            .collect()
    }

    /// Handle an incoming gossip pull: compare digests and return entries
    /// that the requester is missing or has stale versions of.
    pub async fn handle_gossip_pull(&self, pull: GossipPull) -> GossipPush {
        let peers = self.peers.read().await;
        let remote: HashMap<&str, &GossipDigest> = pull
            .digests
            .iter()
            .map(|d| (d.agent_id.as_str(), d))
            .collect();

        let mut entries = Vec::new();
        for (id, entry) in peers.iter() {
            match remote.get(id.as_str()) {
                None => entries.push(entry.clone()),
                Some(digest) if entry.last_seen > digest.last_seen => {
                    entries.push(entry.clone());
                }
                _ => {}
            }
        }

        GossipPush { entries }
    }

    /// Merge a gossip push into our local peer table (only if newer).
    pub async fn merge_gossip_push(&self, push: GossipPush) {
        let mut peers = self.peers.write().await;
        for entry in push.entries {
            let dominated = peers
                .get(&entry.agent_id)
                .map_or(true, |existing| entry.last_seen > existing.last_seen);

            if dominated {
                let agent_id = entry.agent_id.clone();
                let mesh_id = entry.mesh_id.clone();
                peers.insert(agent_id.clone(), entry);
                self.bus.emit(Event::PeerAnnounced { agent_id, mesh_id });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_registry() -> MeshRegistry {
        MeshRegistry::new("mesh:test", EventBus::default())
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
    async fn announce_and_lookup() {
        let reg = make_registry();
        reg.handle_announce(announce("alice")).await;

        let peer = reg.get_peer("agent:alice").await.unwrap();
        assert_eq!(peer.display_name, "alice");
        assert_eq!(peer.status, PeerStatus::Available);
        assert_eq!(reg.peer_count().await, 1);
    }

    #[tokio::test]
    async fn remove_peer() {
        let reg = make_registry();
        reg.handle_announce(announce("alice")).await;
        assert_eq!(reg.peer_count().await, 1);

        let removed = reg.remove_peer("agent:alice").await;
        assert!(removed.is_some());
        assert_eq!(reg.peer_count().await, 0);
    }

    #[tokio::test]
    async fn remove_nonexistent() {
        let reg = make_registry();
        assert!(reg.remove_peer("agent:ghost").await.is_none());
    }

    #[tokio::test]
    async fn list_and_filter() {
        let reg = make_registry();
        reg.handle_announce(announce("alice")).await;

        let mut msg = announce("bob");
        msg.status = PeerStatus::Busy;
        reg.handle_announce(msg).await;

        assert_eq!(reg.list_peers().await.len(), 2);
        assert_eq!(reg.peers_by_status(PeerStatus::Available).await.len(), 1);
        assert_eq!(reg.peers_by_status(PeerStatus::Busy).await.len(), 1);
    }

    #[tokio::test]
    async fn peers_by_capability() {
        let reg = make_registry();

        let mut alice = announce("alice");
        alice.capabilities = vec!["matchmaking".into(), "scheduling".into()];
        reg.handle_announce(alice).await;

        let mut bob = announce("bob");
        bob.capabilities = vec!["matchmaking".into()];
        reg.handle_announce(bob).await;

        let mut carol = announce("carol");
        carol.capabilities = vec!["scheduling".into()];
        reg.handle_announce(carol).await;

        assert_eq!(reg.peers_by_capability("matchmaking").await.len(), 2);
        assert_eq!(reg.peers_by_capability("scheduling").await.len(), 2);
        assert_eq!(reg.peers_by_capability("unknown").await.len(), 0);
    }

    #[tokio::test]
    async fn search_peers_combined() {
        let reg = make_registry();

        let mut alice = announce("alice");
        alice.capabilities = vec!["matchmaking".into()];
        reg.handle_announce(alice).await;

        let mut bob = announce("bob");
        bob.status = PeerStatus::Busy;
        bob.capabilities = vec!["matchmaking".into()];
        reg.handle_announce(bob).await;

        // Search for available matchmaking peers
        let query = PeerQuery {
            status: Some(PeerStatus::Available),
            capability: Some("matchmaking".into()),
            mesh_id: None,
        };
        let results = reg.search_peers(&query).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].agent_id, "agent:alice");

        // Search with no filters returns all
        let all = reg.search_peers(&PeerQuery::default()).await;
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn events_emitted_on_announce() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        let reg = MeshRegistry::new("mesh:test", bus);

        reg.handle_announce(announce("alice")).await;

        // Should receive PeerAnnounced and PatchReady
        let e1 = rx.recv().await.unwrap();
        assert!(matches!(e1, Event::PeerAnnounced { .. }));
        let e2 = rx.recv().await.unwrap();
        assert!(matches!(e2, Event::PatchReady { .. }));
    }

    #[tokio::test]
    async fn gossip_digest_roundtrip() {
        let reg_a = make_registry();
        reg_a.handle_announce(announce("alice")).await;
        reg_a.handle_announce(announce("bob")).await;

        let reg_b = make_registry();
        reg_b.handle_announce(announce("alice")).await;

        // B pulls from A: B knows about alice, A knows alice + bob
        let pull = GossipPull {
            from: "node-b".into(),
            digests: reg_b.build_digest().await,
        };

        let push = reg_a.handle_gossip_pull(pull).await;
        // A should send bob (which B doesn't have)
        assert_eq!(push.entries.len(), 1);
        assert_eq!(push.entries[0].agent_id, "agent:bob");

        // B merges the push
        reg_b.merge_gossip_push(push).await;
        assert_eq!(reg_b.peer_count().await, 2);
        assert!(reg_b.get_peer("agent:bob").await.is_some());
    }

    #[tokio::test]
    async fn gossip_merge_only_newer() {
        let reg = make_registry();
        reg.handle_announce(announce("alice")).await;
        let original = reg.get_peer("agent:alice").await.unwrap();

        // Create an older entry
        let mut stale = original.clone();
        stale.display_name = "stale-alice".into();
        stale.last_seen = chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        reg.merge_gossip_push(GossipPush {
            entries: vec![stale],
        })
        .await;

        // Should NOT overwrite with stale data
        let current = reg.get_peer("agent:alice").await.unwrap();
        assert_eq!(current.display_name, "alice");
    }
}
