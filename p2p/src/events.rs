use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Mesh lifecycle events emitted by discovery, signaling, and room subsystems.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    PeerAnnounced {
        agent_id: String,
        mesh_id: String,
    },
    PeerDeparted {
        agent_id: String,
        mesh_id: String,
    },
    SignalReceived {
        from: String,
        to: String,
    },
    RoomJoined {
        agent_id: String,
        room_id: String,
    },
    RoomLeft {
        agent_id: String,
        room_id: String,
    },
    PeerAuthenticated {
        agent_id: String,
        mesh_id: String,
    },
    PeerAuthFailed {
        agent_id: String,
        reason: String,
    },
    ChannelEstablished {
        from: String,
        to: String,
    },
    RoomStateChanged {
        room_id: String,
        key: String,
        author: String,
    },
    PatchReady {
        mesh_id: String,
        description: String,
    },
}

const DEFAULT_CAPACITY: usize = 256;

/// Broadcast-based event bus for decoupled mesh components.
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Broadcast an event to all subscribers. Returns the number of receivers.
    pub fn emit(&self, event: Event) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// Convenience: emit a PatchReady event.
    pub fn emit_patch_ready(&self, mesh_id: &str, description: &str) -> usize {
        self.emit(Event::PatchReady {
            mesh_id: mesh_id.to_string(),
            description: description.to_string(),
        })
    }

    /// Subscribe to the event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emit_and_receive() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();

        bus.emit(Event::PeerAnnounced {
            agent_id: "agent:alice".into(),
            mesh_id: "mesh:test".into(),
        });

        let event = rx.recv().await.unwrap();
        match event {
            Event::PeerAnnounced { agent_id, mesh_id } => {
                assert_eq!(agent_id, "agent:alice");
                assert_eq!(mesh_id, "mesh:test");
            }
            _ => panic!("unexpected event variant"),
        }
    }

    #[tokio::test]
    async fn patch_ready_convenience() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();

        bus.emit_patch_ready("mesh:golf", "peer joined");

        let event = rx.recv().await.unwrap();
        match event {
            Event::PatchReady {
                mesh_id,
                description,
            } => {
                assert_eq!(mesh_id, "mesh:golf");
                assert_eq!(description, "peer joined");
            }
            _ => panic!("expected PatchReady"),
        }
    }

    #[tokio::test]
    async fn no_subscribers_does_not_panic() {
        let bus = EventBus::default();
        let count = bus.emit(Event::PeerDeparted {
            agent_id: "agent:bob".into(),
            mesh_id: "mesh:test".into(),
        });
        assert_eq!(count, 0);
    }
}
