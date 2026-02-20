use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::events::EventBus;

#[derive(Debug, Error)]
pub enum MatchError {
    #[error("request not found: {0}")]
    RequestNotFound(String),
    #[error("offer not found: {0}")]
    OfferNotFound(String),
    #[error("schedule not found: {0}")]
    ScheduleNotFound(String),
    #[error("offer expired")]
    OfferExpired,
    #[error("no matching peers found")]
    NoMatchFound,
}

// ---------------------------------------------------------------------------
// Match requests
// ---------------------------------------------------------------------------

/// A request from a peer to find a match under constraints.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchRequest {
    pub request_id: String,
    pub agent_id: String,
    pub mesh_id: String,
    pub game: String,
    pub skill_range: SkillRange,
    pub time_window: TimeWindow,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillRange {
    pub min: f64,
    pub max: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimeWindow {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// A proposed or confirmed schedule slot between peers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScheduleSlot {
    pub schedule_id: String,
    pub agent_id: String,
    pub mesh_id: String,
    pub slot_start: DateTime<Utc>,
    pub slot_end: DateTime<Utc>,
    pub state: SlotState,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotState {
    Proposed,
    Confirmed,
    Declined,
}

/// An actionable offer sent between peers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerOffer {
    pub offer_id: String,
    pub from_agent: String,
    pub mesh_id: String,
    pub summary: String,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// Result of a match attempt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchResult {
    pub request_id: String,
    pub matched_agents: Vec<String>,
    pub game: String,
}

// ---------------------------------------------------------------------------
// Matchmaker
// ---------------------------------------------------------------------------

/// Manages match requests, schedule slots, and offers within a mesh.
#[derive(Clone)]
pub struct Matchmaker {
    mesh_id: String,
    requests: Arc<RwLock<HashMap<String, MatchRequest>>>,
    schedules: Arc<RwLock<HashMap<String, ScheduleSlot>>>,
    offers: Arc<RwLock<HashMap<String, PeerOffer>>>,
    bus: EventBus,
}

impl Matchmaker {
    pub fn new(mesh_id: &str, bus: EventBus) -> Self {
        Self {
            mesh_id: mesh_id.to_string(),
            requests: Arc::new(RwLock::new(HashMap::new())),
            schedules: Arc::new(RwLock::new(HashMap::new())),
            offers: Arc::new(RwLock::new(HashMap::new())),
            bus,
        }
    }

    // -----------------------------------------------------------------------
    // Match requests
    // -----------------------------------------------------------------------

    /// Submit a match request. Returns the generated request ID.
    pub async fn submit_request(
        &self,
        agent_id: &str,
        game: &str,
        skill_range: SkillRange,
        time_window: TimeWindow,
    ) -> MatchRequest {
        let request = MatchRequest {
            request_id: Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            mesh_id: self.mesh_id.clone(),
            game: game.to_string(),
            skill_range,
            time_window,
            created_at: Utc::now(),
        };

        self.requests
            .write()
            .await
            .insert(request.request_id.clone(), request.clone());

        self.bus.emit_patch_ready(
            &self.mesh_id,
            &format!("{} submitted match request for {}", agent_id, game),
        );

        request
    }

    /// Find compatible match requests for a given request.
    ///
    /// Two requests are compatible if:
    /// - They are for the same game
    /// - Their skill ranges overlap
    /// - Their time windows overlap
    /// - They are from different agents
    pub async fn find_matches(&self, request_id: &str) -> Result<Vec<MatchRequest>, MatchError> {
        let requests = self.requests.read().await;
        let target = requests
            .get(request_id)
            .ok_or_else(|| MatchError::RequestNotFound(request_id.to_string()))?
            .clone();
        drop(requests);

        let requests = self.requests.read().await;
        let matches: Vec<MatchRequest> = requests
            .values()
            .filter(|r| {
                r.request_id != target.request_id
                    && r.game == target.game
                    && ranges_overlap(&r.skill_range, &target.skill_range)
                    && windows_overlap(&r.time_window, &target.time_window)
            })
            .cloned()
            .collect();

        Ok(matches)
    }

    /// Remove a match request.
    pub async fn cancel_request(&self, request_id: &str) -> Result<MatchRequest, MatchError> {
        self.requests
            .write()
            .await
            .remove(request_id)
            .ok_or_else(|| MatchError::RequestNotFound(request_id.to_string()))
    }

    /// List all pending match requests.
    pub async fn list_requests(&self) -> Vec<MatchRequest> {
        self.requests.read().await.values().cloned().collect()
    }

    /// List match requests for a specific game.
    pub async fn requests_for_game(&self, game: &str) -> Vec<MatchRequest> {
        self.requests
            .read()
            .await
            .values()
            .filter(|r| r.game == game)
            .cloned()
            .collect()
    }

    // -----------------------------------------------------------------------
    // Schedule slots
    // -----------------------------------------------------------------------

    /// Propose a schedule slot. Returns the generated schedule ID.
    pub async fn propose_slot(
        &self,
        agent_id: &str,
        slot_start: DateTime<Utc>,
        slot_end: DateTime<Utc>,
    ) -> ScheduleSlot {
        let slot = ScheduleSlot {
            schedule_id: Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            mesh_id: self.mesh_id.clone(),
            slot_start,
            slot_end,
            state: SlotState::Proposed,
            created_at: Utc::now(),
        };

        self.schedules
            .write()
            .await
            .insert(slot.schedule_id.clone(), slot.clone());

        self.bus.emit_patch_ready(
            &self.mesh_id,
            &format!("{} proposed schedule slot", agent_id),
        );

        slot
    }

    /// Confirm a proposed schedule slot.
    pub async fn confirm_slot(&self, schedule_id: &str) -> Result<ScheduleSlot, MatchError> {
        let mut schedules = self.schedules.write().await;
        let slot = schedules
            .get_mut(schedule_id)
            .ok_or_else(|| MatchError::ScheduleNotFound(schedule_id.to_string()))?;

        slot.state = SlotState::Confirmed;
        let result = slot.clone();
        drop(schedules);

        self.bus.emit_patch_ready(
            &self.mesh_id,
            &format!("schedule {} confirmed", schedule_id),
        );

        Ok(result)
    }

    /// Decline a proposed schedule slot.
    pub async fn decline_slot(&self, schedule_id: &str) -> Result<ScheduleSlot, MatchError> {
        let mut schedules = self.schedules.write().await;
        let slot = schedules
            .get_mut(schedule_id)
            .ok_or_else(|| MatchError::ScheduleNotFound(schedule_id.to_string()))?;

        slot.state = SlotState::Declined;
        let result = slot.clone();
        drop(schedules);

        self.bus
            .emit_patch_ready(&self.mesh_id, &format!("schedule {} declined", schedule_id));

        Ok(result)
    }

    /// Get a schedule slot by ID.
    pub async fn get_slot(&self, schedule_id: &str) -> Result<ScheduleSlot, MatchError> {
        self.schedules
            .read()
            .await
            .get(schedule_id)
            .cloned()
            .ok_or_else(|| MatchError::ScheduleNotFound(schedule_id.to_string()))
    }

    /// List all schedule slots.
    pub async fn list_schedules(&self) -> Vec<ScheduleSlot> {
        self.schedules.read().await.values().cloned().collect()
    }

    /// List all schedule slots for an agent.
    pub async fn slots_for_agent(&self, agent_id: &str) -> Vec<ScheduleSlot> {
        self.schedules
            .read()
            .await
            .values()
            .filter(|s| s.agent_id == agent_id)
            .cloned()
            .collect()
    }

    // -----------------------------------------------------------------------
    // Offers
    // -----------------------------------------------------------------------

    /// Submit an offer to the mesh.
    pub async fn submit_offer(
        &self,
        from_agent: &str,
        offer_id: &str,
        summary: &str,
        expires_at: DateTime<Utc>,
    ) -> PeerOffer {
        let offer = PeerOffer {
            offer_id: offer_id.to_string(),
            from_agent: from_agent.to_string(),
            mesh_id: self.mesh_id.clone(),
            summary: summary.to_string(),
            expires_at,
            created_at: Utc::now(),
        };

        self.offers
            .write()
            .await
            .insert(offer.offer_id.clone(), offer.clone());

        self.bus.emit_patch_ready(
            &self.mesh_id,
            &format!("{} submitted offer {}", from_agent, offer_id),
        );

        offer
    }

    /// Get an offer by ID. Returns error if expired.
    pub async fn get_offer(&self, offer_id: &str) -> Result<PeerOffer, MatchError> {
        let offer = self
            .offers
            .read()
            .await
            .get(offer_id)
            .cloned()
            .ok_or_else(|| MatchError::OfferNotFound(offer_id.to_string()))?;

        if offer.expires_at < Utc::now() {
            return Err(MatchError::OfferExpired);
        }

        Ok(offer)
    }

    /// List all active (non-expired) offers.
    pub async fn active_offers(&self) -> Vec<PeerOffer> {
        let now = Utc::now();
        self.offers
            .read()
            .await
            .values()
            .filter(|o| o.expires_at >= now)
            .cloned()
            .collect()
    }

    /// Remove an offer.
    pub async fn withdraw_offer(&self, offer_id: &str) -> Result<PeerOffer, MatchError> {
        self.offers
            .write()
            .await
            .remove(offer_id)
            .ok_or_else(|| MatchError::OfferNotFound(offer_id.to_string()))
    }

    /// Purge expired requests and offers.
    pub async fn purge_expired(&self) {
        let now = Utc::now();

        // Purge expired match requests (past time window)
        let mut requests = self.requests.write().await;
        requests.retain(|_, r| r.time_window.end >= now);
        drop(requests);

        // Purge expired offers
        let mut offers = self.offers.write().await;
        offers.retain(|_, o| o.expires_at >= now);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ranges_overlap(a: &SkillRange, b: &SkillRange) -> bool {
    a.min <= b.max && b.min <= a.max
}

fn windows_overlap(a: &TimeWindow, b: &TimeWindow) -> bool {
    a.start < b.end && b.start < a.end
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn make_matchmaker() -> Matchmaker {
        Matchmaker::new("mesh:golf-sim", EventBus::default())
    }

    fn skill(min: f64, max: f64) -> SkillRange {
        SkillRange { min, max }
    }

    fn window_from_now(hours: i64) -> TimeWindow {
        let start = Utc::now();
        let end = start + Duration::hours(hours);
        TimeWindow { start, end }
    }

    #[tokio::test]
    async fn submit_and_find_match() {
        let mm = make_matchmaker();

        let req_a = mm
            .submit_request(
                "agent:alice",
                "golf-18",
                skill(50.0, 80.0),
                window_from_now(2),
            )
            .await;

        let _req_b = mm
            .submit_request(
                "agent:bob",
                "golf-18",
                skill(60.0, 90.0),
                window_from_now(2),
            )
            .await;

        let matches = mm.find_matches(&req_a.request_id).await.unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].agent_id, "agent:bob");
    }

    #[tokio::test]
    async fn no_match_different_game() {
        let mm = make_matchmaker();

        let req_a = mm
            .submit_request(
                "agent:alice",
                "golf-18",
                skill(50.0, 80.0),
                window_from_now(2),
            )
            .await;

        let _req_b = mm
            .submit_request("agent:bob", "golf-9", skill(60.0, 90.0), window_from_now(2))
            .await;

        let matches = mm.find_matches(&req_a.request_id).await.unwrap();
        assert!(matches.is_empty());
    }

    #[tokio::test]
    async fn no_match_non_overlapping_skill() {
        let mm = make_matchmaker();

        let req_a = mm
            .submit_request(
                "agent:alice",
                "golf-18",
                skill(10.0, 30.0),
                window_from_now(2),
            )
            .await;

        let _req_b = mm
            .submit_request(
                "agent:bob",
                "golf-18",
                skill(60.0, 90.0),
                window_from_now(2),
            )
            .await;

        let matches = mm.find_matches(&req_a.request_id).await.unwrap();
        assert!(matches.is_empty());
    }

    #[tokio::test]
    async fn no_match_non_overlapping_time() {
        let mm = make_matchmaker();

        let window_now = window_from_now(1);
        let window_later = TimeWindow {
            start: Utc::now() + Duration::hours(3),
            end: Utc::now() + Duration::hours(5),
        };

        let req_a = mm
            .submit_request("agent:alice", "golf-18", skill(50.0, 80.0), window_now)
            .await;

        let _req_b = mm
            .submit_request("agent:bob", "golf-18", skill(60.0, 90.0), window_later)
            .await;

        let matches = mm.find_matches(&req_a.request_id).await.unwrap();
        assert!(matches.is_empty());
    }

    #[tokio::test]
    async fn cancel_request() {
        let mm = make_matchmaker();

        let req = mm
            .submit_request(
                "agent:alice",
                "golf-18",
                skill(50.0, 80.0),
                window_from_now(2),
            )
            .await;

        assert_eq!(mm.list_requests().await.len(), 1);
        mm.cancel_request(&req.request_id).await.unwrap();
        assert!(mm.list_requests().await.is_empty());
    }

    #[tokio::test]
    async fn requests_for_game() {
        let mm = make_matchmaker();

        mm.submit_request(
            "agent:alice",
            "golf-18",
            skill(50.0, 80.0),
            window_from_now(2),
        )
        .await;
        mm.submit_request("agent:bob", "golf-9", skill(50.0, 80.0), window_from_now(2))
            .await;
        mm.submit_request(
            "agent:carol",
            "golf-18",
            skill(50.0, 80.0),
            window_from_now(2),
        )
        .await;

        let golf18 = mm.requests_for_game("golf-18").await;
        assert_eq!(golf18.len(), 2);

        let golf9 = mm.requests_for_game("golf-9").await;
        assert_eq!(golf9.len(), 1);
    }

    #[tokio::test]
    async fn schedule_lifecycle() {
        let mm = make_matchmaker();
        let start = Utc::now() + Duration::hours(1);
        let end = start + Duration::hours(2);

        let slot = mm.propose_slot("agent:alice", start, end).await;
        assert_eq!(slot.state, SlotState::Proposed);

        let confirmed = mm.confirm_slot(&slot.schedule_id).await.unwrap();
        assert_eq!(confirmed.state, SlotState::Confirmed);
    }

    #[tokio::test]
    async fn decline_slot() {
        let mm = make_matchmaker();
        let start = Utc::now() + Duration::hours(1);
        let end = start + Duration::hours(2);

        let slot = mm.propose_slot("agent:alice", start, end).await;
        let declined = mm.decline_slot(&slot.schedule_id).await.unwrap();
        assert_eq!(declined.state, SlotState::Declined);
    }

    #[tokio::test]
    async fn slots_for_agent() {
        let mm = make_matchmaker();
        let start = Utc::now() + Duration::hours(1);
        let end = start + Duration::hours(2);

        mm.propose_slot("agent:alice", start, end).await;
        mm.propose_slot("agent:alice", end, end + Duration::hours(1))
            .await;
        mm.propose_slot("agent:bob", start, end).await;

        assert_eq!(mm.slots_for_agent("agent:alice").await.len(), 2);
        assert_eq!(mm.slots_for_agent("agent:bob").await.len(), 1);
    }

    #[tokio::test]
    async fn offer_lifecycle() {
        let mm = make_matchmaker();
        let expires = Utc::now() + Duration::hours(24);

        let offer = mm
            .submit_offer("agent:alice", "offer-1", "Join my golf round", expires)
            .await;
        assert_eq!(offer.offer_id, "offer-1");

        let retrieved = mm.get_offer("offer-1").await.unwrap();
        assert_eq!(retrieved.summary, "Join my golf round");

        let active = mm.active_offers().await;
        assert_eq!(active.len(), 1);

        mm.withdraw_offer("offer-1").await.unwrap();
        assert!(mm.active_offers().await.is_empty());
    }

    #[tokio::test]
    async fn expired_offer_rejected() {
        let mm = make_matchmaker();
        let already_expired = Utc::now() - Duration::hours(1);

        mm.submit_offer("agent:alice", "offer-old", "Old offer", already_expired)
            .await;

        let result = mm.get_offer("offer-old").await;
        assert!(matches!(result, Err(MatchError::OfferExpired)));
    }

    #[tokio::test]
    async fn purge_expired() {
        let mm = make_matchmaker();

        // Create a request with an expired time window
        let expired_window = TimeWindow {
            start: Utc::now() - Duration::hours(3),
            end: Utc::now() - Duration::hours(1),
        };
        mm.submit_request("agent:alice", "golf-18", skill(50.0, 80.0), expired_window)
            .await;

        // Create a valid request
        mm.submit_request(
            "agent:bob",
            "golf-18",
            skill(50.0, 80.0),
            window_from_now(2),
        )
        .await;

        // Create expired and valid offers
        mm.submit_offer(
            "agent:alice",
            "old",
            "expired",
            Utc::now() - Duration::hours(1),
        )
        .await;
        mm.submit_offer(
            "agent:bob",
            "new",
            "valid",
            Utc::now() + Duration::hours(24),
        )
        .await;

        mm.purge_expired().await;

        assert_eq!(mm.list_requests().await.len(), 1);
        assert_eq!(mm.active_offers().await.len(), 1);
    }
}
