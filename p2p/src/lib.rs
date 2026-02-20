pub mod auth;
pub mod channel;
pub mod crypto;
pub mod discovery;
pub mod events;
pub mod federation;
pub mod gossip;
pub mod matchmaking;
pub mod node;
pub mod relay;
pub mod room;
pub mod room_state;
pub mod signal;

pub use auth::{AuthChallenge, AuthConfirm, AuthError, AuthHello, AuthResponse, PeerAuthenticator};
pub use channel::{ChannelError, EncryptedChannel};
pub use crypto::{CryptoError, PeerIdentity, PublicKeyBytes, Signature};
pub use discovery::{AnnounceMessage, MeshRegistry, PeerEntry, PeerQuery, PeerStatus};
pub use events::{Event, EventBus};
pub use federation::{FederationEnvelope, FederationError, FederationRouter, MeshLink};
pub use gossip::{GossipError, GossipMessage, GossipRouter};
pub use matchmaking::{
    MatchError, MatchRequest, MatchResult, Matchmaker, PeerOffer, ScheduleSlot, SkillRange,
    SlotState, TimeWindow,
};
pub use node::MeshNode;
pub use relay::SignalRelay;
pub use room::{RoomError, RoomInfo, RoomManager};
pub use room_state::{
    ClockOrdering, RoomStateSync, StateDelta, StateEntry, StateError, StateSnapshot, VectorClock,
};
pub use signal::{
    IceCandidate, IceServer, SessionAnswer, SessionOffer, SignalError, SignalMessage,
    SignalingChannel,
};
