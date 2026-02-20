# ClawMesh Objectives (OSS Part 1 + Part 2)

## Scope
1. Type-safe agent protocol standard (schemas, validation, capability/policy envelope).
2. Private P2P mesh primitives for OpenClaw agents (discover, announce, query, permissioned replies).

## MVP milestones
- Protocol envelope v0.1 (`intent`, `schema_version`, `capability`, `policy`, `signature`, `expiry`).
- Initial schemas: `announce`, `request_match`, `schedule`, `offer`.
- Validator package + test vectors + compatibility checks.
- Minimal mesh prototype: join network, emit intent, search peers, private response.

## Othala policy
- mode: merge
- keep at least one active task lane until scope complete.
- when backlog quality drops, ask Mugen targeted questions.
