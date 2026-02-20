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

## Validation model

1. Validate envelope structure.
2. Dispatch payload schema by `intent`.
3. Reject unknown fields by default in `v0.1` schemas.
