# JSON Schemas

Versioned protocol schemas live here.

## Layout

- `v0.1/envelope.schema.json`: shared envelope constraints.
- `v0.1/messages/*.schema.json`: intent payload schemas.
- `examples/`: sample messages for validator and tests.

## Notes

- Schemas target JSON Schema Draft 2020-12.
- `v0.1` is strict: unknown properties are rejected.
