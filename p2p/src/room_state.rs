use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

use crate::events::EventBus;

// ---------------------------------------------------------------------------
// Vector clock for causal ordering
// ---------------------------------------------------------------------------

/// A vector clock mapping agent IDs to logical timestamps.
///
/// Used to establish causal ordering of state updates across peers.
/// Each peer increments its own entry before broadcasting an update.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorClock {
    pub clocks: HashMap<String, u64>,
}

/// Result of comparing two vector clocks.
#[derive(Debug, PartialEq, Eq)]
pub enum ClockOrdering {
    /// `self` happened before `other` (all entries ≤, at least one <).
    Before,
    /// `self` happened after `other`.
    After,
    /// Neither dominates — the updates are concurrent.
    Concurrent,
    /// Clocks are identical.
    Equal,
}

impl VectorClock {
    /// Increment the logical clock for `agent_id` and return the new value.
    pub fn tick(&mut self, agent_id: &str) -> u64 {
        let entry = self.clocks.entry(agent_id.to_string()).or_insert(0);
        *entry += 1;
        *entry
    }

    /// Get the clock value for a specific agent.
    pub fn get(&self, agent_id: &str) -> u64 {
        self.clocks.get(agent_id).copied().unwrap_or(0)
    }

    /// Merge another vector clock into this one, taking the max of each entry.
    pub fn merge(&mut self, other: &VectorClock) {
        for (agent_id, &value) in &other.clocks {
            let entry = self.clocks.entry(agent_id.clone()).or_insert(0);
            *entry = (*entry).max(value);
        }
    }

    /// Compare this clock with another to determine causal ordering.
    pub fn partial_cmp_clock(&self, other: &VectorClock) -> ClockOrdering {
        let all_keys: std::collections::HashSet<&String> = self
            .clocks
            .keys()
            .chain(other.clocks.keys())
            .collect();

        let mut self_leq = true; // all self[k] <= other[k]
        let mut other_leq = true; // all other[k] <= self[k]

        for key in all_keys {
            let s = self.get(key);
            let o = other.get(key);

            if s > o {
                self_leq = false; // self[k] > other[k], so self is NOT ≤ other
            }
            if o > s {
                other_leq = false; // other[k] > self[k], so other is NOT ≤ self
            }
        }

        match (self_leq, other_leq) {
            (true, true) => ClockOrdering::Equal,
            (true, false) => ClockOrdering::Before,
            (false, true) => ClockOrdering::After,
            (false, false) => ClockOrdering::Concurrent,
        }
    }
}

// ---------------------------------------------------------------------------
// State entries
// ---------------------------------------------------------------------------

/// A single key-value state entry with causal metadata.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateEntry {
    /// The state key (e.g. "game_score", "player_position").
    pub key: String,
    /// JSON-encoded value (opaque to the sync layer).
    pub value: serde_json::Value,
    /// Who last wrote this entry.
    pub author: String,
    /// Vector clock at the time of the write.
    pub clock: VectorClock,
    /// Wall-clock timestamp (used as tiebreaker for concurrent updates).
    pub updated_at: DateTime<Utc>,
}

/// A delta message exchanged between peers for incremental sync.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateDelta {
    pub room_id: String,
    pub from: String,
    pub entries: Vec<StateEntry>,
}

/// A full snapshot of room state (used for initial sync when joining).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub room_id: String,
    /// The aggregate vector clock for the entire room state.
    pub clock: VectorClock,
    pub entries: HashMap<String, StateEntry>,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Room state store
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum StateError {
    #[error("room not found: {0}")]
    RoomNotFound(String),
    #[error("key not found: {0}")]
    KeyNotFound(String),
}

/// Per-room state storage with vector clock tracking.
#[derive(Debug)]
struct RoomState {
    #[allow(dead_code)]
    room_id: String,
    entries: HashMap<String, StateEntry>,
    /// Aggregate vector clock — the merge of all entry clocks.
    clock: VectorClock,
}

