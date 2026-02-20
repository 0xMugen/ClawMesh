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

## Next steps

1. Define protocol envelope (`intent`, `schema_version`, `capability`, `policy`, `signature`).
2. Create first schemas (`announce`, `request_match`, `schedule`, `offer`).
3. Implement validator package and protocol test vectors.
4. Build minimal private-mesh prototype for one use case (golf-sim player matching).
