use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};

use crate::events::{Event, EventBus};
use crate::signal::{SignalError, SignalMessage, signal_source, signal_target};

/// A relay server that routes signaling messages between registered peers.
///
/// Each peer registers with a sender channel. When a signaling message arrives,
/// the relay looks up the target peer's channel and forwards the message.
/// This enables NAT traversal by acting as the rendezvous point for WebRTC
/// offer/answer/ICE exchange between peers that cannot directly reach each other.
#[derive(Clone)]
pub struct SignalRelay {
    peers: Arc<RwLock<HashMap<String, mpsc::Sender<SignalMessage>>>>,
    bus: EventBus,
}

impl SignalRelay {
    pub fn new(bus: EventBus) -> Self {
        Self {
            peers: Arc::new(RwLock::new(HashMap::new())),
            bus,
        }
    }

    /// Register a peer and return a receiver for incoming signaling messages.
    /// If the peer was already registered, the old channel is replaced.
    pub async fn register(&self, agent_id: &str, buffer: usize) -> mpsc::Receiver<SignalMessage> {
        let (tx, rx) = mpsc::channel(buffer);
        self.peers
            .write()
            .await
            .insert(agent_id.to_string(), tx);
        tracing::info!(%agent_id, "peer registered with signal relay");
        rx
    }

    /// Unregister a peer, dropping its sender channel.
    pub async fn unregister(&self, agent_id: &str) -> bool {
        let removed = self.peers.write().await.remove(agent_id).is_some();
        if removed {
            tracing::info!(%agent_id, "peer unregistered from signal relay");
        }
        removed
    }

    /// Forward a signaling message to the target peer.
    /// Returns an error if the target peer is not registered or the channel is full/closed.
    pub async fn relay(&self, msg: SignalMessage) -> Result<(), SignalError> {
        let target = signal_target(&msg).to_string();
        let source = signal_source(&msg).to_string();

        let peers = self.peers.read().await;
        let tx = peers
            .get(&target)
            .ok_or_else(|| SignalError::UnknownPeer(target.clone()))?;

        tx.send(msg)
            .await
            .map_err(|_| SignalError::ChannelClosed)?;

        self.bus.emit(Event::SignalReceived {
            from: source,
            to: target,
        });

        Ok(())
    }

    /// Check if a peer is currently registered.
    pub async fn is_registered(&self, agent_id: &str) -> bool {
        self.peers.read().await.contains_key(agent_id)
    }

    /// Return the number of registered peers.
    pub async fn peer_count(&self) -> usize {
        self.peers.read().await.len()
    }

