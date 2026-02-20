use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::events::{Event, EventBus};

/// Metadata about a room within a mesh.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomInfo {
    pub room_id: String,
    pub mesh_id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub max_members: Option<usize>,
}

/// Manages rooms and their memberships within a mesh.
#[derive(Clone)]
pub struct RoomManager {
    mesh_id: String,
    rooms: Arc<RwLock<HashMap<String, RoomInfo>>>,
    members: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    bus: EventBus,
}

impl RoomManager {
    pub fn new(mesh_id: &str, bus: EventBus) -> Self {
        Self {
            mesh_id: mesh_id.to_string(),
            rooms: Arc::new(RwLock::new(HashMap::new())),
            members: Arc::new(RwLock::new(HashMap::new())),
            bus,
        }
    }

    /// Create a new room. Returns the room info or None if it already exists.
    pub async fn create_room(
        &self,
        room_id: &str,
        name: &str,
        max_members: Option<usize>,
    ) -> Option<RoomInfo> {
        let mut rooms = self.rooms.write().await;
        if rooms.contains_key(room_id) {
            return None;
        }

        let info = RoomInfo {
            room_id: room_id.to_string(),
            mesh_id: self.mesh_id.clone(),
            name: name.to_string(),
            created_at: Utc::now(),
            max_members,
        };
        rooms.insert(room_id.to_string(), info.clone());

        let mut members = self.members.write().await;
        members.insert(room_id.to_string(), HashSet::new());

        Some(info)
    }

    /// Join a room. Returns Ok(()) on success, Err with reason on failure.
    pub async fn join_room(&self, room_id: &str, agent_id: &str) -> Result<(), RoomError> {
        let rooms = self.rooms.read().await;
        let room = rooms.get(room_id).ok_or(RoomError::NotFound)?;

        let mut members = self.members.write().await;
        let member_set = members.get_mut(room_id).ok_or(RoomError::NotFound)?;

        if member_set.contains(agent_id) {
            return Err(RoomError::AlreadyJoined);
        }

        if let Some(max) = room.max_members {
            if member_set.len() >= max {
                return Err(RoomError::RoomFull);
            }
        }

        member_set.insert(agent_id.to_string());
        drop(members);
        drop(rooms);

        self.bus.emit(Event::RoomJoined {
            agent_id: agent_id.to_string(),
            room_id: room_id.to_string(),
        });
        self.bus.emit_patch_ready(
            &self.mesh_id,
            &format!("{} joined room {}", agent_id, room_id),
        );

        Ok(())
    }

    /// Leave a room. Returns Ok(()) on success.
    pub async fn leave_room(&self, room_id: &str, agent_id: &str) -> Result<(), RoomError> {
        let mut members = self.members.write().await;
        let member_set = members.get_mut(room_id).ok_or(RoomError::NotFound)?;

        if !member_set.remove(agent_id) {
            return Err(RoomError::NotMember);
        }
        drop(members);

        self.bus.emit(Event::RoomLeft {
            agent_id: agent_id.to_string(),
            room_id: room_id.to_string(),
        });
        self.bus.emit_patch_ready(
            &self.mesh_id,
            &format!("{} left room {}", agent_id, room_id),
        );

        Ok(())
    }

    /// List members of a room.
    pub async fn room_members(&self, room_id: &str) -> Result<Vec<String>, RoomError> {
        let members = self.members.read().await;
        let member_set = members.get(room_id).ok_or(RoomError::NotFound)?;
        Ok(member_set.iter().cloned().collect())
    }

    /// List all rooms.
    pub async fn list_rooms(&self) -> Vec<RoomInfo> {
        self.rooms.read().await.values().cloned().collect()
    }

    /// Get info about a specific room.
    pub async fn get_room(&self, room_id: &str) -> Option<RoomInfo> {
        self.rooms.read().await.get(room_id).cloned()
    }

