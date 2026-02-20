# ClawMesh

Type-safe agent protocol + private mesh networking for OpenClaw.

## Scope

### Part 1 (OSS)
Agent data format standard (schema-enforced, capability-aware, versioned).

### Part 2 (OSS)
Private P2P agent meshes by theme/community (office, golf-sim, project teams).

### Part 3 (Product)
Managed app layer (hosted personal agents, friends/networks UX, subscriptions).

## Dev setup (Nix)

```bash
nix develop
```

## Architecture

### Protocol (v0.1)
- Envelope-based message format with `intent`, `schema_version`, `capability`, `policy`, `signature`.
- Four core intents: `announce`, `request_match`, `schedule`, `offer`.
- JSON Schema 2020-12 validation for all message types.
- Policy enforcement: `public`, `mesh-only`, `private` dissemination control.
- Optional Ed25519 signature verification on envelopes.

### P2P Library (`clawmesh-p2p`)
- **Identity**: Ed25519 keypair generation, signing, and verification.
- **Authentication**: Mutual 4-step challenge-response handshake.
- **Discovery**: In-memory peer registry with gossip-based sync.
- **Peer search**: Query peers by capability, status, or mesh membership.
- **Matchmaking**: Submit match requests, find compatible peers, propose/confirm schedules, send offers.
- **Rooms**: Create/join/leave rooms with membership tracking.
- **State sync**: Per-room key-value state with vector clock causal ordering.
- **Signaling**: WebRTC offer/answer/ICE relay for NAT traversal.
- **Encrypted channels**: AEAD-encrypted bidirectional transport.
- **Federation**: Multi-mesh envelope relay with TTL-based loop prevention.
- **Event bus**: Broadcast-based event system for decoupled components.
- **MeshNode**: Unified entry point wiring all subsystems together.

### Gateway (`gateway`)
- Axum-based HTTP API serving the mesh protocol.
- Envelope routing with policy enforcement and signature verification.
- REST endpoints for peers, rooms, state, matchmaking, schedules, offers, and federation.
- Federation endpoints for cross-mesh link management and envelope relay.
- OpenTelemetry tracing integration.

### Validator (`validator`)
- TypeScript/Node.js package for JSON Schema validation of protocol messages.
- CLI tool for validating message files.
- Test vectors covering all intents and known-invalid cases.

## Next steps

1. Persistence layer for peer registry, rooms, and matchmaking state.
2. WebRTC data channel integration for direct peer communication.
3. Full signature enforcement (reject unsigned envelopes in strict mode).
4. Rate limiting and abuse prevention on gateway endpoints.
