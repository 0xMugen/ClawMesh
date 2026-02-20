use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

/// SDP offer from the initiating peer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionOffer {
    pub from: String,
    pub to: String,
    pub sdp: String,
}

/// SDP answer from the receiving peer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionAnswer {
    pub from: String,
    pub to: String,
    pub sdp: String,
}

/// ICE candidate for NAT traversal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IceCandidate {
    pub from: String,
    pub to: String,
    pub candidate: String,
    pub sdp_m_line_index: u32,
}

/// Union of all signaling message types.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalMessage {
    Offer(SessionOffer),
    Answer(SessionAnswer),
    IceCandidate(IceCandidate),
}

#[derive(Debug, Error)]
pub enum SignalError {
    #[error("signaling channel closed")]
    ChannelClosed,
    #[error("unknown peer: {0}")]
    UnknownPeer(String),
}

/// In-memory MPSC-based signaling transport for forwarding WebRTC negotiation messages.
#[derive(Debug)]
pub struct SignalingChannel {
    tx: mpsc::Sender<SignalMessage>,
    rx: mpsc::Receiver<SignalMessage>,
}

impl SignalingChannel {
    pub fn new(buffer: usize) -> (mpsc::Sender<SignalMessage>, Self) {
        let (tx, rx) = mpsc::channel(buffer);
        let sender = tx.clone();
        (sender, Self { tx, rx })
    }

    /// Send a signaling message.
    pub async fn send(&self, msg: SignalMessage) -> Result<(), SignalError> {
        self.tx
            .send(msg)
            .await
            .map_err(|_| SignalError::ChannelClosed)
    }

    /// Receive the next signaling message.
    pub async fn recv(&mut self) -> Result<SignalMessage, SignalError> {
        self.rx.recv().await.ok_or(SignalError::ChannelClosed)
    }

    /// Clone the sender half for multi-producer use.
    pub fn sender(&self) -> mpsc::Sender<SignalMessage> {
        self.tx.clone()
    }
}

/// Create a linked pair of signaling channels for testing.
pub fn signaling_pair() -> (SignalingChannel, SignalingChannel) {
    let (tx_a, rx_a) = mpsc::channel(32);
    let (tx_b, rx_b) = mpsc::channel(32);
    (
        SignalingChannel { tx: tx_b, rx: rx_a },
        SignalingChannel { tx: tx_a, rx: rx_b },
    )
}

/// Stub: relay a signal message toward the target peer.
/// In production this would look up the peer's signaling channel and forward.
pub async fn relay_signal(_target_agent_id: &str, msg: SignalMessage) -> Result<(), SignalError> {
    tracing::info!(?msg, "relay_signal stub: would forward to target peer");
    Ok(())
}

// ---------------------------------------------------------------------------
// NAT traversal helpers
// ---------------------------------------------------------------------------

/// Parsed representation of a STUN/TURN server endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Default public STUN servers useful for development/testing.
pub fn default_ice_servers() -> Vec<IceServer> {
    vec![
        IceServer {
            urls: vec!["stun:stun.l.google.com:19302".into()],
            username: None,
            credential: None,
        },
        IceServer {
            urls: vec!["stun:stun1.l.google.com:19302".into()],
            username: None,
            credential: None,
        },
    ]
}

/// Build an ICE server list, merging user-provided TURN servers with defaults.
pub fn build_ice_config(turn_servers: Vec<IceServer>) -> Vec<IceServer> {
    let mut servers = default_ice_servers();
    servers.extend(turn_servers);
    servers
}

/// Helper: extract the target peer id from a signal message.
pub fn signal_target(msg: &SignalMessage) -> &str {
    match msg {
        SignalMessage::Offer(o) => &o.to,
        SignalMessage::Answer(a) => &a.to,
        SignalMessage::IceCandidate(c) => &c.to,
    }
}

/// Helper: extract the source peer id from a signal message.
pub fn signal_source(msg: &SignalMessage) -> &str {
    match msg {
        SignalMessage::Offer(o) => &o.from,
        SignalMessage::Answer(a) => &a.from,
        SignalMessage::IceCandidate(c) => &c.from,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn signaling_pair_roundtrip() {
        let (mut a, mut b) = signaling_pair();

        let offer = SignalMessage::Offer(SessionOffer {
            from: "agent:alice".into(),
            to: "agent:bob".into(),
            sdp: "v=0\r\n...".into(),
        });
        a.send(offer.clone()).await.unwrap();

        let received = b.recv().await.unwrap();
        match received {
            SignalMessage::Offer(o) => {
                assert_eq!(o.from, "agent:alice");
                assert_eq!(o.to, "agent:bob");
            }
            _ => panic!("expected Offer"),
        }

        let answer = SignalMessage::Answer(SessionAnswer {
            from: "agent:bob".into(),
            to: "agent:alice".into(),
            sdp: "v=0\r\nanswer".into(),
        });
        b.send(answer).await.unwrap();

        let received = a.recv().await.unwrap();
        match received {
            SignalMessage::Answer(a) => {
                assert_eq!(a.from, "agent:bob");
                assert_eq!(a.to, "agent:alice");
            }
            _ => panic!("expected Answer"),
        }
    }

    #[tokio::test]
    async fn ice_candidate_roundtrip() {
        let (a, mut b) = signaling_pair();

        let candidate = SignalMessage::IceCandidate(IceCandidate {
            from: "agent:alice".into(),
            to: "agent:bob".into(),
            candidate: "candidate:1 1 UDP 2122252543 192.168.1.1 50000 typ host".into(),
            sdp_m_line_index: 0,
        });
        a.send(candidate).await.unwrap();

        let received = b.recv().await.unwrap();
        match received {
            SignalMessage::IceCandidate(c) => {
                assert_eq!(c.from, "agent:alice");
                assert!(c.candidate.contains("typ host"));
                assert_eq!(c.sdp_m_line_index, 0);
            }
            _ => panic!("expected IceCandidate"),
        }
    }

    #[test]
    fn default_ice_servers_returns_stun() {
        let servers = default_ice_servers();
        assert!(!servers.is_empty());
        for s in &servers {
            assert!(s.urls[0].starts_with("stun:"));
            assert!(s.username.is_none());
        }
    }

    #[test]
    fn build_ice_config_merges_turn() {
        let turn = vec![IceServer {
            urls: vec!["turn:my-turn.example.com:3478".into()],
            username: Some("user".into()),
            credential: Some("pass".into()),
        }];
        let config = build_ice_config(turn);
        assert!(config.len() >= 3);
        assert!(config.last().unwrap().urls[0].starts_with("turn:"));
    }

    #[test]
    fn signal_target_and_source() {
        let msg = SignalMessage::Offer(SessionOffer {
            from: "agent:alice".into(),
            to: "agent:bob".into(),
            sdp: "sdp".into(),
        });
        assert_eq!(signal_target(&msg), "agent:bob");
        assert_eq!(signal_source(&msg), "agent:alice");
    }
}