    /// List all registered peer agent IDs.
    pub async fn registered_peers(&self) -> Vec<String> {
        self.peers.read().await.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{IceCandidate, SessionAnswer, SessionOffer};

    fn make_relay() -> SignalRelay {
        SignalRelay::new(EventBus::default())
    }

    #[tokio::test]
    async fn register_and_relay_offer() {
        let relay = make_relay();
        let mut rx_bob = relay.register("agent:bob", 16).await;

        let offer = SignalMessage::Offer(SessionOffer {
            from: "agent:alice".into(),
            to: "agent:bob".into(),
            sdp: "v=0\r\noffer-sdp".into(),
        });
        relay.relay(offer).await.unwrap();

        let received = rx_bob.recv().await.unwrap();
        match received {
            SignalMessage::Offer(o) => {
                assert_eq!(o.from, "agent:alice");
                assert_eq!(o.to, "agent:bob");
                assert_eq!(o.sdp, "v=0\r\noffer-sdp");
            }
            _ => panic!("expected Offer"),
        }
    }

    #[tokio::test]
    async fn full_offer_answer_ice_exchange() {
        let relay = make_relay();
        let mut rx_alice = relay.register("agent:alice", 16).await;
        let mut rx_bob = relay.register("agent:bob", 16).await;

        // Alice sends offer to Bob
        relay
            .relay(SignalMessage::Offer(SessionOffer {
                from: "agent:alice".into(),
                to: "agent:bob".into(),
                sdp: "offer-sdp".into(),
            }))
            .await
            .unwrap();

        let msg = rx_bob.recv().await.unwrap();
        assert!(matches!(msg, SignalMessage::Offer(_)));

        // Bob sends answer to Alice
        relay
            .relay(SignalMessage::Answer(SessionAnswer {
                from: "agent:bob".into(),
                to: "agent:alice".into(),
                sdp: "answer-sdp".into(),
            }))
            .await
            .unwrap();

        let msg = rx_alice.recv().await.unwrap();
        assert!(matches!(msg, SignalMessage::Answer(_)));

        // Alice sends ICE candidate to Bob
        relay
            .relay(SignalMessage::IceCandidate(IceCandidate {
                from: "agent:alice".into(),
                to: "agent:bob".into(),
                candidate: "candidate:1 1 UDP 2122252543 192.168.1.1 50000 typ host".into(),
                sdp_m_line_index: 0,
            }))
            .await
            .unwrap();

        let msg = rx_bob.recv().await.unwrap();
        assert!(matches!(msg, SignalMessage::IceCandidate(_)));
    }

    #[tokio::test]
    async fn relay_to_unknown_peer_fails() {
        let relay = make_relay();

        let offer = SignalMessage::Offer(SessionOffer {
            from: "agent:alice".into(),
            to: "agent:nobody".into(),
            sdp: "sdp".into(),
        });

        let err = relay.relay(offer).await.unwrap_err();
        assert!(matches!(err, SignalError::UnknownPeer(_)));
    }

    #[tokio::test]
    async fn unregister_peer() {
        let relay = make_relay();
        let _rx = relay.register("agent:alice", 16).await;
        assert!(relay.is_registered("agent:alice").await);
        assert_eq!(relay.peer_count().await, 1);

        assert!(relay.unregister("agent:alice").await);
        assert!(!relay.is_registered("agent:alice").await);
        assert_eq!(relay.peer_count().await, 0);
    }

    #[tokio::test]
    async fn unregister_nonexistent_returns_false() {
        let relay = make_relay();
        assert!(!relay.unregister("agent:ghost").await);
    }

    #[tokio::test]
    async fn reregister_replaces_channel() {
        let relay = make_relay();
        let _rx_old = relay.register("agent:alice", 16).await;
        let mut rx_new = relay.register("agent:alice", 16).await;

        // Old receiver is dropped; message should go to new channel
        relay
            .relay(SignalMessage::Offer(SessionOffer {
                from: "agent:bob".into(),
                to: "agent:alice".into(),
                sdp: "sdp".into(),
            }))
            .await
            .unwrap();

        let msg = rx_new.recv().await.unwrap();
        assert!(matches!(msg, SignalMessage::Offer(_)));
        assert_eq!(relay.peer_count().await, 1);
    }

    #[tokio::test]
    async fn relay_after_unregister_fails() {
        let relay = make_relay();
        let _rx = relay.register("agent:bob", 16).await;
        relay.unregister("agent:bob").await;

        let err = relay
            .relay(SignalMessage::Offer(SessionOffer {
                from: "agent:alice".into(),
                to: "agent:bob".into(),
                sdp: "sdp".into(),
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, SignalError::UnknownPeer(_)));
    }

    #[tokio::test]
    async fn events_emitted_on_relay() {
        let bus = EventBus::default();
        let mut rx_events = bus.subscribe();
        let relay = SignalRelay::new(bus);
        let _rx_bob = relay.register("agent:bob", 16).await;

        relay
            .relay(SignalMessage::Offer(SessionOffer {
                from: "agent:alice".into(),
                to: "agent:bob".into(),
                sdp: "sdp".into(),
            }))
            .await
            .unwrap();

        let event = rx_events.recv().await.unwrap();
        match event {
            Event::SignalReceived { from, to } => {
                assert_eq!(from, "agent:alice");
                assert_eq!(to, "agent:bob");
            }
            _ => panic!("expected SignalReceived"),
        }
    }

    #[tokio::test]
    async fn registered_peers_listing() {
        let relay = make_relay();
        relay.register("agent:alice", 16).await;
        relay.register("agent:bob", 16).await;
        relay.register("agent:carol", 16).await;

        let mut peers = relay.registered_peers().await;
        peers.sort();
        assert_eq!(peers, vec!["agent:alice", "agent:bob", "agent:carol"]);
    }
}