    /// Count members in a room.
    pub async fn member_count(&self, room_id: &str) -> Result<usize, RoomError> {
        let members = self.members.read().await;
        let member_set = members.get(room_id).ok_or(RoomError::NotFound)?;
        Ok(member_set.len())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RoomError {
    #[error("room not found")]
    NotFound,
    #[error("already joined this room")]
    AlreadyJoined,
    #[error("room is full")]
    RoomFull,
    #[error("not a member of this room")]
    NotMember,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manager() -> RoomManager {
        RoomManager::new("mesh:test", EventBus::default())
    }

    #[tokio::test]
    async fn create_and_join() {
        let mgr = make_manager();
        let room = mgr.create_room("room:lobby", "Lobby", None).await;
        assert!(room.is_some());

        mgr.join_room("room:lobby", "agent:alice").await.unwrap();
        let members = mgr.room_members("room:lobby").await.unwrap();
        assert_eq!(members.len(), 1);
        assert!(members.contains(&"agent:alice".to_string()));
    }

    #[tokio::test]
    async fn duplicate_room_returns_none() {
        let mgr = make_manager();
        assert!(mgr.create_room("room:a", "A", None).await.is_some());
        assert!(mgr.create_room("room:a", "A2", None).await.is_none());
    }

    #[tokio::test]
    async fn join_nonexistent_room() {
        let mgr = make_manager();
        let err = mgr.join_room("room:missing", "agent:alice").await;
        assert!(matches!(err, Err(RoomError::NotFound)));
    }

    #[tokio::test]
    async fn double_join_rejected() {
        let mgr = make_manager();
        mgr.create_room("room:a", "A", None).await;
        mgr.join_room("room:a", "agent:alice").await.unwrap();
        let err = mgr.join_room("room:a", "agent:alice").await;
        assert!(matches!(err, Err(RoomError::AlreadyJoined)));
    }

    #[tokio::test]
    async fn room_full() {
        let mgr = make_manager();
        mgr.create_room("room:small", "Small", Some(1)).await;
        mgr.join_room("room:small", "agent:alice").await.unwrap();
        let err = mgr.join_room("room:small", "agent:bob").await;
        assert!(matches!(err, Err(RoomError::RoomFull)));
    }

    #[tokio::test]
    async fn leave_and_rejoin() {
        let mgr = make_manager();
        mgr.create_room("room:a", "A", None).await;
        mgr.join_room("room:a", "agent:alice").await.unwrap();
        mgr.leave_room("room:a", "agent:alice").await.unwrap();
        assert_eq!(mgr.member_count("room:a").await.unwrap(), 0);

        // Can rejoin after leaving
        mgr.join_room("room:a", "agent:alice").await.unwrap();
        assert_eq!(mgr.member_count("room:a").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn leave_nonmember() {
        let mgr = make_manager();
        mgr.create_room("room:a", "A", None).await;
        let err = mgr.leave_room("room:a", "agent:ghost").await;
        assert!(matches!(err, Err(RoomError::NotMember)));
    }

    #[tokio::test]
    async fn list_rooms() {
        let mgr = make_manager();
        mgr.create_room("room:a", "Alpha", None).await;
        mgr.create_room("room:b", "Beta", None).await;
        let rooms = mgr.list_rooms().await;
        assert_eq!(rooms.len(), 2);
    }

    #[tokio::test]
    async fn events_emitted_on_join_and_leave() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        let mgr = RoomManager::new("mesh:test", bus);

        mgr.create_room("room:a", "A", None).await;
        mgr.join_room("room:a", "agent:alice").await.unwrap();

        let e1 = rx.recv().await.unwrap();
        assert!(matches!(e1, Event::RoomJoined { .. }));
        let e2 = rx.recv().await.unwrap();
        assert!(matches!(e2, Event::PatchReady { .. }));

        mgr.leave_room("room:a", "agent:alice").await.unwrap();

        let e3 = rx.recv().await.unwrap();
        assert!(matches!(e3, Event::RoomLeft { .. }));
        let e4 = rx.recv().await.unwrap();
        assert!(matches!(e4, Event::PatchReady { .. }));
    }
}
