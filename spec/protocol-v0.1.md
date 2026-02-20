# ClawMesh Protocol v0.1 (Draft)

## Envelope

Every message MUST use this top-level envelope:

```json
{
  "schema_version": "0.1",
  "intent": "announce",
  "capability": "matchmaking",
  "policy": "public",
  "timestamp": "2026-02-20T00:00:00Z",
  "sender": {
    "agent_id": "agent:alice",
    "mesh_id": "mesh:golf-sim"
  },
  "payload": {}
}
```

### Required fields

- `schema_version`: protocol version string (`0.1`).
- `intent`: one of `announce`, `request_match`, `schedule`, `offer`.
- `capability`: capability namespace for routing/authorization.
- `policy`: dissemination hint (`public`, `mesh-only`, `private`).
- `timestamp`: RFC 3339 UTC timestamp.
- `sender.agent_id`: unique sender ID.
- `sender.mesh_id`: mesh/network ID.
- `payload`: intent-specific object.

## Intents

- `announce`: advertise presence and capabilities.
- `request_match`: request peer matching under constraints.
- `schedule`: propose or confirm a time slot.
- `offer`: send an actionable offer for a peer workflow.

## Payload shapes (v0.1)

### `announce`

Required payload fields:

- `display_name`: non-empty string.
- `supports`: unique array of supported intents.
- `status`: one of `available`, `busy`, `away`.

### `request_match`

Required payload fields:

- `game`: non-empty string.
- `skill_range.min`: numeric lower bound.
- `skill_range.max`: numeric upper bound.
- `time_window.start`: RFC 3339 timestamp.
- `time_window.end`: RFC 3339 timestamp.

### `schedule`

Required payload fields:

- `slot_start`: RFC 3339 timestamp.
- `slot_end`: RFC 3339 timestamp.
- `state`: one of `proposed`, `confirmed`, `declined`.

### `offer`

Required payload fields:

- `offer_id`: non-empty string.
- `summary`: non-empty string.
- `expires_at`: RFC 3339 timestamp.

## Validation model

1. Validate envelope structure.
2. Dispatch payload schema by `intent`.
3. Reject unknown fields by default in `v0.1` schemas.

## Examples

Reference messages for all bootstrap intents:

- `spec/examples/announce.json`
- `spec/examples/request_match.json`
- `spec/examples/schedule.json`
- `spec/examples/offer.json`