impl RoomState {
    fn new(room_id: &str) -> Self {
        Self {
            room_id: room_id.to_string(),
            entries: HashMap::new(),
            clock: VectorClock::default(),
        }
    }

    /// Apply a state entry if it causally dominates or is concurrent-and-newer
    /// than the existing entry for that key.
    ///
    /// Returns `true` if the entry was applied (state changed).
    fn apply_entry(&mut self, entry: StateEntry) -> bool {
        match self.entries.get(&entry.key) {
            None => {
                // New key — always accept.
                self.clock.merge(&entry.clock);
                self.entries.insert(entry.key.clone(), entry);
                true
            }
            Some(existing) => {
                match entry.clock.partial_cmp_clock(&existing.clock) {
                    ClockOrdering::After => {
                        // Incoming causally dominates — accept.
                        self.clock.merge(&entry.clock);
                        self.entries.insert(entry.key.clone(), entry);
                        true
                    }
                    ClockOrdering::Concurrent => {
                        // Concurrent: last-writer-wins using wall-clock tiebreaker,
                        // with agent_id as secondary tiebreaker for determinism.
                        let dominated = entry.updated_at > existing.updated_at
                            || (entry.updated_at == existing.updated_at
                                && entry.author > existing.author);
                        if dominated {
                            self.clock.merge(&entry.clock);
                            self.entries.insert(entry.key.clone(), entry);
                            true
                        } else {
                            // Still merge the clock even if we don't accept the value,
                            // so we can correctly order future updates.
                            self.clock.merge(&entry.clock);
                            false
                        }
                    }
                    ClockOrdering::Before | ClockOrdering::Equal => {
                        // Incoming is stale or identical — ignore the value.
                        false
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// State sync manager
// ---------------------------------------------------------------------------

/// Manages state synchronization for all rooms in a mesh.
///
/// Peers use this to:
/// 1. **Set state** — write a key-value pair, incrementing their vector clock.
/// 2. **Get state** — read the current value of a key.
/// 3. **Build deltas** — produce a set of entries that are newer than a given clock.
/// 4. **Apply deltas** — merge incoming entries from other peers.
/// 5. **Snapshot/restore** — full state transfer for late joiners.
#[derive(Clone)]
pub struct RoomStateSync {
    mesh_id: String,
    rooms: Arc<RwLock<HashMap<String, RoomState>>>,
    bus: EventBus,
}

impl RoomStateSync {
    pub fn new(mesh_id: &str, bus: EventBus) -> Self {
        Self {
            mesh_id: mesh_id.to_string(),
            rooms: Arc::new(RwLock::new(HashMap::new())),
            bus,
        }
    }

    /// Initialize state tracking for a room. Idempotent.
    pub async fn init_room(&self, room_id: &str) {
        let mut rooms = self.rooms.write().await;
        rooms
            .entry(room_id.to_string())
            .or_insert_with(|| RoomState::new(room_id));
    }

    /// Remove state tracking for a room.
    pub async fn remove_room(&self, room_id: &str) -> bool {
        self.rooms.write().await.remove(room_id).is_some()
    }

    /// Set a state key in a room. The agent's vector clock is incremented.
    ///
    /// Returns the new `StateEntry` (suitable for broadcasting as a delta).
    pub async fn set(
        &self,
        room_id: &str,
        agent_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<StateEntry, StateError> {
        let mut rooms = self.rooms.write().await;
        let room = rooms
            .get_mut(room_id)
            .ok_or_else(|| StateError::RoomNotFound(room_id.to_string()))?;

        // Build the new entry's clock: merge room clock, then tick.
        let mut entry_clock = room.clock.clone();
        entry_clock.tick(agent_id);

        let entry = StateEntry {
            key: key.to_string(),
            value,
            author: agent_id.to_string(),
            clock: entry_clock,
            updated_at: Utc::now(),
        };

        room.apply_entry(entry.clone());
        drop(rooms);

        self.bus.emit_patch_ready(
            &self.mesh_id,
            &format!("{} set state key {} in {}", agent_id, key, room_id),
        );

        Ok(entry)
    }

    /// Get the current value of a state key.
    pub async fn get(
        &self,
        room_id: &str,
        key: &str,
    ) -> Result<StateEntry, StateError> {
        let rooms = self.rooms.read().await;
        let room = rooms
            .get(room_id)
            .ok_or_else(|| StateError::RoomNotFound(room_id.to_string()))?;
        room.entries
            .get(key)
            .cloned()
            .ok_or_else(|| StateError::KeyNotFound(key.to_string()))
    }

    /// List all state keys and their entries for a room.
    pub async fn list_entries(
        &self,
        room_id: &str,
    ) -> Result<Vec<StateEntry>, StateError> {
        let rooms = self.rooms.read().await;
        let room = rooms
            .get(room_id)
            .ok_or_else(|| StateError::RoomNotFound(room_id.to_string()))?;
        Ok(room.entries.values().cloned().collect())
    }

    /// Build a delta of entries that are newer than `since_clock`.
    ///
    /// This is used to send incremental updates to a peer. The peer provides
    /// its last known vector clock, and we return entries whose clocks
    /// dominate or are concurrent with the given clock.
    pub async fn build_delta(
        &self,
        room_id: &str,
        agent_id: &str,
        since_clock: &VectorClock,
    ) -> Result<StateDelta, StateError> {
        let rooms = self.rooms.read().await;
        let room = rooms
            .get(room_id)
            .ok_or_else(|| StateError::RoomNotFound(room_id.to_string()))?;

        let entries: Vec<StateEntry> = room
            .entries
            .values()
            .filter(|entry| {
                matches!(
                    entry.clock.partial_cmp_clock(since_clock),
                    ClockOrdering::After | ClockOrdering::Concurrent
                )
            })
            .cloned()
            .collect();

        Ok(StateDelta {
            room_id: room_id.to_string(),
            from: agent_id.to_string(),
            entries,
        })
    }

    /// Apply a delta received from another peer.
    ///
    /// Returns the keys that were actually updated (i.e., the incoming entry
    /// causally dominated or won the concurrent tiebreaker).
    pub async fn apply_delta(&self, delta: StateDelta) -> Result<Vec<String>, StateError> {
        let mut rooms = self.rooms.write().await;
        let room = rooms
            .get_mut(&delta.room_id)
            .ok_or_else(|| StateError::RoomNotFound(delta.room_id.clone()))?;

        let mut updated_keys = Vec::new();
        for entry in delta.entries {
            let key = entry.key.clone();
            if room.apply_entry(entry) {
                updated_keys.push(key);
            }
        }
        drop(rooms);

        if !updated_keys.is_empty() {
            self.bus.emit_patch_ready(
                &self.mesh_id,
                &format!(
                    "state sync from {}: {} keys updated in {}",
                    delta.from,
                    updated_keys.len(),
                    delta.room_id
                ),
            );
        }

        Ok(updated_keys)
    }

    /// Take a full snapshot of the room state (for late joiners).
    pub async fn snapshot(&self, room_id: &str) -> Result<StateSnapshot, StateError> {
        let rooms = self.rooms.read().await;
        let room = rooms
            .get(room_id)
            .ok_or_else(|| StateError::RoomNotFound(room_id.to_string()))?;

        Ok(StateSnapshot {
            room_id: room_id.to_string(),
            clock: room.clock.clone(),
            entries: room.entries.clone(),
            created_at: Utc::now(),
        })
    }

    /// Restore room state from a snapshot (used when joining a room late).
    ///
    /// This merges the snapshot into existing state (if any), so it is safe
    /// to call even if some entries have already been received via deltas.
    pub async fn restore_snapshot(&self, snapshot: StateSnapshot) -> Result<Vec<String>, StateError> {
        let mut rooms = self.rooms.write().await;
        let room = rooms
            .entry(snapshot.room_id.clone())
            .or_insert_with(|| RoomState::new(&snapshot.room_id));

        let mut updated_keys = Vec::new();
        for (_, entry) in snapshot.entries {
            let key = entry.key.clone();
            if room.apply_entry(entry) {
                updated_keys.push(key);
            }
        }
        drop(rooms);

        if !updated_keys.is_empty() {
            self.bus.emit_patch_ready(
                &self.mesh_id,
                &format!(
                    "snapshot restored: {} keys in {}",
                    updated_keys.len(),
                    snapshot.room_id
                ),
            );
        }

        Ok(updated_keys)
    }

    /// Get the current aggregate vector clock for a room.
    pub async fn room_clock(
        &self,
        room_id: &str,
    ) -> Result<VectorClock, StateError> {
        let rooms = self.rooms.read().await;
        let room = rooms
            .get(room_id)
            .ok_or_else(|| StateError::RoomNotFound(room_id.to_string()))?;
        Ok(room.clock.clone())
    }

    /// Count the number of state entries in a room.
    pub async fn entry_count(&self, room_id: &str) -> Result<usize, StateError> {
        let rooms = self.rooms.read().await;
        let room = rooms
            .get(room_id)
            .ok_or_else(|| StateError::RoomNotFound(room_id.to_string()))?;
        Ok(room.entries.len())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Event;
    use serde_json::json;

    // -- Vector clock tests --

    #[test]
    fn clock_tick_and_get() {
        let mut clock = VectorClock::default();
        assert_eq!(clock.get("agent:alice"), 0);
        assert_eq!(clock.tick("agent:alice"), 1);
        assert_eq!(clock.tick("agent:alice"), 2);
        assert_eq!(clock.get("agent:alice"), 2);
    }

    #[test]
    fn clock_merge() {
        let mut a = VectorClock::default();
        a.tick("agent:alice");
        a.tick("agent:alice");

        let mut b = VectorClock::default();
        b.tick("agent:bob");
        b.tick("agent:alice");

        a.merge(&b);
        assert_eq!(a.get("agent:alice"), 2); // max(2, 1)
        assert_eq!(a.get("agent:bob"), 1);
    }

    #[test]
    fn clock_ordering_before() {
        let mut a = VectorClock::default();
        a.tick("agent:alice");

        let mut b = VectorClock::default();
        b.tick("agent:alice");
        b.tick("agent:alice");

        assert_eq!(a.partial_cmp_clock(&b), ClockOrdering::Before);
    }

    #[test]
    fn clock_ordering_after() {
        let mut a = VectorClock::default();
        a.tick("agent:alice");
        a.tick("agent:alice");

        let mut b = VectorClock::default();
        b.tick("agent:alice");

        assert_eq!(a.partial_cmp_clock(&b), ClockOrdering::After);
    }

    #[test]
    fn clock_ordering_concurrent() {
        let mut a = VectorClock::default();
        a.tick("agent:alice");

        let mut b = VectorClock::default();
        b.tick("agent:bob");

        assert_eq!(a.partial_cmp_clock(&b), ClockOrdering::Concurrent);
    }

    #[test]
    fn clock_ordering_equal() {
        let mut a = VectorClock::default();
        a.tick("agent:alice");

        let mut b = VectorClock::default();
        b.tick("agent:alice");

        assert_eq!(a.partial_cmp_clock(&b), ClockOrdering::Equal);
    }

    #[test]
    fn clock_empty_equal() {
        let a = VectorClock::default();
        let b = VectorClock::default();
        assert_eq!(a.partial_cmp_clock(&b), ClockOrdering::Equal);
    }

    // -- Room state sync tests --

    fn make_sync() -> RoomStateSync {
        RoomStateSync::new("mesh:test", EventBus::default())
    }

    #[tokio::test]
    async fn set_and_get() {
        let sync = make_sync();
        sync.init_room("room:lobby").await;

        sync.set("room:lobby", "agent:alice", "score", json!(42))
            .await
            .unwrap();

        let entry = sync.get("room:lobby", "score").await.unwrap();
        assert_eq!(entry.value, json!(42));
        assert_eq!(entry.author, "agent:alice");
        assert_eq!(entry.clock.get("agent:alice"), 1);
    }

    #[tokio::test]
    async fn set_increments_clock() {
        let sync = make_sync();
        sync.init_room("room:lobby").await;

        sync.set("room:lobby", "agent:alice", "a", json!(1))
            .await
            .unwrap();
        sync.set("room:lobby", "agent:alice", "b", json!(2))
            .await
            .unwrap();

        let entry = sync.get("room:lobby", "b").await.unwrap();
        assert_eq!(entry.clock.get("agent:alice"), 2);
    }

    #[tokio::test]
    async fn overwrite_same_key() {
        let sync = make_sync();
        sync.init_room("room:lobby").await;

        sync.set("room:lobby", "agent:alice", "x", json!("old"))
            .await
            .unwrap();
        sync.set("room:lobby", "agent:alice", "x", json!("new"))
            .await
            .unwrap();

        let entry = sync.get("room:lobby", "x").await.unwrap();
        assert_eq!(entry.value, json!("new"));
    }

    #[tokio::test]
    async fn get_nonexistent_key() {
        let sync = make_sync();
        sync.init_room("room:lobby").await;

        let result = sync.get("room:lobby", "missing").await;
        assert!(matches!(result, Err(StateError::KeyNotFound(_))));
    }

    #[tokio::test]
    async fn get_nonexistent_room() {
        let sync = make_sync();
        let result = sync.get("room:ghost", "key").await;
        assert!(matches!(result, Err(StateError::RoomNotFound(_))));
    }

    #[tokio::test]
    async fn delta_sync_between_peers() {
        let sync_a = make_sync();
        let sync_b = make_sync();

        sync_a.init_room("room:game").await;
        sync_b.init_room("room:game").await;

        // Alice sets some state
        sync_a
            .set("room:game", "agent:alice", "score", json!(100))
            .await
            .unwrap();
        sync_a
            .set("room:game", "agent:alice", "level", json!(3))
            .await
            .unwrap();

        // Bob has an empty clock — wants all entries
        let empty_clock = VectorClock::default();
        let delta = sync_a
            .build_delta("room:game", "agent:alice", &empty_clock)
            .await
            .unwrap();
        assert_eq!(delta.entries.len(), 2);

        // Bob applies the delta
        let updated = sync_b.apply_delta(delta).await.unwrap();
        assert_eq!(updated.len(), 2);

        // Bob now has Alice's state
        let entry = sync_b.get("room:game", "score").await.unwrap();
        assert_eq!(entry.value, json!(100));
    }

    #[tokio::test]
    async fn delta_only_sends_newer_entries() {
        let sync = make_sync();
        sync.init_room("room:game").await;

        sync.set("room:game", "agent:alice", "a", json!(1))
            .await
            .unwrap();

        // Capture the clock after "a" is set
        let clock_after_a = sync.room_clock("room:game").await.unwrap();

        sync.set("room:game", "agent:alice", "b", json!(2))
            .await
            .unwrap();

        // Delta since clock_after_a should only include "b"
        let delta = sync
            .build_delta("room:game", "agent:alice", &clock_after_a)
            .await
            .unwrap();
        assert_eq!(delta.entries.len(), 1);
        assert_eq!(delta.entries[0].key, "b");
    }

    #[tokio::test]
    async fn stale_delta_rejected() {
        let sync = make_sync();
        sync.init_room("room:game").await;

        // Set initial value
        sync.set("room:game", "agent:alice", "x", json!("new"))
            .await
            .unwrap();

        // Create a stale entry with an older clock
        let stale_entry = StateEntry {
            key: "x".to_string(),
            value: json!("stale"),
            author: "agent:bob".to_string(),
            clock: VectorClock::default(), // clock {}, which is Before {alice:1}
            updated_at: Utc::now(),
        };

        let delta = StateDelta {
            room_id: "room:game".to_string(),
            from: "agent:bob".to_string(),
            entries: vec![stale_entry],
        };

        let updated = sync.apply_delta(delta).await.unwrap();
        assert!(updated.is_empty());

        // Value unchanged
        let entry = sync.get("room:game", "x").await.unwrap();
        assert_eq!(entry.value, json!("new"));
    }

    #[tokio::test]
    async fn concurrent_updates_last_writer_wins() {
        let sync = make_sync();
        sync.init_room("room:game").await;

        // Two concurrent entries for the same key from different agents
        let mut clock_a = VectorClock::default();
        clock_a.tick("agent:alice");

        let mut clock_b = VectorClock::default();
        clock_b.tick("agent:bob");

        // Alice's write is "earlier" by wall clock
        let entry_a = StateEntry {
            key: "x".to_string(),
            value: json!("alice_value"),
            author: "agent:alice".to_string(),
            clock: clock_a,
            updated_at: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };

        // Bob's write is "later" by wall clock
        let entry_b = StateEntry {
            key: "x".to_string(),
            value: json!("bob_value"),
            author: "agent:bob".to_string(),
            clock: clock_b,
            updated_at: DateTime::parse_from_rfc3339("2026-01-01T00:00:01Z")
                .unwrap()
                .with_timezone(&Utc),
        };

        // Apply Alice first, then Bob (concurrent — Bob wins via later timestamp)
        let delta_a = StateDelta {
            room_id: "room:game".to_string(),
            from: "agent:alice".to_string(),
            entries: vec![entry_a],
        };
        let delta_b = StateDelta {
            room_id: "room:game".to_string(),
            from: "agent:bob".to_string(),
            entries: vec![entry_b],
        };

        sync.apply_delta(delta_a).await.unwrap();
        let updated = sync.apply_delta(delta_b).await.unwrap();
        assert_eq!(updated, vec!["x"]);

        let entry = sync.get("room:game", "x").await.unwrap();
        assert_eq!(entry.value, json!("bob_value"));
    }

    #[tokio::test]
    async fn snapshot_and_restore() {
        let sync_a = make_sync();
        sync_a.init_room("room:game").await;

        sync_a
            .set("room:game", "agent:alice", "hp", json!(100))
            .await
            .unwrap();
        sync_a
            .set("room:game", "agent:alice", "mp", json!(50))
            .await
            .unwrap();

        let snapshot = sync_a.snapshot("room:game").await.unwrap();
        assert_eq!(snapshot.entries.len(), 2);

        // New peer restores from snapshot
        let sync_b = make_sync();
        let updated = sync_b.restore_snapshot(snapshot).await.unwrap();
        assert_eq!(updated.len(), 2);

        let hp = sync_b.get("room:game", "hp").await.unwrap();
        assert_eq!(hp.value, json!(100));
        let mp = sync_b.get("room:game", "mp").await.unwrap();
        assert_eq!(mp.value, json!(50));
    }

    #[tokio::test]
    async fn snapshot_restore_merges_with_existing() {
        let sync = make_sync();
        sync.init_room("room:game").await;

        // Already have a local entry
        sync.set("room:game", "agent:bob", "local_key", json!("local"))
            .await
            .unwrap();

        // Restore a snapshot that has different entries
        let mut entries = HashMap::new();
        let mut clock = VectorClock::default();
        clock.tick("agent:alice");
        entries.insert(
            "remote_key".to_string(),
            StateEntry {
                key: "remote_key".to_string(),
                value: json!("remote"),
                author: "agent:alice".to_string(),
                clock,
                updated_at: Utc::now(),
            },
        );

        let snapshot = StateSnapshot {
            room_id: "room:game".to_string(),
            clock: VectorClock::default(),
            entries,
            created_at: Utc::now(),
        };

        sync.restore_snapshot(snapshot).await.unwrap();

        // Both entries should exist
        assert!(sync.get("room:game", "local_key").await.is_ok());
        assert!(sync.get("room:game", "remote_key").await.is_ok());
    }

    #[tokio::test]
    async fn list_entries() {
        let sync = make_sync();
        sync.init_room("room:game").await;

        sync.set("room:game", "agent:alice", "a", json!(1))
            .await
            .unwrap();
        sync.set("room:game", "agent:bob", "b", json!(2))
            .await
            .unwrap();

        let entries = sync.list_entries("room:game").await.unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn entry_count() {
        let sync = make_sync();
        sync.init_room("room:game").await;

        assert_eq!(sync.entry_count("room:game").await.unwrap(), 0);

        sync.set("room:game", "agent:alice", "a", json!(1))
            .await
            .unwrap();
        assert_eq!(sync.entry_count("room:game").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn remove_room() {
        let sync = make_sync();
        sync.init_room("room:temp").await;
        sync.set("room:temp", "agent:alice", "x", json!(1))
            .await
            .unwrap();

        assert!(sync.remove_room("room:temp").await);
        assert!(!sync.remove_room("room:temp").await);

        assert!(matches!(
            sync.get("room:temp", "x").await,
            Err(StateError::RoomNotFound(_))
        ));
    }

    #[tokio::test]
    async fn init_room_idempotent() {
        let sync = make_sync();
        sync.init_room("room:a").await;
        sync.set("room:a", "agent:alice", "x", json!(1))
            .await
            .unwrap();

        // Second init should NOT wipe existing state
        sync.init_room("room:a").await;
        let entry = sync.get("room:a", "x").await.unwrap();
        assert_eq!(entry.value, json!(1));
    }

    #[tokio::test]
    async fn multi_agent_state() {
        let sync = make_sync();
        sync.init_room("room:game").await;

        // Multiple agents writing to different keys
        sync.set("room:game", "agent:alice", "alice_score", json!(100))
            .await
            .unwrap();
        sync.set("room:game", "agent:bob", "bob_score", json!(200))
            .await
            .unwrap();
        sync.set("room:game", "agent:carol", "carol_score", json!(150))
            .await
            .unwrap();

        assert_eq!(sync.entry_count("room:game").await.unwrap(), 3);

        let clock = sync.room_clock("room:game").await.unwrap();
        assert_eq!(clock.get("agent:alice"), 1);
        assert_eq!(clock.get("agent:bob"), 1); // bob's own counter is 1
        assert_eq!(clock.get("agent:carol"), 1); // carol's own counter is 1
    }

    #[tokio::test]
    async fn events_emitted_on_set() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        let sync = RoomStateSync::new("mesh:test", bus);

        sync.init_room("room:game").await;
        sync.set("room:game", "agent:alice", "x", json!(1))
            .await
            .unwrap();

        let event = rx.recv().await.unwrap();
        assert!(matches!(event, Event::PatchReady { .. }));
    }

    #[tokio::test]
    async fn snapshot_serialization_roundtrip() {
        let sync = make_sync();
        sync.init_room("room:game").await;
        sync.set("room:game", "agent:alice", "k", json!({"nested": true}))
            .await
            .unwrap();

        let snapshot = sync.snapshot("room:game").await.unwrap();
        let json = serde_json::to_string(&snapshot).unwrap();
        let deserialized: StateSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.room_id, "room:game");
        assert_eq!(deserialized.entries.len(), 1);
        assert_eq!(deserialized.entries["k"].value, json!({"nested": true}));
    }

    #[tokio::test]
    async fn delta_serialization_roundtrip() {
        let sync = make_sync();
        sync.init_room("room:game").await;
        sync.set("room:game", "agent:alice", "k", json!(42))
            .await
            .unwrap();

        let delta = sync
            .build_delta("room:game", "agent:alice", &VectorClock::default())
            .await
            .unwrap();
        let json = serde_json::to_string(&delta).unwrap();
        let deserialized: StateDelta = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.room_id, "room:game");
        assert_eq!(deserialized.entries.len(), 1);
        assert_eq!(deserialized.entries[0].value, json!(42));
    }
}
