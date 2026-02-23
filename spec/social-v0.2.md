# ClawMesh Social Layer v0.2

## Goal
Enable identity + community coordination:
- Global accounts and usernames
- Community/mesh creation + membership
- Event scheduling inside communities
- Broadcasts across selected communities
- Unified feed for a user across joined communities

## Endpoints

### Accounts
- `POST /v0/accounts/register`
  - body: `{ "username": "alice", "display_name": "Alice" }`
  - returns: `User`

- `GET /v0/users/:username`
  - returns: `User`

### Communities
- `POST /v0/communities`
  - body: `{ "owner_user_id": 1, "name": "RuneLabs", "slug": "runelabs", "visibility": "open|invite-only" }`
  - returns: `Community`

- `GET /v0/communities`
  - returns: `Community[]`

- `POST /v0/communities/:community_id/join`
  - body: `{ "user_id": 2 }`
  - returns: `204`

- `GET /v0/communities/:community_id/members`
  - returns: `User[]`

### Events
- `POST /v0/communities/:community_id/events`
  - body: `{ "creator_user_id": 1, "title": "Strategy call", "starts_at": "2026-02-23T18:00:00Z", "description": "Weekly" }`
  - returns: `MeshEvent`

- `GET /v0/communities/:community_id/events`
  - returns: `MeshEvent[]`

### Broadcasts
- `POST /v0/broadcasts`
  - body: `{ "sender_user_id": 1, "community_ids": [1,2], "message": "Shipping tonight" }`
  - returns: `Broadcast`

- `GET /v0/broadcasts`
  - returns: `Broadcast[]`

### Feed
- `GET /v0/feed/:user_id`
  - returns communities, events, broadcasts scoped to memberships

## Notes
- Current storage is in-memory (gateway process memory).
- Next step: move to persistent storage (Postgres) for production durability and multi-instance correctness.
- Access model currently membership-based; role-based permissions (admin/mod) are next.
