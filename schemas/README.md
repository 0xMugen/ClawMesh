# JSON Schemas

Versioned protocol schemas live here.

## Layout

- `v0.1/envelope.schema.json`: shared envelope constraints.
- `v0.1/messages/*.schema.json`: intent payload schemas.
- `examples/`: sample messages for validator and tests.
  - `announce.valid.json`
  - `announce.invalid.json`
  - `request_match.valid.json`
  - `schedule.valid.json`
  - `offer.valid.json`

## Notes

- Schemas target JSON Schema Draft 2020-12.
- `v0.1` is strict: unknown properties are rejected.
